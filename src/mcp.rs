//! HQ's own MCP server: lets a session see, start and steer the other sessions on the board.
//!
//! A small Streamable HTTP server (MCP over JSON-RPC, POST only) on 127.0.0.1 at a port the OS
//! picks. Every session HQ starts gets a token of its own: Claude through `--mcp-config` (HTTP),
//! Codex through `-c mcp_servers.hq...` and Hermes over ACP's `mcpServers`. Those two start
//! `hq --mcp-stdio` (`run_stdio_bridge`), which reaches the same server with the token it is given.
//!
//! The machine-wide install (`install_everywhere`) registers that same bridge as `hq` in Claude
//! Code's user config, Codex's config.toml and Hermes' config.yaml, so agents started outside HQ
//! get the tools too. Without a token of its own the bridge reads the "outside" token from a
//! discovery file HQ writes while it runs. Inside HQ the per-session entry, also named `hq`,
//! takes its place (Claude prefers `--mcp-config`; Codex merges the overrides into the entry).
//!
//! Like session.rs, this side stays dumb: it checks who is calling, and hands every tool call to the
//! frontend (the `mcp-call` event, answered with the `mcp_reply` command), which owns the cells and
//! so knows what each tool means. Only the tool list lives here, since it is part of the protocol.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

/// What the tools appear under: `mcp__hq__spawn_session` and so on.
pub const SERVER_NAME: &str = "hq";
const TOKEN_VAR: &str = "HQ_MCP_TOKEN";
const URL_VAR: &str = "HQ_MCP_URL";
/// Versions of the protocol this server speaks, newest first. A client asking for another gets the newest.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// The longest a tool call may take (`wait_for_sessions` and `ask_user` can wait this long).
const CALL_LIMIT: Duration = Duration::from_secs(60 * 60);
/// An SSE comment goes out this often while a call runs, so no client times the stream out.
const KEEPALIVE: Duration = Duration::from_secs(15);

const INSTRUCTIONS: &str = "You are running inside HQ, a desktop app where the user watches several agent sessions (Claude Code, Codex, Hermes) side by side, each in its own cell. These tools let you see the other sessions, start new ones, message them, wait for them and coordinate with them. Sessions you start open as cells the user can see and take over. Prefer this over doing everything yourself when work splits cleanly into independent parts, or when a different agent suits a part better. Before editing files that other sessions may touch, claim them with claim_files. Keep the number of sessions you start small: each one costs the user's plan usage.";

/// The port the server listens on, once `start` has run.
static PORT: OnceLock<u16> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();
/// token -> the session (the id `Sessions` hands out) it was issued to, None until it has started.
static TOKENS: Mutex<Option<HashMap<String, Option<u64>>>> = Mutex::new(None);
/// Calls waiting on the frontend: call id -> where its answer goes.
static CALLS: Mutex<Option<HashMap<u64, mpsc::Sender<Value>>>> = Mutex::new(None);
/// Claude's per-session config files, removed when the session ends.
static CONFIG_FILES: Mutex<Option<HashMap<u64, PathBuf>>> = Mutex::new(None);
static NEXT_CALL: AtomicU64 = AtomicU64::new(0);
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
/// The session id calls from outside HQ arrive under (`Sessions` hands out ids from 1).
const OUTSIDE: u64 = 0;
/// Where this copy's discovery file is, once written.
static DISCOVERY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// `~/.hq/mcp.json` (`mcp-<profile>.json` for another profile): the URL and the outside token,
/// readable only by this user, for the bridge an agent outside HQ starts.
fn discovery_file(profile: Option<&str>) -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty())?;
    let name = match profile {
        Some(p) => format!("mcp-{p}.json"),
        None => "mcp.json".into(),
    };
    Some(PathBuf::from(home).join(".hq").join(name))
}

/// Starts the server on a thread of its own. Sessions started before it is up, or when it could not
/// bind, run without HQ's tools.
pub fn start(app: &AppHandle, profile: Option<&str>) {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("HQ MCP server could not start: {e}");
            return;
        }
    };
    let Ok(addr) = listener.local_addr() else { return };
    let _ = APP.set(app.clone());
    let _ = PORT.set(addr.port());
    // The frontend refuses outside calls unless the user installed HQ's tools machine-wide.
    if let (Some(file), Some(url)) = (discovery_file(profile), url()) {
        let token = new_token();
        let body = json!({ "url": url, "token": token, "pid": std::process::id() }).to_string();
        let written = file.parent().map(std::fs::create_dir_all).transpose().and_then(|_| write_private(&file, body.as_bytes()));
        if written.is_ok() {
            TOKENS.lock().unwrap().get_or_insert_with(HashMap::new).insert(token, Some(OUTSIDE));
            *DISCOVERY.lock().unwrap() = Some(file);
        }
    }
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || handle_connection(stream));
        }
    });
}

fn url() -> Option<String> {
    PORT.get().map(|port| format!("http://127.0.0.1:{port}/mcp"))
}

/// A token for a session about to start. Bind it to the session once it has an id; dropped
/// unbound (the start failed), it is revoked.
pub struct Grant {
    token: String,
    url: String,
    bound: bool,
}

impl Grant {
    /// None while the server is not running.
    pub fn issue() -> Option<Grant> {
        let url = url()?;
        let token = new_token();
        TOKENS.lock().unwrap().get_or_insert_with(HashMap::new).insert(token.clone(), None);
        Some(Grant { token, url, bound: false })
    }

    pub fn bind(mut self, id: u64) {
        if let Some(map) = TOKENS.lock().unwrap().as_mut() {
            map.insert(self.token.clone(), Some(id));
        }
        self.bound = true;
    }

    /// Claude: writes a config file naming the server and returns the arguments that load it. A
    /// file rather than JSON on the command line, which other users of the machine could read.
    /// Read-only tools are allowed up front; the rest go through the session's permission prompts.
    pub fn claude_args(&self) -> Result<(Vec<String>, PathBuf), String> {
        // On Linux temp_dir() is the shared /tmp, where another user could pre-create this folder or
        // a file in it; Windows and macOS temp folders are per user, and HQ ships only for those.
        // A Linux build should move this to a 0700 folder of its own and open with create_new.
        let dir = std::env::temp_dir().join("hq-mcp");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join(format!("{}-{}.json", std::process::id(), NEXT_FILE.fetch_add(1, Ordering::Relaxed)));
        let config = json!({ "mcpServers": { SERVER_NAME: {
            "type": "http",
            "url": self.url,
            "headers": { "Authorization": format!("Bearer {}", self.token) },
        } } });
        write_private(&file, config.to_string().as_bytes()).map_err(|e| e.to_string())?;
        let allowed: Vec<String> = TOOLS.iter().filter(|t| t.auto_allow).map(|t| format!("mcp__{SERVER_NAME}__{}", t.name)).collect();
        let args = vec!["--mcp-config".into(), file.to_string_lossy().into_owned(), "--allowedTools".into(), allowed.join(",")];
        Ok((args, file))
    }

    /// Codex: config overrides for `codex app-server` that run the stdio bridge, and the variables
    /// it forwards to it (from the environment, so the token never appears on a command line). The
    /// same shape as the machine-wide entry, which Codex merges these into: a `url` here would clash
    /// with its `command`. Codex gives a tool a minute by default; waiting on sessions takes longer.
    pub fn codex_args(&self) -> (Vec<String>, [(&'static str, String); 2]) {
        let mut args = Vec::new();
        for (key, value) in [
            ("command", toml_string(&bridge_program())),
            ("args", "[\"--mcp-stdio\"]".into()),
            ("env_vars", format!("[\"{URL_VAR}\", \"{TOKEN_VAR}\"]")),
            ("tool_timeout_sec", CALL_LIMIT.as_secs().to_string()),
            ("enabled", "true".into()),
        ] {
            args.push("-c".into());
            args.push(format!("mcp_servers.{SERVER_NAME}.{key}={value}"));
        }
        (args, [(URL_VAR, self.url.clone()), (TOKEN_VAR, self.token.clone())])
    }

    /// Hermes: an ACP `mcpServers` entry. ACP agents must take a command, so it runs this
    /// program's stdio bridge; HTTP entries are optional in ACP.
    pub fn acp_server(&self) -> Value {
        json!({
            "name": SERVER_NAME,
            "command": bridge_program(),
            "args": ["--mcp-stdio"],
            "env": [{ "name": URL_VAR, "value": self.url }, { "name": TOKEN_VAR, "value": self.token }],
        })
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if !self.bound {
            if let Some(map) = TOKENS.lock().unwrap().as_mut() {
                map.remove(&self.token);
            }
        }
    }
}

/// Remembers the config file written for session `id`, so `release` removes it.
pub fn keep_config_file(id: u64, file: PathBuf) {
    CONFIG_FILES.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, file);
}

/// The session has ended: its tokens stop working and its config file goes.
/// A child that exits before its Grant is bound releases first, leaving that token mapped to a dead
/// id and its config file until HQ quits; the id is never reused, so the token reaches nothing.
pub fn release(id: u64) {
    if let Some(map) = TOKENS.lock().unwrap().as_mut() {
        map.retain(|_, v| *v != Some(id));
    }
    if let Some(file) = CONFIG_FILES.lock().unwrap().as_mut().and_then(|m| m.remove(&id)) {
        let _ = std::fs::remove_file(file);
    }
}

/// This program, which agents start with `--mcp-stdio`.
fn bridge_program() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "hq".into())
}

/// A TOML string: a literal one ('...', no escapes, so Windows paths stay as they are) when it can be.
fn toml_string(s: &str) -> String {
    if !s.contains('\'') && !s.contains('\n') {
        format!("'{s}'")
    } else {
        serde_json::to_string(s).unwrap_or_default() // JSON's escapes are valid in a TOML basic string
    }
}

/// HQ is closing: the config files and the discovery file go.
pub fn cleanup() {
    if let Some(file) = DISCOVERY.lock().unwrap().take() {
        let _ = std::fs::remove_file(file);
    }
    if let Some(files) = CONFIG_FILES.lock().unwrap().take() {
        for file in files.into_values() {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// The frontend's answer to a call; the text is what the agent reads.
pub fn reply(id: u64, text: String, is_error: bool) {
    let tx = CALLS.lock().unwrap().as_mut().and_then(|m| m.remove(&id));
    if let Some(tx) = tx {
        let _ = tx.send(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }));
    }
}

/// 256 bits for a session's token. `RandomState` is seeded from the OS's random source, and its
/// keys are never seen outside this process. std does not promise that is a CSPRNG; the token only
/// guards a 127.0.0.1 port, so swap in getrandom if one ever joins the dependency tree.
fn new_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut token = String::with_capacity(64);
    for part in 0..4u32 {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u32(part);
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        token.push_str(&format!("{:016x}", h.finish()));
    }
    token
}

/// Writes a file only this user can read (on Unix; on Windows the temp folder is per user already).
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

// ---------- tools ----------

struct Tool {
    name: &'static str,
    description: &'static str,
    /// Allowed without a permission prompt in Claude sessions: it only reads, or only touches HQ's
    /// own notes, claims and the caller's own cell.
    auto_allow: bool,
    schema: fn() -> Value,
}

fn session_arg() -> Value {
    json!({ "type": "string", "description": "A session id from list_sessions (like \"s3f9a\")." })
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "list_sessions",
        description: "Lists every session open in HQ: its id, agent, folder, title, state (new, idle, working, waiting on the user, stopped), task progress, which session started it, and which one is you.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "spawn_session",
        description: "Starts a new session in HQ and sends it a first prompt. It opens as a cell the user can see. Returns its id; follow it with wait_for_sessions and read_session. Set worktree to give it a git worktree of its own (a new branch next to the repository), so parallel sessions do not edit the same checkout.",
        auto_allow: false,
        schema: || json!({
            "type": "object",
            "properties": {
                "folder": { "type": "string", "description": "Absolute path of the folder it works in. Defaults to your own folder." },
                "prompt": { "type": "string", "description": "The task. It starts with none of your context, so say everything it needs." },
                "agent": { "type": "string", "enum": ["claude", "codex", "hermes"], "description": "Which agent runs it. Defaults to claude." },
                "name": { "type": "string", "description": "A short name shown in its header." },
                "model": { "type": "string", "description": "A model for the agent (for Claude: fable, opus, sonnet, haiku or a full id). Defaults to the user's default." },
                "effort": { "type": "string", "enum": ["low", "medium", "high", "xhigh", "max"] },
                "permission_mode": { "type": "string", "enum": ["default", "acceptEdits", "plan", "auto"], "description": "Claude takes any of these; Codex and Hermes take default (asks before edits) or acceptEdits. Defaults to default, or plan if you are in plan mode (which cannot start Codex or Hermes). Never freer than your own mode, and permissions can never be bypassed." },
                "worktree": { "type": "string", "description": "A branch name: creates a git worktree for it beside the repository of `folder`, and starts the session there." },
                "collapsed": { "type": "boolean", "description": "Open it collapsed in the dock, out of the user's way." }
            },
            "required": ["prompt"]
        }),
    },
    Tool {
        name: "send_message",
        description: "Sends a message to another session, as a new prompt. A working session reads it at its next step. Its answer is not returned here: use wait_for_sessions, then read_session.",
        auto_allow: false,
        schema: || json!({
            "type": "object",
            "properties": { "session": session_arg(), "message": { "type": "string" } },
            "required": ["session", "message"]
        }),
    },
    Tool {
        name: "read_session",
        description: "Reads a session's state, task list and latest replies (full text).",
        auto_allow: true,
        schema: || json!({
            "type": "object",
            "properties": {
                "session": session_arg(),
                "replies": { "type": "integer", "minimum": 1, "maximum": 10, "description": "How many of its latest replies to return. Defaults to 1." }
            },
            "required": ["session"]
        }),
    },
    Tool {
        name: "wait_for_sessions",
        description: "Waits until sessions stop working (finished, stopped, or waiting on the user), then returns their states and latest replies. Use it after spawn_session or send_message instead of polling.",
        auto_allow: true,
        schema: || json!({
            "type": "object",
            "properties": {
                "sessions": { "type": "array", "items": { "type": "string" }, "description": "Session ids." },
                "mode": { "type": "string", "enum": ["all", "any"], "description": "Wait for all of them (default) or for the first one." },
                "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "Gives up after this long and says which are still working. Defaults to 600." }
            },
            "required": ["sessions"]
        }),
    },
    Tool {
        name: "interrupt_session",
        description: "Stops a session's current turn (like the user pressing Esc). Only sessions you started, directly or through others.",
        auto_allow: false,
        schema: || json!({ "type": "object", "properties": { "session": session_arg() }, "required": ["session"] }),
    },
    Tool {
        name: "close_session",
        description: "Closes a session's cell and ends its process. The conversation stays on disk. Only sessions you started, directly or through others.",
        auto_allow: false,
        schema: || json!({ "type": "object", "properties": { "session": session_arg() }, "required": ["session"] }),
    },
    Tool {
        name: "set_session_model",
        description: "Changes a session's model and/or effort. Only sessions you started, directly or through others.",
        auto_allow: false,
        schema: || json!({
            "type": "object",
            "properties": {
                "session": session_arg(),
                "model": { "type": "string" },
                "effort": { "type": "string", "enum": ["low", "medium", "high", "xhigh", "max"] }
            },
            "required": ["session"]
        }),
    },
    Tool {
        name: "collapse_session",
        description: "Collapses a session's cell into the dock, or opens it again. Only sessions you started, directly or through others.",
        auto_allow: true,
        schema: || json!({
            "type": "object",
            "properties": { "session": session_arg(), "collapsed": { "type": "boolean", "description": "Defaults to true." } },
            "required": ["session"]
        }),
    },
    Tool {
        name: "schedule_message",
        description: "Sends a message to a session (yourself by default) later: after a delay, at a time, or once the plan's usage limit has reset. Survives HQ restarts.",
        auto_allow: false,
        schema: || json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" },
                "session": session_arg(),
                "delay_minutes": { "type": "number", "minimum": 1 },
                "at": { "type": "string", "description": "An ISO 8601 time." },
                "after_usage_reset": { "type": "boolean", "description": "When the plan's used-up limit resets." }
            },
            "required": ["message"]
        }),
    },
    Tool {
        name: "list_scheduled",
        description: "Lists messages waiting to be sent by schedule_message.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "cancel_scheduled",
        description: "Cancels a scheduled message by its id.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }),
    },
    Tool {
        name: "notify_user",
        description: "Shows the user a desktop notification from this session. For something they should know now; not for routine progress.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": { "message": { "type": "string" } }, "required": ["message"] }),
    },
    Tool {
        name: "ask_user",
        description: "Asks the user a question in your cell and waits for the answer. Returns at once if the user is away (AFK).",
        auto_allow: true,
        schema: || json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": { "type": "array", "items": { "type": "string" }, "description": "Answers to pick from; the user can always type another." },
                "timeout_seconds": { "type": "integer", "minimum": 10, "maximum": 3600, "description": "Defaults to 1800." }
            },
            "required": ["question"]
        }),
    },
    Tool {
        name: "get_usage",
        description: "The user's Claude plan usage: each limit's percent used and when it resets, and whether the plan is out now. Use it to decide how many sessions to start, or which model.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "claim_files",
        description: "Claims files (or folders) so other sessions know you are editing them. Fails, naming the holder, if another session holds any of them. Claims end when you release them or your session ends. Advisory: nothing stops an edit.",
        auto_allow: true,
        schema: || json!({
            "type": "object",
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Absolute paths, or relative to your folder." },
                "note": { "type": "string", "description": "What you are doing with them." }
            },
            "required": ["paths"]
        }),
    },
    Tool {
        name: "release_files",
        description: "Releases files you claimed. No paths releases all of yours.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": { "paths": { "type": "array", "items": { "type": "string" } } } }),
    },
    Tool {
        name: "list_claims",
        description: "Lists every claimed file, who holds it and why.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "note_set",
        description: "Saves a shared note every session can read: a plan, a decision, an API contract, a task board. Kept across HQ restarts.",
        auto_allow: true,
        schema: || json!({
            "type": "object",
            "properties": { "key": { "type": "string" }, "value": { "type": "string" } },
            "required": ["key", "value"]
        }),
    },
    Tool {
        name: "note_get",
        description: "Reads a shared note, or lists every note's key, author and time when no key is given.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": { "key": { "type": "string" } } }),
    },
    Tool {
        name: "note_delete",
        description: "Deletes a shared note.",
        auto_allow: true,
        schema: || json!({ "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] }),
    },
];

/// The tools that change nothing at all.
const READ_ONLY: [&str; 7] = ["list_sessions", "read_session", "wait_for_sessions", "get_usage", "list_scheduled", "list_claims", "note_get"];

fn tool_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": { "readOnlyHint": READ_ONLY.contains(&t.name) },
            })
        })
        .collect();
    json!({ "tools": tools })
}

// ---------- HTTP ----------

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

fn handle_connection(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let request = match read_request(&mut stream) {
        Ok(r) => r,
        Err(status) => return respond(&mut stream, status, "text/plain", status_text(status).as_bytes()),
    };
    if let Err(status) = check(&request) {
        return respond(&mut stream, status, "text/plain", status_text(status).as_bytes());
    }
    let caller = match caller(&request) {
        Some(c) => c,
        None => return respond(&mut stream, 401, "text/plain", b"Unauthorized"),
    };
    let message: Value = match serde_json::from_slice(&request.body) {
        Ok(v) => v,
        Err(_) => return respond_json(&mut stream, &rpc_error(Value::Null, -32700, "Parse error")),
    };
    let sse = request.header("accept").is_some_and(|a| a.contains("text/event-stream"));
    if let Value::Array(batch) = message {
        // Batches (protocol 2025-03-26) are answered together, without streaming.
        let replies: Vec<Value> = batch.into_iter().filter_map(|m| dispatch(caller, m, &mut |_| true)).collect();
        if replies.is_empty() {
            return respond(&mut stream, 202, "text/plain", b"");
        }
        return respond_json(&mut stream, &Value::Array(replies));
    }
    let is_call = message["method"] == "tools/call" && message.get("id").is_some();
    if is_call && sse {
        // Headers now, the answer when it comes, and comments meanwhile so the stream stays open.
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        if stream.write_all(head.as_bytes()).and_then(|_| stream.flush()).is_err() {
            return;
        }
        let mut alive = |_: ()| stream.try_clone().and_then(|mut s| s.write_all(b": keepalive\n\n").and_then(|_| s.flush())).is_ok();
        if let Some(reply) = dispatch(caller, message, &mut alive) {
            let _ = stream.write_all(format!("event: message\ndata: {reply}\n\n").as_bytes());
            let _ = stream.flush();
        }
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    match dispatch(caller, message, &mut |_| true) {
        Some(reply) => respond_json(&mut stream, &reply),
        None => respond(&mut stream, 202, "text/plain", b""),
    }
}

/// Only `/mcp`, only POST, only from this machine's own clients. A browser page cannot get here:
/// it would have to send an Origin (refused), and a page that rebinds a name to 127.0.0.1 sends
/// that name as its Host (refused).
fn check(r: &Request) -> Result<(), u16> {
    let path = r.path.split('?').next().unwrap_or("");
    if path != "/mcp" {
        return Err(404);
    }
    if r.header("origin").is_some() {
        return Err(403);
    }
    let host = r.header("host").unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    if !matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(403);
    }
    if r.method != "POST" {
        return Err(405);
    }
    Ok(())
}

/// The session the bearer token belongs to.
fn caller(r: &Request) -> Option<u64> {
    let token = r.header("authorization")?.strip_prefix("Bearer ")?.trim();
    TOKENS.lock().unwrap().as_ref()?.get(token).copied().flatten()
}

fn read_request(stream: &mut TcpStream) -> Result<Request, u16> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read_line = |reader: &mut BufReader<&mut TcpStream>, line: &mut String, total: &mut usize| -> Result<(), u16> {
        line.clear();
        let n = reader.read_line(line).map_err(|_| 400u16)?;
        *total += n;
        if n == 0 || *total > MAX_HEADER_BYTES {
            return Err(if n == 0 { 400 } else { 431 });
        }
        Ok(())
    };
    let mut total = 0;
    read_line(&mut reader, &mut line, &mut total)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or(400u16)?.to_owned();
    let path = parts.next().ok_or(400u16)?.to_owned();
    let mut headers = HashMap::new();
    loop {
        read_line(&mut reader, &mut line, &mut total)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let chunked = headers.get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        let len: usize = headers.get("content-length").map(|v| v.parse().map_err(|_| 400u16)).transpose()?.unwrap_or(0);
        if len > MAX_BODY_BYTES {
            return Err(413);
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).map_err(|_| 400u16)?;
        body
    };
    Ok(Request { method, path, headers, body })
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, u16> {
    let mut body = Vec::new();
    loop {
        let mut size = String::new();
        reader.read_line(&mut size).map_err(|_| 400u16)?;
        let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16).map_err(|_| 400u16)?;
        if body.len() + size > MAX_BODY_BYTES {
            return Err(413);
        }
        if size == 0 {
            // Trailers, if any, up to the blank line.
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).map_err(|_| 400u16)? == 0 || l.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).map_err(|_| 400u16)?;
        let mut crlf = [0; 2];
        reader.read_exact(&mut crlf).map_err(|_| 400u16)?;
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Bad Request",
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let allow = if status == 405 { "Allow: POST\r\n" } else { "" };
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{allow}Connection: close\r\n\r\n",
        status_text(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body)).and_then(|_| stream.flush());
    let _ = stream.shutdown(Shutdown::Both);
}

fn respond_json(stream: &mut TcpStream, value: &Value) {
    respond(stream, 200, "application/json", value.to_string().as_bytes());
}

// ---------- JSON-RPC ----------

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answers one message; None for a notification or a response, which get no answer. `alive` is
/// called while a tool call waits, and returns false once the client has gone.
fn dispatch(caller: u64, message: Value, alive: &mut dyn FnMut(()) -> bool) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let Some(method) = message["method"].as_str() else { return None };
    let params = &message["params"];
    Some(match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
            rpc_result(id, json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "title": "HQ", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, tool_list()),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            if !TOOLS.iter().any(|t| t.name == name) {
                return Some(rpc_error(id, -32602, &format!("Unknown tool: {name}")));
            }
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            rpc_result(id, call_frontend(caller, name, args, alive))
        }
        _ => rpc_error(id, -32601, &format!("Method not found: {method}")),
    })
}

fn tool_error(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// Hands a call to the frontend and waits for its answer.
fn call_frontend(caller: u64, tool: &str, args: Value, alive: &mut dyn FnMut(()) -> bool) -> Value {
    let Some(app) = APP.get() else { return tool_error("HQ is not ready.") };
    let id = NEXT_CALL.fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, rx) = mpsc::channel();
    CALLS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, tx);
    let forget = || {
        if let Some(map) = CALLS.lock().unwrap().as_mut() {
            map.remove(&id);
        }
    };
    if app.emit("mcp-call", json!({ "id": id, "proc": caller, "tool": tool, "args": args })).is_err() {
        forget();
        return tool_error("HQ's window is not available.");
    }
    let started = Instant::now();
    loop {
        match rx.recv_timeout(KEEPALIVE) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => return tool_error("HQ dropped the call."),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if started.elapsed() > CALL_LIMIT || !alive(()) {
                    forget();
                    let _ = app.emit("mcp-cancel", json!({ "id": id }));
                    return tool_error("The call took too long, or the client went away.");
                }
            }
        }
    }
}

// ---------- stdio bridge ----------

/// `hq --mcp-stdio [--profile <name>]`: an MCP server on stdio for agents that only start MCP
/// servers as commands. The handshake and the tool list are answered here, so the server comes up
/// even while HQ is closed; tool calls go to HQ's HTTP server. Started by HQ for a session, it is
/// given `HQ_MCP_URL` and `HQ_MCP_TOKEN`; started by an agent outside HQ (the machine-wide
/// install), it reads them from the discovery file each time, so HQ may start or restart meanwhile.
pub fn run_stdio_bridge(profile: Option<String>) -> i32 {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    let mut running = Vec::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let (out, profile) = (out.clone(), profile.clone());
        running.retain(|t: &std::thread::JoinHandle<()>| !t.is_finished());
        // A thread per message: a long tool call must not hold up a ping or a second call.
        running.push(std::thread::spawn(move || {
            let replies = bridge_message(message, profile.as_deref());
            let mut out = out.lock().unwrap();
            for reply in replies {
                let _ = writeln!(out, "{reply}");
            }
            let _ = out.flush();
        }));
    }
    // Stdin closed: answer what is in flight before going.
    for t in running {
        let _ = t.join();
    }
    0
}

fn bridge_message(message: Value, profile: Option<&str>) -> Vec<String> {
    let forward = message.is_array() || message["method"] == "tools/call";
    if !forward {
        return dispatch(OUTSIDE, message, &mut |_| true).map(|r| r.to_string()).into_iter().collect();
    }
    let result = endpoint(profile).and_then(|(addr, token)| {
        post(&addr, &token, message.to_string().as_bytes()).map_err(|e| format!("HQ is not reachable ({e}). Is HQ open?"))
    });
    match result {
        Ok(replies) => replies,
        Err(e) => match message.get("id") {
            Some(id) => vec![rpc_result(id.clone(), tool_error(&e)).to_string()],
            None => Vec::new(),
        },
    }
}

/// Where HQ listens and the token to use: from the environment, else from the discovery file.
fn endpoint(profile: Option<&str>) -> Result<(String, String), String> {
    let (url, token) = match (std::env::var(URL_VAR), std::env::var(TOKEN_VAR)) {
        (Ok(url), Ok(token)) if !url.is_empty() && !token.is_empty() => (url, token),
        _ => {
            let file = discovery_file(profile).ok_or("No home folder to find HQ from.")?;
            let text = std::fs::read_to_string(&file).map_err(|_| "HQ is not running. Open HQ, then try again.".to_string())?;
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
            match (v["url"].as_str(), v["token"].as_str()) {
                (Some(url), Some(token)) => (url.to_owned(), token.to_owned()),
                _ => return Err(format!("{} is not HQ's discovery file.", file.display())),
            }
        }
    };
    let addr = url.strip_prefix("http://").and_then(|rest| rest.split('/').next()).filter(|a| !a.is_empty());
    let addr = addr.ok_or_else(|| format!("{URL_VAR} must be an http:// URL"))?;
    Ok((addr.to_owned(), token))
}

/// POSTs one message and returns the JSON-RPC messages that came back (none for a 202).
fn post(addr: &str, token: &str, body: &[u8]) -> std::io::Result<Vec<String>> {
    let mut stream = TcpStream::connect(addr)?;
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut sse = false;
    let mut len: Option<usize> = None;
    loop {
        let mut l = String::new();
        if reader.read_line(&mut l)? == 0 || l.trim().is_empty() {
            break;
        }
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("content-type:") && lower.contains("text/event-stream") {
            sse = true;
        }
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().ok();
        }
    }
    if code == 202 {
        return Ok(Vec::new());
    }
    if code != 200 {
        return Err(std::io::Error::other(format!("HTTP {code}")));
    }
    if sse {
        let mut replies = Vec::new();
        for l in reader.lines() {
            let l = l?;
            if let Some(data) = l.strip_prefix("data:") {
                replies.push(data.trim().to_owned());
            }
        }
        return Ok(replies);
    }
    let mut body = Vec::new();
    match len {
        Some(n) => {
            body.resize(n, 0);
            reader.read_exact(&mut body)?;
        }
        None => {
            reader.read_to_end(&mut body)?;
        }
    }
    let text = String::from_utf8_lossy(&body).trim().to_owned();
    Ok(if text.is_empty() { Vec::new() } else { vec![text] })
}

// ---------- worktrees ----------

/// Creates a git worktree on branch `branch` for the repository holding `folder`, at
/// `<repo>.worktrees/<branch>` beside it, and returns its path. An existing branch is checked out;
/// a new one starts from the repository's current HEAD. An existing worktree there is reused.
pub fn create_worktree(folder: &str, branch: &str) -> Result<String, String> {
    let git = |dir: &str, args: &[&str]| -> Result<String, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null());
        crate::session::hide_console(&mut cmd);
        let out = cmd.output().map_err(|e| format!("Could not run git ({e}). Is Git installed?"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    };
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') {
        return Err("A worktree needs a branch name.".into());
    }
    git(folder, &["check-ref-format", "--branch", branch]).map_err(|_| format!("\"{branch}\" is not a valid branch name."))?;
    let top = git(folder, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{folder} is not in a git repository."))?;
    let top_path = PathBuf::from(&top);
    let repo_name = top_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let parent = top_path.parent().ok_or("The repository has no parent folder for its worktrees.")?;
    let leaf: String = branch.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let path = parent.join(format!("{repo_name}.worktrees")).join(leaf);
    let path_str = path.to_string_lossy().into_owned();
    if path.join(".git").exists() {
        return Ok(path_str);
    }
    let exists = git(&top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    let args: Vec<&str> = if exists { vec!["worktree", "add", &path_str, branch] } else { vec!["worktree", "add", "-b", branch, &path_str] };
    git(&top, &args).map_err(|e| format!("git worktree add failed: {e}"))?;
    Ok(path_str)
}

// ---------- machine-wide install ----------

/// How installing (or removing) HQ's tools went for one agent.
#[derive(serde::Serialize, Debug)]
pub struct InstallResult {
    agent: &'static str,
    /// done, skipped (the agent is not installed) or failed
    status: &'static str,
    detail: String,
}

fn result(agent: &'static str, outcome: Result<String, String>) -> InstallResult {
    match outcome {
        Ok(detail) if detail.starts_with("skipped: ") => InstallResult { agent, status: "skipped", detail: detail["skipped: ".len()..].to_owned() },
        Ok(detail) => InstallResult { agent, status: "done", detail },
        Err(detail) => InstallResult { agent, status: "failed", detail },
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// The arguments an agent starts the bridge with. A profile other than the default reaches its own HQ.
fn bridge_args(profile: Option<&str>) -> Vec<String> {
    let mut args = vec!["--mcp-stdio".to_owned()];
    if let Some(p) = profile {
        args.extend(["--profile".to_owned(), p.to_owned()]);
    }
    args
}

/// Registers HQ's tools as `hq` for every Claude Code, Codex and Hermes on this machine (`install`),
/// or removes them. Running agents pick the change up when they next start.
pub fn install_everywhere(profile: Option<&str>, install: bool) -> Vec<InstallResult> {
    vec![
        result("Claude Code", claude_install(profile, install)),
        result("Codex", codex_install(profile, install)),
        result("Hermes", hermes_install(profile, install)),
    ]
}

/// Through `claude mcp`, at user scope: Claude keeps that config in a file its sessions also write.
fn claude_install(profile: Option<&str>, install: bool) -> Result<String, String> {
    let run = |args: &[String]| -> Result<String, String> {
        let mut cmd = std::process::Command::new(crate::session::claude_program());
        cmd.args(args).env_remove("CLAUDECODE").stdin(std::process::Stdio::null());
        crate::session::hide_console(&mut cmd);
        let out = cmd.output().map_err(|e| format!("Could not run claude ({e})"))?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_owned();
        if out.status.success() { Ok(text) } else { Err(text) }
    };
    let remove: Vec<String> = ["mcp", "remove", SERVER_NAME, "-s", "user"].map(String::from).to_vec();
    let removed = run(&remove);
    if !install {
        return Ok(if removed.is_ok() { "Removed from your user config.".into() } else { "It was not installed.".into() });
    }
    let mut add: Vec<String> = ["mcp", "add", "-s", "user", SERVER_NAME, "--"].map(String::from).to_vec();
    add.push(bridge_program());
    add.extend(bridge_args(profile));
    run(&add).map(|_| "Added to your user config (~/.claude.json).".into())
}

fn codex_config() -> Option<PathBuf> {
    let dir = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".codex")))?;
    Some(dir.join("config.toml"))
}

/// Codex's config.toml, edited as text: every `[mcp_servers.hq]` table (and its sub-tables) is
/// replaced by HQ's. Codex has no command that sets the tool timeout.
fn codex_install(profile: Option<&str>, install: bool) -> Result<String, String> {
    let file = codex_config().ok_or("No home folder.")?;
    let dir = file.parent().ok_or("No Codex folder.")?;
    if !dir.is_dir() {
        return Ok("skipped: Codex has not been set up on this machine (no ~/.codex).".into());
    }
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    let block = install.then(|| codex_block(profile));
    let new = codex_edit(&old, block.as_deref());
    if new == old {
        return Ok(if install { "Already installed.".into() } else { "It was not installed.".into() });
    }
    if !old.is_empty() {
        std::fs::write(file.with_extension("toml.hq-backup"), &old).map_err(|e| e.to_string())?;
    }
    std::fs::write(&file, new).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(format!("{} {}.", if install { "Added to" } else { "Removed from" }, file.display()))
}

fn codex_block(profile: Option<&str>) -> String {
    let args: Vec<String> = bridge_args(profile).iter().map(|a| toml_string(a)).collect();
    format!(
        "[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [{}]\ntool_timeout_sec = {}\n",
        toml_string(&bridge_program()),
        args.join(", "),
        CALL_LIMIT.as_secs()
    )
}

/// `config` without its `[mcp_servers.hq]` tables, and with `block` at the end when given.
/// A line-based edit: an hq server written as dotted keys, inline, or under a quoted header is not
/// seen and gets a duplicate, which Codex then refuses. HQ writes only the plain table form.
fn codex_edit(config: &str, block: Option<&str>) -> String {
    let own = |header: &str| {
        let name = header.trim().trim_start_matches('[').trim_end_matches(']').trim();
        name == format!("mcp_servers.{SERVER_NAME}") || name.starts_with(&format!("mcp_servers.{SERVER_NAME}."))
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in config.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            skipping = own(t.split('#').next().unwrap_or(t));
        }
        if !skipping {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if let Some(block) = block {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(block);
    } else if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn hermes_config() -> Option<PathBuf> {
    let dir = std::env::var_os("HERMES_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".hermes")))?;
    Some(dir.join("config.yaml"))
}

/// Hermes' config.yaml, edited as text: an `hq:` entry under the top-level `mcp_servers:` map.
fn hermes_install(profile: Option<&str>, install: bool) -> Result<String, String> {
    let file = hermes_config().ok_or("No home folder.")?;
    if !file.parent().is_some_and(|d| d.is_dir()) {
        return Ok("skipped: Hermes has not been set up on this machine (no ~/.hermes).".into());
    }
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    let new = hermes_edit(&old, install.then_some(profile))?;
    if new == old {
        return Ok(if install { "Already installed.".into() } else { "It was not installed.".into() });
    }
    if !old.is_empty() {
        std::fs::write(file.with_extension("yaml.hq-backup"), &old).map_err(|e| e.to_string())?;
    }
    std::fs::write(&file, new).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(format!("{} {}.", if install { "Added to" } else { "Removed from" }, file.display()))
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// `config` with HQ's entry in `mcp_servers:` (`Some(profile)`) or without it (`None`). Only the
/// block style Hermes writes is understood; anything else is left to the user, with the snippet.
fn hermes_edit(config: &str, install: Option<Option<&str>>) -> Result<String, String> {
    let mut lines: Vec<String> = config.lines().map(str::to_owned).collect();
    let header = lines.iter().position(|l| {
        let t = l.split('#').next().unwrap_or("").trim_end();
        indent_of(l) == 0 && (t == "mcp_servers:" || t.starts_with("mcp_servers:"))
    });
    let entry = |indent: usize, profile: Option<&str>| -> Vec<String> {
        let pad = " ".repeat(indent);
        let args: Vec<String> = bridge_args(profile).iter().map(|a| serde_json::to_string(a).unwrap_or_default()).collect();
        vec![
            format!("{pad}{SERVER_NAME}:"),
            format!("{pad}  command: {}", serde_json::to_string(&bridge_program()).unwrap_or_default()),
            format!("{pad}  args: [{}]", args.join(", ")),
            format!("{pad}  timeout: {}", CALL_LIMIT.as_secs()),
        ]
    };
    let Some(h) = header else {
        let Some(profile) = install else { return Ok(config.to_owned()) };
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("mcp_servers:".into());
        lines.extend(entry(2, profile));
        return Ok(lines.join("\n") + "\n");
    };
    let inline = lines[h].split('#').next().unwrap_or("").trim_end().trim_start_matches("mcp_servers:").trim().to_owned();
    if !inline.is_empty() && inline != "{}" {
        return Err("config.yaml lists mcp_servers in a style HQ does not edit. Add an `hq` entry that runs HQ with --mcp-stdio by hand.".into());
    }
    if inline == "{}" {
        lines[h] = "mcp_servers:".into();
    }
    // The map's children: the lines after the header indented deeper than it (blank lines included).
    let end = (h + 1..lines.len()).find(|&i| !lines[i].trim().is_empty() && indent_of(&lines[i]) == 0).unwrap_or(lines.len());
    let child = (h + 1..end).find(|&i| !lines[i].trim().is_empty() && !lines[i].trim_start().starts_with('#')).map(|i| indent_of(&lines[i])).unwrap_or(2);
    // Our entry: from `hq:` at the child indent up to the next line at that indent or less.
    let ours = (h + 1..end).find(|&i| indent_of(&lines[i]) == child && lines[i].trim_start().split('#').next().unwrap_or("").trim_end() == format!("{SERVER_NAME}:"));
    if let Some(start) = ours {
        let stop = (start + 1..end).find(|&i| !lines[i].trim().is_empty() && indent_of(&lines[i]) <= child).unwrap_or(end);
        lines.drain(start..stop);
    }
    if let Some(profile) = install {
        let at = h + 1;
        for (k, l) in entry(child, profile).into_iter().enumerate() {
            lines.insert(at + k, l);
        }
    }
    let trailing = config.ends_with('\n') || config.is_empty();
    Ok(lines.join("\n") + if trailing { "\n" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn only_local_posts_to_mcp_get_through() {
        assert_eq!(check(&request("POST", "/mcp", &[("host", "127.0.0.1:5000")])), Ok(()));
        assert_eq!(check(&request("POST", "/mcp?x=1", &[("host", "localhost:5000")])), Ok(()));
        assert_eq!(check(&request("GET", "/mcp", &[("host", "127.0.0.1:5000")])), Err(405));
        assert_eq!(check(&request("POST", "/other", &[("host", "127.0.0.1:5000")])), Err(404));
        assert_eq!(check(&request("POST", "/mcp", &[("host", "evil.example:5000")])), Err(403));
        assert_eq!(check(&request("POST", "/mcp", &[("host", "127.0.0.1:5000"), ("origin", "https://evil.example")])), Err(403));
    }

    #[test]
    fn tokens_are_long_and_unique() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn every_tool_has_an_object_schema_and_a_unique_name() {
        let list = tool_list();
        let tools = list["tools"].as_array().unwrap();
        let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(tools.len(), TOOLS.len());
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), TOOLS.len());
    }

    #[test]
    fn answers_the_handshake_and_refuses_unknown_methods() {
        let init = dispatch(1, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }), &mut |_| true).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "hq");
        let newer = dispatch(1, json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "2099-01-01" } }), &mut |_| true).unwrap();
        assert_eq!(newer["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(dispatch(1, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), &mut |_| true).is_none());
        let unknown = dispatch(1, json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }), &mut |_| true).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let bad_tool = dispatch(1, json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "rm_rf" } }), &mut |_| true).unwrap();
        assert_eq!(bad_tool["error"]["code"], -32602);
    }

    #[test]
    fn codex_config_gets_one_hq_table() {
        let old = "model = \"o3\"\n\n[mcp_servers.hq]\nurl = \"x\"\n\n[mcp_servers.hq.env]\nA = \"1\"\n\n[mcp_servers.other]\ncommand = \"y\"\n";
        let new = codex_edit(old, Some("[mcp_servers.hq]\ncommand = 'hq'\n"));
        assert_eq!(new, "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"y\"\n\n[mcp_servers.hq]\ncommand = 'hq'\n");
        assert_eq!(codex_edit(&new, None), "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"y\"\n");
        assert_eq!(codex_edit("", Some("[mcp_servers.hq]\n")), "[mcp_servers.hq]\n");
    }

    #[test]
    fn hermes_config_gets_one_hq_entry() {
        let old = "model: x\nmcp_servers:\n    github:\n        command: npx\n    hq:\n        command: old\n        args: []\ntoolsets: [a]\n";
        let new = hermes_edit(old, Some(None)).unwrap();
        assert!(new.starts_with("model: x\nmcp_servers:\n    hq:\n      command: "), "{new}");
        assert!(new.contains("    github:\n        command: npx\ntoolsets: [a]\n"), "{new}");
        assert_eq!(new.matches("hq:").count(), 1);
        assert_eq!(hermes_edit(&new, None).unwrap(), "model: x\nmcp_servers:\n    github:\n        command: npx\ntoolsets: [a]\n");
        let fresh = hermes_edit("model: x\n", Some(Some("dev"))).unwrap();
        assert!(fresh.starts_with("model: x\n\nmcp_servers:\n  hq:\n    command: "), "{fresh}");
        assert!(fresh.contains("args: [\"--mcp-stdio\", \"--profile\", \"dev\"]"), "{fresh}");
        assert!(hermes_edit("mcp_servers: {a: 1}\n", Some(None)).is_err());
        assert_eq!(hermes_edit("mcp_servers: {}\n", None).unwrap(), "mcp_servers:\n");
    }

    #[test]
    fn toml_strings_keep_windows_paths() {
        assert_eq!(toml_string(r"C:\Program Files\HQ\hq.exe"), r"'C:\Program Files\HQ\hq.exe'");
        assert_eq!(toml_string("it's"), "\"it's\"");
    }

    #[test]
    fn the_bridge_answers_the_handshake_without_hq() {
        let replies = bridge_message(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), Some("no-such-profile-for-tests"));
        let v: Value = serde_json::from_str(&replies[0]).unwrap();
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), TOOLS.len());
    }

    #[test]
    fn reads_chunked_bodies() {
        let mut input: &[u8] = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        assert_eq!(read_chunked(&mut input).unwrap(), b"hello world");
    }

    #[test]
    fn an_unbound_grant_is_revoked_when_dropped() {
        let _ = PORT.set(1);
        let grant = Grant::issue().unwrap();
        let token = grant.token.clone();
        assert!(TOKENS.lock().unwrap().as_ref().unwrap().contains_key(&token));
        drop(grant);
        assert!(!TOKENS.lock().unwrap().as_ref().unwrap().contains_key(&token));
        let grant = Grant::issue().unwrap();
        let token = grant.token.clone();
        grant.bind(77);
        assert_eq!(TOKENS.lock().unwrap().as_ref().unwrap().get(&token), Some(&Some(77)));
        release(77);
        assert!(!TOKENS.lock().unwrap().as_ref().unwrap().contains_key(&token));
    }
}

//! Hermes Agent cells: one `hermes acp` child process per cell, spoken to over ACP (JSON-RPC 2.0
//! on stdio) and translated to and from the stream-json lines the frontend already speaks.
//!
//! Unlike the claude path this backend has to interpret messages, so all of that lives here: the
//! frontend sees Claude-shaped lines, and session.rs sees one more child process whose stdin and
//! stdout are this translator's. Deleting this file, its `mod` line, the `hermes` feature and
//! `ui/hermes.js` (with the hooks that call it) removes Hermes support entirely.
//!
//! Hermes keeps its own sessions, memory and skills in its own state folder; HQ only adds a
//! Claude-shaped copy of each conversation under its own config folder, so a restored cell can
//! draw its history without starting Hermes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};

use crate::session::{hide_console, Sessions, SpawnOptions};
use crate::store::{self, TranscriptChunk};

/// ACP version spoken; Hermes 0.21 answers 1.
const PROTOCOL_VERSION: u64 = 1;

/// Starts every note HQ adds to a prompt (this one, and the composer's task-list nudge), so a copy
/// rebuilt from Hermes' history can cut them off.
const TASK_HINT_PREFIX: &str = "(HQ: track this work";
/// The composer's one-off nudge for a Hermes cell (`HERMES_TASK_HINT` in ui/app.js).
const COMPOSER_TASK_HINT: &str = "(HQ: track this work with your todo tool so the user can follow progress.)";
/// What Claude gets as an appended system prompt. ACP has no system prompt, so it rides on the
/// first prompt a process sends instead; Hermes keeps it in the session's history from there.
const TASK_PROMPT: &str = "(HQ: track this work with your todo tool when it has three or more steps or will take more than a few minutes: the user watches several sessions at once in HQ and judges progress from your todo list. Write the list before starting and keep it current, marking each item in_progress as you start it and completed as soon as it is done. Skip the list for questions and quick single-step jobs. This applies to the rest of the session.)";

/// The `hermes` executable: `HQ_HERMES` when set, otherwise whatever is on PATH. Windows installs
/// of Hermes 0.22 ship only `hermes.cmd`, which `Command` never finds by bare name (it only
/// tries `.exe`), so PATH is searched here.
pub(crate) fn hermes_program() -> PathBuf {
    if let Some(p) = std::env::var_os("HQ_HERMES").filter(|p| !p.is_empty()) {
        return p.into();
    }
    #[cfg(windows)]
    if let Some(p) = find_on_path() {
        return p;
    }
    "hermes".into()
}

/// The `hermes` executable on the PATH children are started with, if there is one.
fn find_on_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let path = Some(crate::macos::path());
    #[cfg(not(target_os = "macos"))]
    let path = std::env::var_os("PATH");
    let names: &[&str] = if cfg!(windows) { &["hermes.exe", "hermes.cmd", "hermes.bat"] } else { &["hermes"] };
    std::env::split_paths(&path?).flat_map(|dir| names.iter().map(move |n| dir.join(n))).find(|p| p.is_file())
}

/// Whether Hermes Agent is installed, so the UI offers it. Read once, when the UI loads: a Hermes
/// installed later shows after HQ restarts (on Windows, it also needs the PATH HQ started with).
pub fn installed() -> bool {
    match std::env::var_os("HQ_HERMES").filter(|p| !p.is_empty()) {
        Some(p) => Path::new(&p).is_file(),
        None => find_on_path().is_some(),
    }
}

/// Where HQ keeps its display copy of Hermes conversations, next to board.json.
fn mirror_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(store::state_path(app)?.with_file_name("hermes"))
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Starts `hermes acp` for a cell. Returns the same process id `Sessions::spawn` does, and the
/// frontend talks to it the same way.
pub fn spawn(app: &AppHandle, opts: SpawnOptions, events: Channel<InvokeResponseBody>) -> Result<u64, String> {
    if !Path::new(&opts.cwd).is_dir() {
        return Err(format!("Folder does not exist: {}", opts.cwd));
    }
    let mirror = mirror_dir(app)?;
    let mut cmd = Command::new(hermes_program());
    cmd.arg("acp")
        .current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    let failed = |e: &dyn std::fmt::Display| format!("Could not start `{} acp` ({e}). Is Hermes Agent installed and on PATH?", hermes_program().display());
    // HQ's MCP server, through the stdio bridge: ACP agents must take a command, HTTP is optional.
    let grant = if opts.mcp { crate::mcp::Grant::issue() } else { None };
    let mcp_servers = Value::Array(grant.iter().map(|g| g.acp_server()).collect());
    // A session host runs it, translator and all, so it outlives HQ (host.rs).
    if let Some(conn) = app.state::<crate::hostlink::Link>().primary(app) {
        let meta = json!({
            "cellKey": opts.cell_key,
            "backend": "hermes",
            "mcpToken": grant.as_ref().map(|g| g.token().to_owned()),
            "mcpPort": crate::mcp::port(),
        });
        let mut msg = crate::hostlink::spawn_message("hermes", &cmd, meta);
        msg["hermes"] = json!({ "opts": opts, "mirror": mirror.to_string_lossy(), "mcpServers": mcp_servers });
        match app.state::<Sessions>().spawn_hosted(&conn, msg, events.clone()) {
            Ok(id) => {
                if let Some(grant) = grant {
                    grant.bind(id);
                }
                // It may have ended already: its token goes now.
                if !app.state::<Sessions>().tracks(id) {
                    crate::mcp::release(id);
                }
                return Ok(id);
            }
            Err(crate::session::HostError::Spawn(e)) => return Err(failed(&e)),
            Err(crate::session::HostError::Link) => {} // run it here instead
        }
    }
    let mut child = cmd.spawn().map_err(|e| failed(&e))?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        crate::session::kill_group(child.id());
        let _ = child.kill();
        return Err("hermes pipes unavailable".into());
    };
    match translate(stdin, stdout, mirror, opts, mcp_servers) {
        Ok((input, lines)) => {
            let id = app.state::<Sessions>().register(app, child, Some(input), lines, stderr, events);
            if let Some(grant) = grant {
                grant.bind(id);
            }
            Ok(id)
        }
        Err(e) => {
            crate::session::kill_group(child.id());
            let _ = child.kill();
            Err(e)
        }
    }
}

/// How long `hermes sessions discard` may take; it waits on Hermes' gateway.
const DISCARD_TIMEOUT: Duration = Duration::from_secs(60);

/// How long Hermes may stay silent before the cell's session is open (and its model and mode
/// set) before HQ gives up on it. Longer than `DISCARD_TIMEOUT`, which can fall inside the wait.
/// Without it a `session/new` Hermes never answers leaves the cell "working" for good, every
/// message held in `early`.
/// Hermes starts in seconds and a load streams its replay as it goes, so two minutes of
/// silence means it is stuck, not slow.
const OPEN_QUIET: Duration = Duration::from_secs(120);

/// Leads a side question asked in a fresh session, which has none of the conversation.
const SIDE_FRESH_PREFACE: &str = "(HQ: you are answering a side question about another Hermes session that is busy with a turn. You cannot see its conversation; answer only from what follows, and do not use tools or carry on its work.)";

/// How long a turn Hermes started itself must go without news, with no tool or question open,
/// before HQ takes it as over: Hermes reports no end for a turn HQ did not send.
const OWN_TURN_QUIET: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(30) };

/// How long a turn HQ sent may go without a line from Hermes before HQ writes it a harmless
/// request (`session/list`, which Hermes answers from its own catalog). Hermes' ACP adapter has
/// been seen holding a whole turn's updates for hours (its gateway link up, the turn long done)
/// and handing them all over the moment HQ next wrote to it; a line now and then keeps that from
/// waiting on the user. A long tool is silent too, so this only writes, never ends the turn.
const TURN_NUDGE: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(60) };

/// How long a held model or mode change waits before HQ asks again, after Hermes said it was busy
/// while HQ saw no turn (one Hermes started itself, not yet reported), and how many times it asks.
const HELD_RETRY: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(10) };
const HELD_TRIES: u32 = 3;
/// How long a sent held change may go unanswered before HQ gives up on it, so a lost answer does
/// not keep the queued prompts waiting for good.
const HELD_ANSWER: Duration = Duration::from_secs(60);

/// Stop's answer during a turn Hermes started itself. Hermes' gateway cancels only the prompts
/// this connection sent (`acp_adapter/gateway_server.py`, `cancel`) and refuses `/stop`, and the
/// turn runs in the gateway, so ending `hermes acp` (Force stop) leaves it running too.
const OWN_TURN_STOP: &str = "Hermes started this turn itself, to act on a background task or watch notice, and Hermes lets HQ stop only the turns HQ sent. It runs on in Hermes, and this cell shows it until it ends; denying its next approval ends it sooner.";

/// A turn's end when Hermes accepted its prompt and then lost it in a gateway restart.
const LOST_ACCEPTED_TURN: &str = "Hermes lost this turn when its gateway restarted, after it had started on your message, so it may have run in part. HQ cleared the lost turn; check what was done and send again to continue.";

/// Why a session will not open while Hermes holds a turn it lost, and what clears it by hand.
fn lost_turn_message(session: &str, why: &str) -> String {
    format!(
        "Hermes could not start the session: a turn it lost when its gateway restarted blocks it, and HQ could not clear it ({why}). Run `hermes sessions discard {session} --yes` in a terminal, then reopen it."
    )
}

/// Runs `hermes sessions discard`, which acknowledges the turn a session lost when Hermes' gateway
/// restarted without replaying it. Err says why Hermes did not accept. Run off the bridge thread.
/// On a timeout only the launcher is killed (`kill_group` does nothing on Windows), which is
/// acceptable for a rare, bounded wait.
fn discard_lost_turn(session: &str, cwd: &str) -> Result<(), String> {
    let mut cmd = Command::new(hermes_program());
    cmd.args(["sessions", "discard", "--yes", "--", session])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("could not run hermes: {e}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            let _ = tx.send(text);
        });
    }
    let deadline = Instant::now() + DISCARD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let text = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                let last = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("no message");
                return Err(format!("exit {}: {}", status.code().map_or("?".into(), |c| c.to_string()), last.chars().take(200).collect::<String>()));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                crate::session::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Starts the translator between Hermes' pipes and the frontend. Returns where the frontend's
/// lines go in, and the pipe its Claude-shaped lines come out of.
pub(crate) fn translate(stdin: ChildStdin, stdout: impl Read + Send + 'static, mirror: PathBuf, opts: SpawnOptions, mcp_servers: Value) -> Result<(mpsc::Sender<String>, PipeReader), String> {
    let (lines, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let (events_tx, events_rx) = mpsc::channel::<Event>();
    let bridge_tx = events_tx.clone();
    let (input_tx, input_rx) = mpsc::channel::<String>();
    {
        let tx = events_tx.clone();
        std::thread::spawn(move || {
            for line in input_rx {
                if tx.send(Event::Frontend(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::FrontendClosed);
        });
    }
    std::thread::spawn(move || {
        // Bytes, not `lines()`: a stray invalid UTF-8 byte must not end the session.
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim().to_owned();
                    if events_tx.send(Event::Hermes(line)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = events_tx.send(Event::HermesClosed);
    });
    let bridge = Bridge::new(stdin, writer, mirror, opts, mcp_servers, bridge_tx);
    std::thread::spawn(move || bridge.run(events_rx));
    Ok((input_tx, lines))
}

/// A Hermes conversation's history for the frontend, read from HQ's own copy of it. A session HQ
/// has no copy of (one started in an editor, say) is copied from Hermes first, when `cwd` is given.
pub fn load_transcript(app: &AppHandle, session_id: &str, cwd: Option<&str>, before: Option<u64>) -> Result<TranscriptChunk, String> {
    if !valid_session_id(session_id) {
        return Err("invalid session id".into());
    }
    let path = mirror_dir(app)?.join(format!("{session_id}.jsonl"));
    if !path.is_file() {
        match cwd.filter(|c| before.is_none() && Path::new(c).is_dir()) {
            Some(cwd) => import(&path, session_id, cwd)?,
            None => return Ok(TranscriptChunk::default()),
        }
    }
    if !path.is_file() {
        return Ok(TranscriptChunk::default());
    }
    store::read_transcript(&path, before).map_err(|e| e.to_string())
}

/// Writes HQ's copy of a conversation from the history Hermes replays when the session loads.
fn import(path: &Path, session_id: &str, cwd: &str) -> Result<(), String> {
    let mut hermes = OneShot::start(Some(cwd))?;
    let mut replay = Replay::default();
    let params = json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] });
    let loaded = hermes.ask("session/load", params, |msg| {
        if msg["method"] == "session/update" && msg["params"]["sessionId"] == session_id {
            replay.push(&msg["params"]["update"]);
        }
    })?;
    if loaded.as_object().is_none_or(|r| r.is_empty()) {
        return Err(format!("Hermes has no session {session_id}"));
    }
    let lines = replay.finish();
    if lines.is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // Written aside and moved in, so a reader never sees half of it.
    let partial = path.with_extension("jsonl.part");
    let stamp = now_iso();
    let text: String = lines
        .into_iter()
        .map(|mut line| {
            line["timestamp"] = json!(stamp);
            format!("{line}\n")
        })
        .collect();
    fs::write(&partial, text).and_then(|_| fs::rename(&partial, path)).map_err(|e| e.to_string())
}

/// Hermes' history replay, as the Claude-shaped lines a cell records.
#[derive(Default)]
struct Replay {
    lines: Vec<Value>,
    user: String,
    text: String,
    seq: u64,
    todos: Todos,
    /// Todo calls, which are drawn as TodoWrite and whose own updates are dropped.
    todo_ids: HashSet<String>,
}

impl Replay {
    fn push(&mut self, u: &Value) {
        let chunk = u["content"]["text"].as_str().unwrap_or_default();
        match u["sessionUpdate"].as_str().unwrap_or_default() {
            "user_message_chunk" => {
                self.flush_text();
                self.user.push_str(chunk);
            }
            "agent_message_chunk" => {
                self.flush_user();
                self.text.push_str(chunk);
            }
            "tool_call" => {
                self.flush_user();
                self.flush_text();
                let Some(id) = u["toolCallId"].as_str() else { return };
                if merge_todo_call(&mut self.todos, u) {
                    self.todo_ids.insert(id.to_owned());
                    self.assistant(json!([todo_write_block(id, todo_items(&self.todos))]));
                    self.lines.push(todo_result_line(&None, id));
                    return;
                }
                let (name, input) = tool_use(u);
                self.assistant(json!([{ "type": "tool_use", "id": id, "name": name, "input": input }]));
            }
            "tool_call_update" => {
                let status = u["status"].as_str().unwrap_or_default();
                let Some(id) = u["toolCallId"].as_str().filter(|_| status == "completed" || status == "failed") else { return };
                if self.todo_ids.contains(id) {
                    return;
                }
                self.flush_user();
                self.flush_text();
                let content = json!([{ "type": "tool_result", "tool_use_id": id, "content": tool_text(u), "is_error": status == "failed" }]);
                self.lines.push(json!({ "type": "user", "message": { "role": "user", "content": content } }));
            }
            _ => {}
        }
    }

    fn flush_user(&mut self) {
        let mut text = std::mem::take(&mut self.user);
        // HQ's notes come last, after what the user wrote.
        for note in [TASK_PROMPT, COMPOSER_TASK_HINT] {
            if let Some(at) = text.find(note) {
                text.replace_range(text[..at].trim_end().len()..at + note.len(), "");
            }
        }
        if !text.trim().is_empty() {
            self.lines.push(json!({ "type": "user", "message": { "role": "user", "content": text } }));
        }
    }

    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            self.assistant(json!([{ "type": "text", "text": text.trim() }]));
        }
    }

    fn assistant(&mut self, content: Value) {
        self.seq += 1;
        let id = format!("hermes-import-{}", self.seq);
        self.lines.push(json!({ "type": "assistant", "message": { "id": id, "type": "message", "role": "assistant", "content": content } }));
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush_user();
        self.flush_text();
        self.lines
    }
}

/// Hermes' ACP sessions, newest first, for the resume list: each one's `sessionId`, `cwd`,
/// `title` and `updatedAt` as Hermes reports them, plus whether the folder still exists.
pub fn list_sessions(cwd: Option<&str>, limit: usize) -> Result<Vec<Value>, String> {
    let mut hermes = OneShot::start(None)?;
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0.. {
        let mut params = json!({});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let reply = hermes.ask("session/list", params, |_| {})?;
        for s in reply["sessions"].as_array().into_iter().flatten() {
            let (Some(id), Some(cwd)) = (s["sessionId"].as_str(), s["cwd"].as_str()) else { continue };
            if valid_session_id(id) {
                sessions.push(json!({ "sessionId": id, "cwd": cwd, "cwdExists": Path::new(cwd).is_dir(), "title": s["title"], "updatedAt": s["updatedAt"] }));
            }
        }
        let next = reply["nextCursor"].as_str().map(str::to_owned);
        if next.is_none() || next == cursor || sessions.len() >= limit || page >= 20 {
            break;
        }
        cursor = next;
    }
    sessions.truncate(limit);
    Ok(sessions)
}

/// A short-lived `hermes acp`, asked a few questions and ended when dropped.
struct OneShot {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Value>,
    next: u64,
    /// When the whole exchange gives up; Python starts slowly, so it is generous.
    deadline: Instant,
}

impl OneShot {
    fn start(cwd: Option<&str>) -> Result<Self, String> {
        let mut cmd = Command::new(hermes_program());
        cmd.arg("acp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        hide_console(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("Could not start `{} acp` ({e})", hermes_program().display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            crate::session::kill_group(child.id());
            let _ = child.kill();
            return Err("hermes pipes unavailable".into());
        };
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        });
        let mut hermes = OneShot { child, stdin: Some(stdin), rx, next: 0, deadline: Instant::now() + Duration::from_secs(120) };
        let init = json!({ "protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}, "clientInfo": { "name": "hq", "version": env!("CARGO_PKG_VERSION") } });
        hermes.ask("initialize", init, |_| {})?;
        Ok(hermes)
    }

    /// Sends a request and waits for its answer, handing every message before it to `notice`.
    fn ask(&mut self, method: &str, params: Value, mut notice: impl FnMut(&Value)) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let stdin = self.stdin.as_mut().ok_or("hermes closed")?;
        writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).map_err(|e| e.to_string())?;
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            let msg = self.rx.recv_timeout(left).map_err(|_| "Hermes did not answer".to_string())?;
            if msg["id"].as_u64() == Some(id) && msg.get("method").is_none() {
                return match msg.get("error") {
                    Some(err) => Err(err["message"].as_str().unwrap_or("Hermes reported an error").to_owned()),
                    None => Ok(msg["result"].clone()),
                };
            }
            notice(&msg);
        }
    }
}

impl Drop for OneShot {
    fn drop(&mut self) {
        // Stdin closes first: if the kill only reaches a launcher shim, Python still sees EOF and ends.
        drop(self.stdin.take());
        crate::session::kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Event {
    Frontend(String),
    FrontendClosed,
    Hermes(String),
    HermesClosed,
    /// `discard_lost_turn` finished for this session, off the bridge thread.
    LostTurnDiscarded { session: String, result: Result<(), String> },
}

/// What a JSON-RPC response from Hermes answers.
enum Call {
    Initialize,
    /// `session/new`, `session/load` or `session/fork` for the cell's own session.
    Open { loaded: Option<String>, fork_of: Option<String> },
    /// A cell turn. `prompt` is kept so a prompt refused for a lost turn can be resent once.
    Prompt { started: Instant, prompt: Value, retried: bool },
    /// `session/fork` for a side question (Handoff, the progress button).
    SideFork { request_id: String, question: String },
    /// `session/set_mode` putting a side question's fork back to asking before edits.
    SideMode { fork: String, question: String },
    SidePrompt { fork: String },
    /// `session/list` written to a turn gone quiet (see `TURN_NUDGE`); its answer is not used.
    Nudge,
    /// A model or mode change the cell asked for, sent once Hermes was idle (see `Held`).
    Held(Held),
    /// The cell's own model or mode, set before anything the cell sent is passed on.
    Setup(After),
}

/// A model or mode change waiting for Hermes to be idle: Hermes refuses either during a turn
/// ("Mode changes apply when the session is idle"), and while busy may report it as a bare
/// "Internal error" with `session_busy` details. `tries`: how often Hermes already said it was busy.
struct Held {
    request_id: String,
    then: After,
    tries: u32,
}

/// State to change once Hermes accepts a control request.
enum After {
    Model(String),
    Mode(String),
}

/// A permission request waiting on the user.
struct Permission {
    rpc_id: Value,
    /// The request's own id (`perm-check-N`, `edit-approval-N`), which Hermes closes when its
    /// approval timeout runs out.
    tool_call_id: String,
    /// The tool call that was running when Hermes asked. Hermes can give up on that tool first
    /// (its own tool timeout), leaving the request open with nobody waiting on the answer.
    during: Option<String>,
    options: Vec<(String, String)>, // (optionId, kind)
}

/// The reply being streamed: one Hermes message id, flushed as one Claude assistant message.
#[derive(Default)]
struct Reply {
    id: Option<String>,
    /// The message id the thought arrived under, which is the answer's too when both come from one model call.
    thought_id: Option<String>,
    text: String,
    thinking: String,
}

struct Bridge {
    hermes: Option<ChildStdin>,
    out: Option<PipeWriter>,
    mirror_dir: PathBuf,
    mirror: Option<File>,
    opts: SpawnOptions,
    /// ACP `mcpServers` for the sessions this process opens: HQ's own server, or none.
    mcp_servers: Value,
    next_rpc: u64,
    calls: HashMap<u64, Call>,
    session: Option<String>,
    /// While `session/load` replays the history, which HQ already drew from its own copy.
    loading: bool,
    /// Whether the session's lost turn was already discarded, so opening it is retried only once.
    discarded_lost_turn: bool,
    /// A turn's prompt held while HQ discards a turn Hermes lost mid-session, and whether Hermes
    /// took none of it (so it is resent) rather than accepting it before losing it (it is not).
    lost_turn_prompt: Option<(Instant, Value, bool)>,
    /// The bridge's own event queue, for work finished on another thread.
    events: mpsc::Sender<Event>,
    /// Frontend lines that arrived before the session was open.
    early: Vec<String>,
    /// Setup calls Hermes has yet to answer; the cell's lines wait in `early` until it has.
    setup: usize,
    /// While the session is not yet ready: when Hermes last said anything. `None` once ready.
    opening: Option<Instant>,
    /// Prompts sent while a turn runs. Hermes would answer them at once and run them later
    /// without a reply of their own, so they wait here and each gets its own turn and result.
    prompts: VecDeque<(Value, Value)>, // (ACP prompt, the frontend's content for HQ's copy)
    in_turn: bool,
    /// Model and mode changes the cell asked for while Hermes was busy, the latest of each kind,
    /// sent once the turn ends. The next queued prompt waits for their answers (`applying`), or
    /// it would make Hermes busy again first.
    held: Vec<Held>,
    applying: usize,
    /// When the held changes now in flight were sent (see `HELD_ANSWER`).
    applying_since: Option<Instant>,
    /// When to ask again for a held change Hermes said it was too busy for (see `HELD_RETRY`).
    held_retry: Option<Instant>,
    /// A turn Hermes started itself, such as on a background process's notice: when it began and
    /// when Hermes last reported on it. Its updates stream in like a turn's, but no reply ends it.
    own_turn: Option<(Instant, Instant)>,
    /// When Hermes last said anything, or HQ last nudged it: what `TURN_NUDGE` counts from.
    heard: Instant,
    models: Vec<Value>,
    model: String,
    default_model: String,
    /// Hermes' slash commands; `None` until Hermes sends them, just after the session opens.
    commands: Option<Vec<Value>>,
    /// Initialize requests waiting for the commands, which their reply carries.
    waiting_init: Vec<String>,
    mode: String,
    title: Option<String>,
    ctx_size: u64,
    ctx_used: u64,
    /// Running token totals for this process, as Claude reports them (cumulative).
    totals: (u64, u64, u64),
    reply: Reply,
    reply_seq: u64,
    tools: HashSet<String>,
    /// What each open terminal call is running, by tool call id (see `real_command`).
    terminal_commands: HashMap<String, String>,
    /// This session has delegated work. A child runs beside the parent and asks through the same
    /// channel, so a child's gate could carry the text of the parent's open terminal call, and no
    /// command can be vouched for from here on. Kept until the session ends, as a child can outlive a turn.
    delegated: bool,
    last_tool: Option<String>,
    permissions: HashMap<String, Permission>,
    /// Side questions in flight: forked session id -> (request id, answer so far).
    sides: HashMap<String, (String, String)>,
    /// Hermes' todo list for this session, which its updates (`merge`) change piecemeal.
    todos: Todos,
    /// Whether `todos` holds the whole list. A fork starts without its parent's, so until Hermes
    /// writes the list out in full its merges would draw a partial checklist, and are not drawn.
    /// (Seeding it from the parent is not possible: the copied mirror keeps no Hermes item ids.)
    todos_known: bool,
    /// The running turn's prompt and the titles of its last tool calls, for side questions.
    turn_prompt: String,
    turn_tools: VecDeque<String>,
    /// Whether this process still owes Hermes `TASK_PROMPT` (the Automatic task lists setting).
    task_prompt_due: bool,
}

impl Bridge {
    fn new(hermes: ChildStdin, out: PipeWriter, mirror_dir: PathBuf, opts: SpawnOptions, mcp_servers: Value, events: mpsc::Sender<Event>) -> Bridge {
        let task_prompt_due = opts.auto_tasks;
        Bridge {
            hermes: Some(hermes),
            out: Some(out),
            mirror_dir,
            mirror: None,
            opts,
            mcp_servers,
            next_rpc: 0,
            calls: HashMap::new(),
            session: None,
            loading: false,
            discarded_lost_turn: false,
            lost_turn_prompt: None,
            events,
            early: Vec::new(),
            setup: 0,
            opening: Some(Instant::now()),
            prompts: VecDeque::new(),
            in_turn: false,
            held: Vec::new(),
            applying: 0,
            applying_since: None,
            held_retry: None,
            own_turn: None,
            heard: Instant::now(),
            models: Vec::new(),
            model: String::new(),
            default_model: String::new(),
            commands: None,
            waiting_init: Vec::new(),
            mode: "default".into(),
            title: None,
            ctx_size: 0,
            ctx_used: 0,
            totals: (0, 0, 0),
            reply: Reply::default(),
            reply_seq: 0,
            tools: HashSet::new(),
            terminal_commands: HashMap::new(),
            delegated: false,
            last_tool: None,
            permissions: HashMap::new(),
            sides: HashMap::new(),
            todos: Vec::new(),
            todos_known: true,
            turn_prompt: String::new(),
            turn_tools: VecDeque::new(),
            task_prompt_due,
        }
    }

    fn run(mut self, events: mpsc::Receiver<Event>) {
        self.call(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // Hermes reads and writes files and runs commands itself.
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                "clientInfo": { "name": "hq", "title": "HQ", "version": env!("CARGO_PKG_VERSION") },
            }),
            Call::Initialize,
        );
        loop {
            // Only a turn Hermes started itself, or a session still opening, needs the clock:
            // nothing else ends them.
            let mut wait = match (self.own_turn, self.opening) {
                (Some(_), _) => Some(Duration::from_millis(250)),
                (None, Some(since)) => Some((since + OPEN_QUIET).saturating_duration_since(Instant::now())),
                (None, None) => None,
            };
            if let Some(nudge) = self.nudge_wait() {
                wait = Some(wait.map_or(nudge, |w| w.min(nudge)));
            }
            let held_clock = self.held_retry.into_iter().chain(self.applying_since.map(|at| at + HELD_ANSWER)).min();
            if let Some(at) = held_clock {
                let retry = at.saturating_duration_since(Instant::now());
                wait = Some(wait.map_or(retry, |w| w.min(retry)));
            }
            let event = match wait {
                Some(wait) => match events.recv_timeout(wait) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.own_turn_quiet();
                        self.open_quiet();
                        self.turn_quiet();
                        self.held_unanswered();
                        if self.held_retry.is_some_and(|at| at <= Instant::now()) {
                            self.held_retry = None;
                            self.release();
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match events.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };
            if self.opening.is_some() && !matches!(event, Event::Frontend(_)) {
                self.opening = Some(Instant::now());
            }
            match event {
                Event::Frontend(line) => {
                    if self.session.is_none() || self.setup > 0 {
                        self.early.push(line);
                    } else {
                        self.frontend(&line);
                    }
                }
                // Closing Hermes' stdin ends it; its stdout closing then ends this loop.
                Event::FrontendClosed => self.hermes = None,
                Event::Hermes(line) => self.from_hermes(&line),
                Event::HermesClosed => break,
                Event::LostTurnDiscarded { session, result } => self.lost_turn_discarded(&session, result),
            }
        }
        // Dropping the writer ends the frontend's stream, which then reports the exit.
    }

    /// `discard_lost_turn` finished: reopen the session, or carry on the turn whose prompt it refused.
    fn lost_turn_discarded(&mut self, session: &str, result: Result<(), String>) {
        match (self.lost_turn_prompt.take(), result) {
            (None, Ok(())) => self.open_session(),
            (None, Err(why)) => self.fatal(&lost_turn_message(session, &why)),
            (Some((started, prompt, true)), Ok(())) => self.call("session/prompt", prompt, Call::Prompt { started, prompt: Value::Null, retried: true }),
            (Some((started, _, false)), Ok(())) => self.turn_ended(started, "error", &Value::Null, Some(LOST_ACCEPTED_TURN)),
            (Some((started, ..)), Err(why)) => {
                let error = format!("Hermes lost a turn when its gateway restarted and HQ could not clear it ({why}). Run `hermes sessions discard {session} --yes` in a terminal, then send again.");
                self.turn_ended(started, "error", &Value::Null, Some(&error));
            }
        }
    }

    // ---------- writing ----------

    fn send_hermes(&mut self, msg: Value) {
        let Some(stdin) = self.hermes.as_mut() else { return };
        let written = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
        if written.is_err() {
            self.hermes = None; // Hermes is gone; its stdout closing reports the exit
        }
    }

    fn call(&mut self, method: &str, params: Value, call: Call) {
        let id = self.next_rpc;
        self.next_rpc += 1;
        self.calls.insert(id, call);
        self.send_hermes(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    fn emit(&mut self, msg: Value) {
        let Some(out) = self.out.as_mut() else { return };
        if writeln!(out, "{msg}").is_err() {
            self.out = None;
        }
    }

    fn reply_ok(&mut self, request_id: &str, response: Value) {
        self.emit(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": response } }));
    }

    fn reply_err(&mut self, request_id: &str, error: &str) {
        self.emit(json!({ "type": "control_response", "response": { "subtype": "error", "request_id": request_id, "error": error } }));
    }

    /// Appends a line to HQ's copy of the conversation, which a restored cell draws from.
    fn record(&mut self, mut line: Value) {
        if self.mirror.is_none() {
            let Some(id) = self.session.as_deref().filter(|id| valid_session_id(id)) else { return };
            let path = self.mirror_dir.join(format!("{id}.jsonl"));
            self.mirror = fs::create_dir_all(&self.mirror_dir).and_then(|_| OpenOptions::new().create(true).append(true).open(path)).ok();
        }
        let Some(file) = self.mirror.as_mut() else { return };
        line["timestamp"] = json!(now_iso());
        if writeln!(file, "{line}").is_err() {
            self.mirror = None;
        }
    }

    // ---------- from Hermes ----------

    fn from_hermes(&mut self, line: &str) {
        self.heard = Instant::now();
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
            (Some(method), Some(id)) => self.hermes_request(method, id.clone(), &msg["params"]),
            (Some("session/update"), None) => self.update(&msg["params"]),
            // Hermes gave up on a request of its own, such as a permission it stopped waiting for.
            (Some("$/cancel_request"), None) => {
                let rpc_id = msg["params"]["requestId"].clone();
                self.dismiss(|p| p.rpc_id == rpc_id);
            }
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(call) = id.as_u64().and_then(|id| self.calls.remove(&id)) else { return };
                match msg.get("error") {
                    Some(err) => self.failed(call, err["message"].as_str().unwrap_or("Hermes reported an error"), err["data"]["details"].as_str()),
                    None => self.answered(call, &msg["result"]),
                }
            }
            (None, None) => {}
        }
    }

    fn answered(&mut self, call: Call, result: &Value) {
        match call {
            Call::Initialize => self.open_session(),
            Call::Open { loaded, fork_of } => self.opened(result, loaded, fork_of),
            Call::Prompt { started, .. } => self.turn_ended(started, result["stopReason"].as_str().unwrap_or("end_turn"), &result["usage"], None),
            Call::SideFork { request_id, question } => {
                let Some(fork) = result["sessionId"].as_str().map(str::to_owned) else {
                    return self.reply_err(&request_id, "Hermes did not fork the session");
                };
                self.sides.insert(fork.clone(), (request_id, String::new()));
                // The fork asks for every edit, and every request it makes is denied (see
                // `hermes_request`). Shell commands Hermes' own approval does not flag still run,
                // as they would in the cell. The fork stays in Hermes' session list: ACP has no delete.
                self.call("session/set_mode", json!({ "sessionId": fork, "modeId": "default" }), Call::SideMode { fork, question });
            }
            // Only once the fork asks before edits does it get the question.
            Call::SideMode { fork, question } => {
                let prompt = json!({ "sessionId": fork, "prompt": [{ "type": "text", "text": question }] });
                self.call("session/prompt", prompt, Call::SidePrompt { fork });
            }
            Call::SidePrompt { fork } => {
                if let Some((request_id, answer)) = self.sides.remove(&fork) {
                    // A stopped answer is cut short; Handoff must not save it as complete.
                    match result["stopReason"].as_str() {
                        Some("cancelled") => self.reply_err(&request_id, "Stopped before Hermes finished answering"),
                        Some("refusal") => self.reply_err(&request_id, "Hermes refused to answer"),
                        _ => self.reply_ok(&request_id, json!({ "response": answer.trim() })),
                    }
                }
            }
            Call::Setup(then) => {
                match then {
                    After::Model(model) => self.model = model,
                    After::Mode(mode) => self.mode = mode,
                }
                self.setup_done();
            }
            Call::Held(held) => {
                self.applying -= 1;
                match held.then {
                    After::Model(model) => self.model = model,
                    After::Mode(mode) => self.mode = mode,
                }
                self.reply_ok(&held.request_id, json!({}));
                self.release();
            }
            Call::Nudge => {}
        }
    }

    fn failed(&mut self, call: Call, error: &str, details: Option<&str>) {
        match call {
            // A turn Hermes lost when its gateway restarted (HQ restarting to update does that to
            // every running session) blocks the session, and Hermes reports it only as "Internal
            // error". Discarding it is what Hermes' own `/discard` does; the turn is gone either way.
            Call::Open { loaded, fork_of } if details.is_some_and(|d| d.starts_with("unknown_execution")) => {
                match loaded.or(fork_of).filter(|id| valid_session_id(id)) {
                    Some(session) if !self.discarded_lost_turn => {
                        self.discarded_lost_turn = true;
                        let (cwd, events) = (self.opts.cwd.clone(), self.events.clone());
                        // The gateway can take a while to answer; the bridge keeps serving the cell.
                        std::thread::spawn(move || {
                            let result = discard_lost_turn(&session, &cwd);
                            let _ = events.send(Event::LostTurnDiscarded { session, result });
                        });
                    }
                    Some(session) => self.fatal(&lost_turn_message(&session, "it was discarded, but Hermes still reports one")),
                    None => self.fatal(&format!("Hermes could not start the session: {error}")),
                }
            }
            Call::Initialize | Call::Open { .. } => self.fatal(&format!("Hermes could not start the session: {error}")),
            // The same lost turn, met mid-session (the gateway restarted under an open cell): every
            // prompt is refused until it is discarded, and a `/discard` HQ wraps in a note or task
            // prompt is not one Hermes recognises. HQ discards it and resends a prompt Hermes took
            // none of; one Hermes accepted and then lost may have run in part, so it is not resent.
            Call::Prompt { started, prompt, retried: false } if details.is_some_and(|d| d.starts_with("unknown_execution")) && self.session.as_deref().is_some_and(valid_session_id) => {
                let session = self.session.clone().unwrap_or_default();
                let untouched = details.is_some_and(|d| d.contains("nothing was submitted"));
                self.lost_turn_prompt = Some((started, prompt, untouched));
                let (cwd, events) = (self.opts.cwd.clone(), self.events.clone());
                std::thread::spawn(move || {
                    let result = discard_lost_turn(&session, &cwd);
                    let _ = events.send(Event::LostTurnDiscarded { session, result });
                });
            }
            // Hermes' ACP layer reports its own refusals as "Internal error" and puts the reason in details.
            Call::Prompt { started, .. } => {
                let error = match details {
                    Some(d) if error == "Internal error" && !d.is_empty() => d,
                    _ => error,
                };
                self.turn_ended(started, "error", &Value::Null, Some(error));
            }
            // Hermes forks only an idle session (`session_busy` mid-turn) and has no /btw over ACP
            // yet, so a side question asked mid-turn goes to a fresh session. That session cannot
            // see the conversation, only what `side_prompt` put in the question.
            Call::SideFork { request_id, question } if details == Some("session_busy") => {
                let question = format!("{SIDE_FRESH_PREFACE}\n\n{question}");
                let params = json!({ "cwd": self.opts.cwd, "mcpServers": [] });
                self.call("session/new", params, Call::SideFork { request_id, question });
            }
            // Hermes started a turn HQ has not seen yet: asked again shortly, unless a newer change replaced it.
            Call::Held(mut held) if is_busy(error, details) && held.tries + 1 < HELD_TRIES => {
                self.applying -= 1;
                held.tries += 1;
                let same = |h: &Held| std::mem::discriminant(&h.then) == std::mem::discriminant(&held.then);
                if self.held.iter().any(same) {
                    // The cell chose again meanwhile; the newer change goes instead.
                    self.reply_ok(&held.request_id, json!({}));
                } else {
                    self.held.push(held);
                }
                self.held_retry.get_or_insert_with(|| Instant::now() + HELD_RETRY);
                self.release();
            }
            Call::Held(held) => {
                self.applying -= 1;
                self.reply_err(&held.request_id, &rpc_error(error, details));
                self.release();
            }
            Call::SideFork { request_id, .. } => self.reply_err(&request_id, error),
            Call::SideMode { fork, .. } | Call::SidePrompt { fork } => {
                if let Some((request_id, _)) = self.sides.remove(&fork) {
                    self.reply_err(&request_id, error);
                }
            }
            // The cell keeps whatever Hermes is really using; system/init reports that.
            Call::Setup(_) => self.setup_done(),
            Call::Nudge => {}
        }
    }

    /// Reports an error the cell cannot recover from, then ends Hermes.
    fn fatal(&mut self, error: &str) {
        self.opening = None;
        self.dismiss(|_| true);
        self.emit(json!({ "type": "result", "subtype": "error", "is_error": true, "result": error }));
        self.hermes = None;
    }

    fn open_session(&mut self) {
        let cwd = self.opts.cwd.clone();
        match self.opts.resume.clone().filter(|id| !id.is_empty()) {
            Some(id) if self.opts.fork => {
                let params = json!({ "sessionId": id, "cwd": cwd, "mcpServers": self.mcp_servers });
                self.call("session/fork", params, Call::Open { loaded: None, fork_of: Some(id) });
            }
            // `session/load`, not `session/resume`: Hermes answers an unknown id for resume
            // with a brand new session, and load says plainly that it found nothing.
            Some(id) => {
                self.loading = true;
                self.todos_known = false; // until the replay brings a full write
                let params = json!({ "sessionId": id, "cwd": cwd, "mcpServers": self.mcp_servers });
                self.call("session/load", params, Call::Open { loaded: Some(id), fork_of: None });
            }
            None => self.call("session/new", json!({ "cwd": cwd, "mcpServers": self.mcp_servers }), Call::Open { loaded: None, fork_of: None }),
        }
    }

    fn opened(&mut self, result: &Value, loaded: Option<String>, fork_of: Option<String>) {
        self.loading = false;
        let Some(id) = loaded.clone().or_else(|| result["sessionId"].as_str().map(str::to_owned)) else {
            return self.fatal("Hermes did not report a session id");
        };
        // Load answers an id it does not know with an empty result.
        if loaded.is_some() && result.as_object().is_none_or(|r| r.is_empty()) {
            return self.fatal(&format!("Hermes has no session {id} (it may have been deleted)"));
        }
        // A load knows the list if its replay held a full write (only merges are left after a
        // context compaction, say); otherwise it waits for one, like a fork.
        if fork_of.is_some() {
            self.todos_known = false;
        } else if loaded.is_none() {
            self.todos_known = true;
        }
        if let Some(parent) = fork_of.filter(|p| valid_session_id(p) && valid_session_id(&id)) {
            let _ = fs::copy(self.mirror_dir.join(format!("{parent}.jsonl")), self.mirror_dir.join(format!("{id}.jsonl")));
        }
        self.session = Some(id.clone());
        let models = &result["models"];
        self.models = models["availableModels"].as_array().cloned().unwrap_or_default();
        self.model = models["currentModelId"].as_str().unwrap_or_default().to_owned();
        self.default_model = self.model.clone();
        if let Some(mode) = result["modes"]["currentModeId"].as_str() {
            self.mode = mode.to_owned();
        }

        // The cell's own model and mode, chosen in HQ before Hermes started. Nothing the cell
        // sent reaches Hermes until both are in place.
        if let Some(model) = self.opts.model.clone().filter(|m| !m.is_empty() && *m != "default" && *m != self.model) {
            self.setup += 1;
            self.call("session/set_model", json!({ "sessionId": id, "modelId": model }), Call::Setup(After::Model(model)));
        }
        if let Some(mode) = self.opts.permission_mode.as_deref().and_then(hermes_mode).filter(|m| *m != self.mode) {
            self.setup += 1;
            self.call("session/set_mode", json!({ "sessionId": id, "modeId": mode }), Call::Setup(After::Mode(mode.to_owned())));
        }
        if self.setup == 0 {
            self.ready();
        }
    }

    fn setup_done(&mut self) {
        self.setup -= 1;
        if self.setup == 0 {
            self.ready();
        }
    }

    /// Passes on what the cell sent while the session was opening.
    fn ready(&mut self) {
        self.opening = None;
        for line in std::mem::take(&mut self.early) {
            self.frontend(&line);
        }
    }

    fn hermes_request(&mut self, method: &str, id: Value, params: &Value) {
        if method != "session/request_permission" {
            // Hermes does its own file and terminal work; nothing else is offered.
            return self.send_hermes(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("HQ does not handle {method}") } }));
        }
        let options: Vec<(String, String)> = params["options"]
            .as_array()
            .map(|o| o.iter().map(|o| (o["optionId"].as_str().unwrap_or("").to_owned(), o["kind"].as_str().unwrap_or("").to_owned())).collect())
            .unwrap_or_default();
        // A side question only asks: anything it wants approved is refused, as is anything from
        // a session that is not this cell's.
        if params["sessionId"].as_str() != self.session.as_deref() {
            let outcome = pick_option(&options, false, false).map_or(json!({ "outcome": "cancelled" }), |o| json!({ "outcome": "selected", "optionId": o }));
            return self.send_hermes(json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome } }));
        }
        if !self.in_turn {
            self.note_own_turn(); // a turn Hermes started may ask before it reports anything
        }
        let tool = &params["toolCall"];
        let (name, input) = tool_use(tool);
        let title = tool["title"].as_str().unwrap_or_default();
        let request_id = format!("hermes-perm-{}", id.to_string().trim_matches('"'));
        let mut request = json!({
            "subtype": "can_use_tool", "tool_name": name, "input": input,
            // The card reads "Allow Write?  <path>" or "Allow Bash?  <what Hermes says it does>".
            "description": title.strip_prefix("Approve edit: ").unwrap_or(title), "tool_use_id": tool["toolCallId"],
        });
        let command = !self.delegated && real_command(tool, &options, &self.terminal_commands);
        if command || plain_edit(tool) {
            request["hermes_reviewable"] = json!(true);
        }
        // The detector's own words, on a real command only: elsewhere this field is the gate's text.
        if let Some(flag) = tool["rawInput"]["description"].as_str().filter(|d| command && !d.is_empty()) {
            request["hermes_flag"] = json!(flag);
        }
        // "Allow for session" becomes the card's second choice.
        if options.iter().any(|(o, _)| o == "allow_session") {
            request["permission_suggestions"] = json!([{ "type": "addRules", "rules": [{ "toolName": name }], "destination": "session" }]);
        }
        let tool_call_id = tool["toolCallId"].as_str().unwrap_or_default().to_owned();
        // Only when one tool is open is it surely the one asking; with several, the card waits
        // for Hermes' approval timeout instead.
        let during = self.last_tool.clone().filter(|t| self.tools.len() == 1 && self.tools.contains(t));
        self.permissions.insert(request_id.clone(), Permission { rpc_id: id, tool_call_id, during, options });
        self.emit(json!({ "type": "control_request", "request_id": request_id, "request": request }));
    }

    fn update(&mut self, params: &Value) {
        let session = params["sessionId"].as_str().unwrap_or_default();
        let u = &params["update"];
        let kind = u["sessionUpdate"].as_str().unwrap_or_default();
        if let Some((_, answer)) = self.sides.get_mut(session) {
            if kind == "agent_message_chunk" {
                answer.push_str(u["content"]["text"].as_str().unwrap_or_default());
            }
            return;
        }
        if self.loading {
            // HQ draws the history from its own copy, but the todo list lives on in Hermes.
            if kind == "tool_call" && merge_todo_call(&mut self.todos, u) {
                self.todos_known |= u["rawInput"]["merge"].as_bool() != Some(true);
            }
            return;
        }
        if self.session.as_deref() != Some(session) {
            return;
        }
        // Work while no prompt of HQ's runs is a turn Hermes started itself. Usage, title and
        // command updates also follow HQ's own turns, so they do not count, nor does news of a
        // tool that is not open (a straggler from a turn already over).
        let open_tool = || u["toolCallId"].as_str().is_some_and(|t| self.tools.contains(t));
        if !self.in_turn && (matches!(kind, "agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "plan") || (kind == "tool_call_update" && open_tool())) {
            self.note_own_turn();
        }
        match kind {
            "agent_message_chunk" => {
                let Some(text) = u["content"]["text"].as_str().filter(|t| !t.is_empty()) else { return };
                let id = u["messageId"].as_str();
                let current = self.reply.id.as_deref().or(self.reply.thought_id.as_deref());
                if current.is_some() && id.is_some() && current != id {
                    self.flush();
                }
                // The answer's leading blank lines are not worth drawing.
                let text = if self.reply.text.is_empty() { text.trim_start() } else { text };
                if text.is_empty() {
                    return;
                }
                if self.reply.text.is_empty() {
                    self.start_reply(id.map(str::to_owned));
                }
                self.reply.text.push_str(text);
                self.emit(json!({
                    "type": "stream_event", "parent_tool_use_id": null, "session_id": self.session,
                    "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } },
                }));
            }
            "agent_thought_chunk" => {
                if let Some(text) = u["content"]["text"].as_str() {
                    // A thought after the answer began, or under another message id, is a new model call's.
                    let id = u["messageId"].as_str();
                    let current = self.reply.thought_id.as_deref().or(self.reply.id.as_deref());
                    if !self.reply.text.is_empty() || (current.is_some() && id.is_some() && current != id) {
                        self.flush();
                    }
                    if self.reply.thought_id.is_none() {
                        self.reply.thought_id = id.map(str::to_owned);
                    }
                    self.reply.thinking.push_str(text);
                }
            }
            "tool_call" => {
                self.flush();
                let Some(id) = u["toolCallId"].as_str().map(str::to_owned) else { return };
                if let Some(title) = u["title"].as_str().filter(|t| !t.is_empty()) {
                    if self.turn_tools.len() >= 30 {
                        self.turn_tools.pop_front();
                    }
                    self.turn_tools.push_back(title.to_owned());
                }
                // Hermes' todo list is drawn as a TodoWrite of the whole list instead of a card of its own.
                // Its updates are not tracked as tools, so they bring no orphan result.
                if merge_todo_call(&mut self.todos, u) {
                    self.todos_known |= u["rawInput"]["merge"].as_bool() != Some(true);
                    if !self.todos_known {
                        return;
                    }
                    return self.emit_todos(format!("hermes-tool-{id}"), &id, todo_items(&self.todos));
                }
                let (name, input) = tool_use(u);
                self.tools.insert(id.clone());
                self.delegated |= u["title"].as_str().is_some_and(|t| t.starts_with("delegate_task"));
                if let Some(command) = terminal_command(u) {
                    self.terminal_commands.insert(id.clone(), command);
                }
                self.last_tool = Some(id.clone());
                self.assistant(format!("hermes-tool-{id}"), json!([{ "type": "tool_use", "id": id, "name": name, "input": input }]));
            }
            "tool_call_update" => {
                let Some(id) = u["toolCallId"].as_str() else { return };
                let status = u["status"].as_str().unwrap_or_default();
                if status != "completed" && status != "failed" {
                    return;
                }
                // Hermes settled the request itself (its approval timeout ran out), or the tool
                // that asked has ended (its tool timeout ran out first).
                self.dismiss(|p| p.tool_call_id == id || p.during.as_deref() == Some(id));
                self.terminal_commands.remove(id);
                if self.tools.remove(id) {
                    let result = json!({
                        "type": "user", "session_id": self.session, "parent_tool_use_id": null,
                        "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": tool_text(u), "is_error": status == "failed" }] },
                    });
                    self.record(result.clone());
                    self.emit(result);
                }
            }
            "plan" => {
                // Hermes' todo list, shown as a TodoWrite so the checklist and progress panel follow it.
                self.flush();
                let todos: Vec<Value> = u["entries"]
                    .as_array()
                    .map(|e| e.iter().map(|e| json!({ "content": e["content"], "activeForm": e["content"], "status": e["status"] })).collect())
                    .unwrap_or_default();
                self.reply_seq += 1;
                let id = format!("hermes-plan-{}", self.reply_seq);
                self.emit_todos(format!("{id}-msg"), &id, todos);
            }
            "available_commands_update" => {
                self.commands = Some(u["availableCommands"].as_array().cloned().unwrap_or_default());
                self.answer_init();
            }
            "usage_update" => {
                self.ctx_size = u["size"].as_u64().unwrap_or(self.ctx_size);
                self.ctx_used = u["used"].as_u64().unwrap_or(self.ctx_used);
                // Hermes sends this after the commands, so a Hermes that sent none never will.
                self.commands.get_or_insert_with(Vec::new);
                self.answer_init();
            }
            "session_info_update" => {
                if let Some(title) = u["title"].as_str().filter(|t| !t.is_empty()) {
                    self.title = Some(title.to_owned());
                    self.record(json!({ "type": "ai-title", "aiTitle": title, "sessionId": self.session }));
                }
            }
            _ => {}
        }
    }

    /// Shows a todo list as a finished TodoWrite call, so the checklist and progress panel follow it.
    /// Drawn before Hermes reports the call's outcome, so an update Hermes rejects (an item without
    /// content, a model slip) shows as applied until the model's retry redraws the list.
    fn emit_todos(&mut self, message_id: String, id: &str, todos: Vec<Value>) {
        self.assistant(message_id, json!([todo_write_block(id, todos)]));
        let result = todo_result_line(&self.session, id);
        self.record(result.clone());
        self.emit(result);
    }

    /// Takes down the cards of requests Hermes no longer waits on, and answers them so nothing
    /// in Hermes is left waiting either.
    fn dismiss(&mut self, gone: impl Fn(&Permission) -> bool) {
        let ids: Vec<String> = self.permissions.iter().filter(|(_, p)| gone(p)).map(|(r, _)| r.clone()).collect();
        for request_id in ids {
            let Some(p) = self.permissions.remove(&request_id) else { continue };
            self.send_hermes(json!({ "jsonrpc": "2.0", "id": p.rpc_id, "result": { "outcome": { "outcome": "cancelled" } } }));
            self.emit(json!({ "type": "control_cancel_request", "request_id": request_id }));
        }
    }

    fn start_reply(&mut self, id: Option<String>) {
        let id = id.unwrap_or_else(|| {
            self.reply_seq += 1;
            format!("hermes-msg-{}", self.reply_seq)
        });
        self.reply.id = Some(id.clone());
        let base = json!({ "type": "stream_event", "parent_tool_use_id": null, "session_id": self.session });
        let mut start = base.clone();
        start["event"] = json!({ "type": "message_start", "message": { "id": id, "role": "assistant", "model": self.model } });
        self.emit(start);
        let mut block = base;
        block["event"] = json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } });
        self.emit(block);
    }

    /// Sends the reply streamed so far as one finished assistant message.
    fn flush(&mut self) {
        let reply = std::mem::take(&mut self.reply);
        let mut content = Vec::new();
        // Each flush is one model call, so its thought is a block of its own; Hermes pads both
        // the thought and the answer with blank lines.
        let (thinking, text) = (reply.thinking.trim(), reply.text.trim());
        if !thinking.is_empty() {
            content.push(json!({ "type": "thinking", "thinking": thinking }));
        }
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
        if content.is_empty() {
            return;
        }
        let id = reply.id.unwrap_or_else(|| {
            self.reply_seq += 1;
            format!("hermes-msg-{}", self.reply_seq)
        });
        self.assistant(id, Value::Array(content));
    }

    fn assistant(&mut self, id: String, content: Value) {
        let msg = json!({
            "type": "assistant", "session_id": self.session, "parent_tool_use_id": null,
            "message": {
                "id": id, "type": "message", "role": "assistant", "model": self.model, "content": content,
                "usage": { "input_tokens": self.ctx_used },
            },
        });
        self.record(msg.clone());
        self.emit(msg);
    }

    /// The mode the cell shows: a held change's, as the cell chose it, so a turn starting
    /// meanwhile does not set its picker back.
    fn shown_mode(&self) -> &str {
        self.held.iter().rev().find_map(|h| match &h.then {
            After::Mode(mode) => Some(mode.as_str()),
            After::Model(_) => None,
        }).unwrap_or(&self.mode)
    }

    /// Hermes reported on a turn it started itself; the first report shows the cell working.
    fn note_own_turn(&mut self) {
        let now = Instant::now();
        match &mut self.own_turn {
            Some((_, last)) => *last = now,
            None => {
                self.own_turn = Some((now, now));
                self.emit(json!({
                    "type": "system", "subtype": "init", "session_id": self.session, "model": self.model,
                    "permissionMode": claude_mode(self.shown_mode()), "cwd": self.opts.cwd,
                }));
            }
        }
    }

    /// Ends a turn Hermes started itself once it has gone quiet. A tool still running or a
    /// question still open is not quiet, however long it takes.
    fn own_turn_quiet(&mut self) {
        let Some((started, last)) = self.own_turn else { return };
        if !self.tools.is_empty() || !self.permissions.is_empty() {
            self.own_turn = Some((started, Instant::now()));
        } else if last.elapsed() >= OWN_TURN_QUIET {
            self.turn_ended(started, "end_turn", &Value::Null, None);
        }
    }

    /// Whether a nudge can be written (see `TURN_NUDGE`): `None` otherwise, so a turn whose
    /// Hermes stdin is gone does not wake the loop for nothing.
    fn nudge_wait(&self) -> Option<Duration> {
        (self.in_turn && self.hermes.is_some()).then(|| (self.heard + TURN_NUDGE).saturating_duration_since(Instant::now()))
    }

    /// Writes Hermes a harmless request when a turn HQ sent has gone quiet (see `TURN_NUDGE`).
    fn turn_quiet(&mut self) {
        if self.nudge_wait() == Some(Duration::ZERO) {
            self.heard = Instant::now(); // once a `TURN_NUDGE`, whether or not Hermes answers
            // A Hermes that holds its output holds the answers too, so only the latest nudge is
            // kept: each write may be the one that frees it, and the calls stay bounded. A late
            // answer to a forgotten one matches no call and is dropped.
            self.calls.retain(|_, c| !matches!(c, Call::Nudge));
            // Narrowed to the cell's folder, so Hermes reads less of its catalog to answer.
            let params = json!({ "cwd": self.opts.cwd });
            self.call("session/list", params, Call::Nudge);
        }
    }

    /// Gives up on a session Hermes went silent while opening. What the cell sent meanwhile is
    /// dropped with it; the error tells the user to send it again in a new cell.
    fn open_quiet(&mut self) {
        let Some(since) = self.opening else { return };
        if since.elapsed() >= OPEN_QUIET {
            let secs = OPEN_QUIET.as_secs();
            self.fatal(&format!("Hermes did not open the session within {secs} s, so nothing you sent reached it. Close this cell and start a new one. Once it is closed, any error Hermes printed is in ~/.hq/hosts/host.log."));
        }
    }

    fn turn_ended(&mut self, started: Instant, stop: &str, usage: &Value, error: Option<&str>) {
        self.flush();
        self.in_turn = false;
        self.own_turn = None;
        let tokens = |k: &str| usage[k].as_u64().unwrap_or(0);
        self.totals.0 += tokens("inputTokens");
        self.totals.1 += tokens("outputTokens");
        self.totals.2 += tokens("cachedReadTokens");
        let mut model_usage = Map::new();
        model_usage.insert(self.model.clone(), json!({
            "contextWindow": self.ctx_size, "inputTokens": self.totals.0,
            "outputTokens": self.totals.1, "cacheReadInputTokens": self.totals.2,
        }));
        let duration = started.elapsed().as_millis() as u64;
        let (subtype, error) = match (stop, error) {
            (_, Some(error)) => ("error", Some(error)),
            // Like Claude's interrupt: an error the frontend does not report.
            ("cancelled", None) => ("error_during_execution", None),
            ("refusal", None) => ("error", Some("Hermes refused to continue this turn.")),
            _ => ("success", None),
        };
        let mut result = json!({
            "type": "result", "subtype": subtype, "is_error": subtype != "success", "session_id": self.session,
            "duration_ms": duration, "modelUsage": model_usage,
        });
        if let Some(error) = error {
            result["result"] = json!(error);
        }
        self.emit(result);
        // Cards for this turn are gone from the frontend; any still open in Hermes are void.
        for (_, p) in std::mem::take(&mut self.permissions) {
            self.send_hermes(json!({ "jsonrpc": "2.0", "id": p.rpc_id, "result": { "outcome": { "outcome": "cancelled" } } }));
        }
        self.tools.clear();
        self.terminal_commands.clear();
        self.turn_tools.clear();
        self.last_tool = None;
        // A Hermes that never listed its commands still answers initialize.
        self.commands.get_or_insert_with(Vec::new);
        self.answer_init();
        self.release();
    }

    /// Keeps `held` as the change to send once Hermes is idle. One of the same kind it replaces is
    /// answered as done: the cell has moved past it.
    fn hold(&mut self, held: Held) {
        let same = |h: &Held| matches!((&h.then, &held.then), (After::Model(_), After::Model(_)) | (After::Mode(_), After::Mode(_)));
        if let Some(i) = self.held.iter().position(same) {
            let old = self.held.remove(i);
            self.reply_ok(&old.request_id, json!({}));
        }
        self.held.push(held);
    }

    /// Gives up on held changes Hermes has not answered within `HELD_ANSWER`, failing them in the
    /// cell, and lets the queued prompts go. A late answer then matches no call and is dropped.
    fn held_unanswered(&mut self) {
        let Some(since) = self.applying_since else { return };
        if self.applying > 0 && since.elapsed() < HELD_ANSWER {
            return;
        }
        let lost: Vec<u64> = self.calls.iter().filter(|(_, c)| matches!(c, Call::Held(_))).map(|(id, _)| *id).collect();
        for id in lost {
            if let Some(Call::Held(held)) = self.calls.remove(&id) {
                self.reply_err(&held.request_id, "Hermes did not answer the change; try it again");
            }
        }
        self.applying = 0;
        self.applying_since = None;
        self.release();
    }

    /// Once Hermes is idle, sends the held changes, then (once they are answered) the next queued prompt.
    fn release(&mut self) {
        let waiting = self.held_retry.is_some_and(|at| at > Instant::now());
        if self.in_turn || self.own_turn.is_some() || self.applying > 0 || waiting {
            return;
        }
        self.held_retry = None;
        self.applying_since = None;
        let Some(session) = self.session.clone() else { return };
        if !self.held.is_empty() {
            self.applying_since = Some(Instant::now());
            for held in std::mem::take(&mut self.held) {
                let (method, params) = match &held.then {
                    After::Model(model) => ("session/set_model", json!({ "sessionId": session, "modelId": model })),
                    After::Mode(mode) => ("session/set_mode", json!({ "sessionId": session, "modeId": mode })),
                };
                self.applying += 1;
                self.call(method, params, Call::Held(held));
            }
            return;
        }
        if let Some((prompt, content)) = self.prompts.pop_front() {
            self.send_prompt(prompt, content);
        }
    }

    // ---------- from the frontend ----------

    fn frontend(&mut self, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        match msg["type"].as_str().unwrap_or_default() {
            "user" => self.prompt(&msg),
            "control_request" => {
                let request_id = msg["request_id"].as_str().unwrap_or_default().to_owned();
                self.control(&request_id, &msg["request"]);
            }
            "control_response" => self.permission_answer(&msg["response"]),
            _ => {}
        }
    }

    fn prompt(&mut self, msg: &Value) {
        let content = &msg["message"]["content"];
        let mut blocks = Vec::new();
        match content {
            Value::String(text) => blocks.push(json!({ "type": "text", "text": slash_command(text) })),
            Value::Array(items) => {
                for item in items {
                    match item["type"].as_str() {
                        Some("text") => blocks.push(json!({ "type": "text", "text": item["text"] })),
                        Some("image") => blocks.push(json!({ "type": "image", "mimeType": item["source"]["media_type"], "data": item["source"]["data"] })),
                        _ => {}
                    }
                }
            }
            _ => return,
        }
        if blocks.is_empty() {
            return;
        }
        let prompt = json!({ "sessionId": self.session, "prompt": blocks });
        // Also behind a held model or mode change, which Hermes must take before the next turn.
        if self.in_turn || self.applying > 0 || !self.held.is_empty() {
            self.prompts.push_back((prompt, content.clone()));
        } else {
            self.send_prompt(prompt, content.clone());
        }
    }

    /// Recorded only now, so HQ's copy keeps the order Hermes saw and leaves out prompts an interrupt dropped.
    fn send_prompt(&mut self, mut prompt: Value, content: Value) {
        // Like Claude, each turn starts with system/init; the cell shows it is working from here.
        let Some(id) = self.session.clone() else { return };
        // Only to Hermes: HQ's copy keeps the prompt as the user wrote it. Added here rather than
        // when queued, so a queued prompt an interrupt drops does not take the note with it.
        if self.task_prompt_due {
            if let Some(blocks) = prompt["prompt"].as_array_mut() {
                self.task_prompt_due = !add_task_prompt(blocks);
            }
        }
        self.emit(json!({
            "type": "system", "subtype": "init", "session_id": id, "model": self.model,
            "permissionMode": claude_mode(self.shown_mode()), "cwd": self.opts.cwd,
        }));
        self.record(json!({ "type": "user", "message": { "role": "user", "content": content } }));
        self.in_turn = true;
        self.heard = Instant::now(); // the turn's quiet counts from its start, not from the idle before it
        // A turn Hermes started itself runs ahead of this prompt. Hermes' gateway queues the
        // prompt behind it and replies only once the prompt has run, so that reply ends both.
        self.own_turn = None;
        self.turn_prompt = prompt_text(&content);
        self.turn_tools.clear();
        self.call("session/prompt", prompt.clone(), Call::Prompt { started: Instant::now(), prompt, retried: false });
    }

    /// Answers the frontend's initialize requests once Hermes has listed its commands.
    fn answer_init(&mut self) {
        let Some(commands) = self.commands.as_ref() else { return };
        let commands: Vec<Value> = commands.iter().map(|c| json!({
            "name": c["name"], "description": c["description"], "argumentHint": c["input"]["hint"], "aliases": [],
        })).collect();
        let mut models = vec![json!({
            "value": "default", "displayName": "Default",
            "description": format!("Hermes' configured model ({})", self.default_model),
            "resolvedModel": self.default_model,
        })];
        models.extend(self.models.iter().map(|m| json!({
            "value": m["modelId"], "displayName": m["name"], "description": m["description"], "resolvedModel": m["modelId"],
        })));
        let response = json!({ "models": models, "commands": commands, "remote_control_available": false });
        for request_id in std::mem::take(&mut self.waiting_init) {
            self.reply_ok(&request_id, response.clone());
        }
    }

    fn control(&mut self, request_id: &str, req: &Value) {
        let session = self.session.clone();
        match req["subtype"].as_str().unwrap_or_default() {
            "initialize" => {
                self.waiting_init.push(request_id.to_owned());
                self.answer_init();
            }
            "get_settings" => {
                let response = json!({ "applied": { "model": self.model } });
                self.reply_ok(request_id, response);
            }
            "interrupt" => {
                // Its approval cards stay: denying one is the way left to end it.
                if self.own_turn.is_some() {
                    return self.reply_err(request_id, OWN_TURN_STOP);
                }
                self.prompts.clear();
                self.dismiss(|_| true); // as ACP asks: open permission requests end with the turn
                if self.in_turn {
                    self.send_hermes(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session } }));
                }
                // Side questions run on, as Claude's do: Stop is for the conversation.
                self.reply_ok(request_id, json!({}));
            }
            "set_model" => {
                let model = match req["model"].as_str().unwrap_or("default") {
                    "default" => self.default_model.clone(),
                    m => m.to_owned(),
                };
                if model.is_empty() {
                    return self.reply_err(request_id, "Hermes reported no default model");
                }
                // Answered once Hermes takes it, which it does only between turns (see `Held`).
                self.hold(Held { request_id: request_id.into(), then: After::Model(model), tries: 0 });
                self.release();
            }
            "set_permission_mode" => match req["mode"].as_str().and_then(hermes_mode) {
                Some(mode) => {
                    self.hold(Held { request_id: request_id.into(), then: After::Mode(mode.into()), tries: 0 });
                    self.release();
                }
                None => self.reply_err(request_id, "Hermes has no such mode"),
            },
            "get_context_usage" => {
                let percent = if self.ctx_size > 0 { self.ctx_used as f64 * 100.0 / self.ctx_size as f64 } else { 0.0 };
                let response = json!({
                    "totalTokens": self.ctx_used, "maxTokens": self.ctx_size, "percentage": percent,
                    "model": self.model, "categories": [],
                });
                self.reply_ok(request_id, response);
            }
            "generate_session_title" => match self.title.clone() {
                Some(title) => self.reply_ok(request_id, json!({ "title": title })),
                None => self.reply_err(request_id, "Hermes has not titled this session yet"),
            },
            // The rename lives in HQ's board; Hermes has no rename over ACP.
            "rename_session" => self.reply_ok(request_id, json!({ "title": req["title"] })),
            "side_question" => {
                // Asked in a throwaway fork, so the question and answer stay out of the conversation.
                let mut question = req["question"].as_str().unwrap_or_default().to_owned();
                if self.in_turn {
                    // The fork has only the persisted history, which lacks the running turn.
                    question = side_prompt(&question, &self.turn_prompt, &self.turn_tools, if self.todos_known { &self.todos } else { &NO_TODOS });
                }
                let params = json!({ "sessionId": session, "cwd": self.opts.cwd, "mcpServers": [] });
                self.call("session/fork", params, Call::SideFork { request_id: request_id.into(), question });
            }
            other => self.reply_err(request_id, &format!("Hermes does not support {other}")),
        }
    }

    fn permission_answer(&mut self, response: &Value) {
        let Some(request_id) = response["request_id"].as_str() else { return };
        let Some(p) = self.permissions.remove(request_id) else { return };
        let decision = &response["response"];
        let allow = response["subtype"] == "success" && decision["behavior"] == "allow";
        let remember = decision["updatedPermissions"].as_array().is_some_and(|a| !a.is_empty());
        let outcome = match pick_option(&p.options, allow, remember) {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.send_hermes(json!({ "jsonrpc": "2.0", "id": p.rpc_id, "result": { "outcome": outcome } }));
    }
}

/// Whether Hermes refused a change only because the session is mid-turn: its friendly wording, or
/// a bare "Internal error" whose details say so.
fn is_busy(error: &str, details: Option<&str>) -> bool {
    details == Some("session_busy") || error.contains("when the session is idle")
}

/// Hermes' error with its details, which a bare "Internal error" otherwise hides.
fn rpc_error(error: &str, details: Option<&str>) -> String {
    match details.filter(|d| !d.is_empty() && !error.contains(d)) {
        Some(details) => format!("{error} ({details})"),
        None => error.to_owned(),
    }
}

/// The option that matches the user's answer, by id first and then by kind.
fn pick_option(options: &[(String, String)], allow: bool, remember: bool) -> Option<String> {
    let wanted: &[&str] = match (allow, remember) {
        // Never `allow_always`: that is a permanent allowlist entry, and HQ offers "this session".
        (true, true) => &["allow_session", "allow_once"],
        (true, false) => &["allow_once"],
        (false, _) => &["deny"],
    };
    let kind = if allow { "allow_once" } else { "reject_once" };
    wanted
        .iter()
        .find_map(|w| options.iter().find(|(id, _)| id == w))
        .or_else(|| options.iter().find(|(_, k)| k == kind))
        .map(|(id, _)| id.clone())
}

/// HQ's permission modes (Claude's names) as Hermes modes. Hermes has no plan mode.
fn hermes_mode(mode: &str) -> Option<&'static str> {
    match mode {
        "default" => Some("default"),
        "acceptEdits" | "accept_edits" => Some("accept_edits"),
        "bypassPermissions" | "dontAsk" | "dont_ask" => Some("dont_ask"),
        _ => None,
    }
}

fn claude_mode(mode: &str) -> &'static str {
    match mode {
        "accept_edits" => "acceptEdits",
        "dont_ask" => "bypassPermissions",
        _ => "default",
    }
}

/// HQ's compact button sends Claude's `/compact`; Hermes calls it `/compress`.
fn slash_command(text: &str) -> String {
    match text.strip_prefix("/compact") {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => format!("/compress{rest}"),
        _ => text.to_owned(),
    }
}

/// Adds `TASK_PROMPT` to a prompt's blocks, and returns whether it did: not to a slash command,
/// which Hermes only runs as the whole prompt, nor beside the composer's one-off nudge, which
/// leaves it for the next prompt.
fn add_task_prompt(blocks: &mut Vec<Value>) -> bool {
    let mut texts = blocks.iter().filter_map(|b| b["text"].as_str());
    if texts.clone().any(|t| t.starts_with(TASK_HINT_PREFIX)) {
        return false;
    }
    let first = texts.next().unwrap_or_default().split_whitespace().next().unwrap_or_default();
    if first.strip_prefix('/').is_some_and(|name| !name.is_empty() && !name.contains('/')) {
        return false;
    }
    blocks.push(json!({ "type": "text", "text": TASK_PROMPT }));
    true
}

/// A Hermes tool call as a Claude tool name and input, so the frontend draws it the same way:
/// shell commands as Bash, edits with a diff as Edit or Write, and anything else under its own name.
fn tool_use(call: &Value) -> (String, Value) {
    let title = call["title"].as_str().unwrap_or_default();
    let (prefix, rest) = title.split_once(": ").unwrap_or((title, ""));
    let content = call["content"].as_array().map(Vec::as_slice).unwrap_or_default();
    if let Some(diff) = content.iter().find(|c| c["type"] == "diff") {
        let path = diff["path"].clone();
        return match diff["oldText"].as_str() {
            Some(old) => ("Edit".into(), json!({ "file_path": path, "old_string": old, "new_string": diff["newText"] })),
            None => ("Write".into(), json!({ "file_path": path, "content": diff["newText"] })),
        };
    }
    let raw = &call["rawInput"];
    if call["kind"] == "execute" {
        let command = raw["command"].as_str().unwrap_or(rest);
        return ("Bash".into(), json!({ "command": command, "description": title }));
    }
    let name = if prefix.is_empty() { "Tool" } else { prefix };
    let input = match raw {
        Value::Object(_) => raw.clone(),
        _ => match call["locations"][0]["path"].as_str() {
            Some(path) => json!({ "path": path, "detail": rest }),
            None => json!({ "detail": if rest.is_empty() { title } else { rest } }),
        },
    };
    (name.to_owned(), input)
}

/// The command an open tool call is running, if it is Hermes' terminal tool, which streams its
/// command in full as `$ <command>` before it asks about it.
fn terminal_command(call: &Value) -> Option<String> {
    if call["kind"] != "execute" || call["title"].as_str().is_none_or(|t| t != "terminal" && !t.starts_with("terminal:")) {
        return None;
    }
    let text = call["content"][0]["content"]["text"].as_str()?;
    text.strip_prefix("$ ").filter(|c| !c.is_empty()).map(str::to_owned)
}

/// Whether the reviewer (review.rs) may judge this command. Every Hermes request that is not an
/// edit arrives as `kind: execute` with a `command`, whether it is a real command or a sentence
/// dressed as one (consent for an MCP tool, a standing event trigger or a saved payment card, a
/// Python script, a computer-use summary). A real one is what the terminal call now open is running,
/// word for word. Offering "always" then leaves out overriding a Smart DENY, which is for a person.
/// A command Hermes redacted no longer matches, so it stays with the user.
fn real_command(call: &Value, options: &[(String, String)], open: &HashMap<String, String>) -> bool {
    call["kind"] == "execute"
        && options.iter().any(|(o, _)| o == "allow_always")
        && call["rawInput"]["command"].as_str().is_some_and(|command| !command.starts_with("execute_code ") && open.values().any(|c| c == command))
}

/// Whether the reviewer may judge this edit. The path must be one the agent gave in full: Hermes
/// resolves the path it shows from its own folder, and the path it writes from the terminal's, so
/// after a `cd` a relative path shows one file and writes another. See also `is_patch`. Not covered:
/// on a Mac with Hermes' ssh or docker terminal backend, an absolute path is written remotely or in
/// the container, not on this machine.
fn plain_edit(call: &Value) -> bool {
    let Some(content) = call["content"].as_array() else { return false };
    let raw = &call["rawInput"];
    let diffs: Vec<&Value> = content.iter().filter(|c| c["type"] == "diff").collect();
    let ([diff], true) = (diffs.as_slice(), content.len() == 1) else { return false };
    let tool_known = raw["tool"] == "write_file" || raw["tool"] == "patch";
    match (diff["path"].as_str(), raw["arguments"]["path"].as_str()) {
        (Some(shown), Some(given)) => tool_known && !is_patch(call) && is_absolute(given) && same_path(shown, given),
        _ => false,
    }
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if cfg!(windows) {
        bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\')
    } else {
        path.starts_with('/')
    }
}

fn same_path(a: &str, b: &str) -> bool {
    let plain = |p: &str| if cfg!(windows) { p.replace('\\', "/").to_lowercase() } else { p.to_owned() };
    plain(a) == plain(b)
}

/// A V4A patch names its own files, and Hermes lists them for approval with a stricter pattern than
/// the one that applies the patch: `***Add File: x` is applied but not listed. So the path shown
/// cannot be trusted for a patch.
fn is_patch(call: &Value) -> bool {
    if call["rawInput"]["arguments"]["mode"] == "patch" || call["rawInput"]["mode"] == "patch" {
        return true;
    }
    call["content"].as_array().is_some_and(|content| {
        content.iter().filter(|c| c["type"] == "diff").any(|diff| {
            diff["newText"].as_str().is_some_and(|text| {
                text.lines().any(|line| {
                    let line = line.trim_start();
                    line.starts_with("***") && (line.contains("File:") || line.contains("Begin Patch"))
                })
            })
        })
    })
}

/// A finished tool call's output as plain text.
fn tool_text(update: &Value) -> String {
    let mut parts = Vec::new();
    for c in update["content"].as_array().map(Vec::as_slice).unwrap_or_default() {
        match c["type"].as_str() {
            Some("content") => parts.extend(c["content"]["text"].as_str().map(str::to_owned)),
            Some("diff") => parts.push(format!("Edited {}", c["path"].as_str().unwrap_or("a file"))),
            _ => {}
        }
    }
    if parts.is_empty() {
        if let Some(raw) = update.get("rawOutput").filter(|r| !r.is_null()) {
            parts.push(raw.as_str().map_or_else(|| raw.to_string(), str::to_owned));
        }
    }
    parts.join("\n")
}

/// Hermes' todo list as it stands: (item id, item), in Hermes' order.
type Todos = Vec<(String, Value)>;

/// What a side question is told of a list not yet known in full.
static NO_TODOS: Todos = Vec::new();

/// Applies a `todo_list` tool call (rawInput `{todos: [...], merge?: bool}`) to the list: a merge
/// updates items by id and adds new ones, anything else replaces the list. Returns whether the
/// call was a todo update at all; a bare `todo_list` call only reads the list.
fn merge_todo_call(list: &mut Todos, call: &Value) -> bool {
    // Hermes titles a call "<tool>: <what>"; another tool taking a `todos` argument is not this one.
    let title = call["title"].as_str().unwrap_or_default();
    if title != "todo_list" && !title.starts_with("todo_list:") {
        return false;
    }
    let raw = &call["rawInput"];
    let Some(items) = raw["todos"].as_array() else { return false };
    if raw["merge"].as_bool() != Some(true) {
        list.clear();
    }
    for item in items {
        let Some(fields) = item.as_object() else { continue };
        let id = match &item["id"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => item["content"].as_str().unwrap_or_default().to_owned(),
        };
        match list.iter_mut().find(|(i, _)| *i == id) {
            // A merge may carry only what changed.
            Some((_, old)) => {
                if let Some(old) = old.as_object_mut() {
                    old.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
            }
            // A merge naming an item this list never saw (a session HQ did not load) has nothing to show.
            None if item["content"].as_str().is_some_and(|c| !c.trim().is_empty()) => list.push((id, item.clone())),
            None => {}
        }
    }
    true
}

/// The list as TodoWrite items. The UI only counts completed, in_progress and pending, so a
/// cancelled item counts as done.
fn todo_items(list: &Todos) -> Vec<Value> {
    list.iter()
        .map(|(_, t)| {
            let status = match t["status"].as_str() {
                Some("cancelled") => "completed",
                Some(s) => s,
                None => "pending",
            };
            json!({ "content": t["content"], "activeForm": t["content"], "status": status })
        })
        .collect()
}

/// The synthetic TodoWrite call standing in for a todo update, and its result.
fn todo_write_block(id: &str, todos: Vec<Value>) -> Value {
    json!({ "type": "tool_use", "id": id, "name": "TodoWrite", "input": { "todos": todos } })
}

fn todo_result_line(session: &Option<String>, id: &str) -> Value {
    json!({
        "type": "user", "session_id": session, "parent_tool_use_id": null,
        "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": "Todos updated" }] },
    })
}

/// At most `max` characters of `s`.
fn clip(s: &str, max: usize) -> &str {
    s.char_indices().nth(max).map_or(s, |(i, _)| &s[..i])
}

/// The text of a frontend prompt (a string, or content blocks).
fn prompt_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter(|i| i["type"] == "text").filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A side question asked while a turn runs. The fork that answers it sees only the persisted
/// history, which lacks the running turn, so the turn is described in front of the question.
fn side_prompt(question: &str, prompt: &str, tools: &VecDeque<String>, todos: &Todos) -> String {
    let mut out = String::new();
    if !prompt.trim().is_empty() {
        let clipped = clip(prompt, 2000);
        let dots = if clipped.len() < prompt.len() { "..." } else { "" };
        out.push_str(&format!("The user's current request (still running) is: \"{clipped}{dots}\". "));
    } else {
        out.push_str("A request is still running. ");
    }
    if !tools.is_empty() {
        out.push_str("Tools run so far this turn: ");
        for (n, t) in tools.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            out.push_str(t);
        }
        out.push_str(". ");
    }
    if !todos.is_empty() {
        out.push_str("Current todo list: ");
        for (n, (_, t)) in todos.iter().enumerate() {
            if n > 0 {
                out.push_str("; ");
            }
            out.push_str(&format!("{} [{}]", t["content"].as_str().unwrap_or_default(), t["status"].as_str().unwrap_or("pending")));
        }
        out.push_str(". ");
    }
    out.push_str("\n\n");
    out.push_str(question);
    out
}

/// The current UTC time as Claude's transcripts write it, e.g. `2026-10-08T08:15:02.123Z`.
fn now_iso() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600, rem % 3_600 / 60, rem % 60, now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that is absolute on this machine, as the agent would give it.
    fn full_path(name: &str) -> String {
        if cfg!(windows) { format!("F:/proj/{name}") } else { format!("/proj/{name}") }
    }

    /// The shape of `build_acp_edit_tool_call` in Hermes' edit_approval.py.
    fn edit_call(tool: &str, given: &str, shown: &str, text: &str) -> Value {
        json!({ "title": "Approve edit", "kind": "edit",
            "content": [{ "type": "diff", "path": shown, "newText": text }],
            "rawInput": { "tool": tool, "arguments": { "path": given } } })
    }

    #[test]
    fn only_the_terminal_tool_streams_a_command() {
        let call = |title: &str, kind: &str, text: &str| json!({ "title": title, "kind": kind, "content": [{ "type": "content", "content": { "type": "text", "text": text } }] });
        assert_eq!(terminal_command(&call("terminal: ls", "execute", "$ ls")).as_deref(), Some("ls"));
        assert_eq!(terminal_command(&call("terminal", "execute", "$ ls")).as_deref(), Some("ls"));
        assert_eq!(terminal_command(&call("terminal: ls + 1 command", "execute", "$ ls\necho hi")).as_deref(), Some("ls\necho hi"));
        assert_eq!(terminal_command(&call("execute_code: x", "execute", "$ ls")), None);
        assert_eq!(terminal_command(&call("process: x", "execute", "$ ls")), None);
        assert_eq!(terminal_command(&call("terminal: ls", "read", "$ ls")), None);
        assert_eq!(terminal_command(&call("terminal: ls", "execute", "ls")), None);
        assert_eq!(terminal_command(&json!({ "title": "terminal: ls", "kind": "execute" })), None);
    }

    #[test]
    fn a_real_command_is_running_word_for_word_and_offers_always() {
        let always = options(&[("allow_once", "allow_once"), ("allow_always", "allow_always"), ("deny", "reject_once")]);
        let once = options(&[("allow_once", "allow_once"), ("deny", "reject_once")]);
        let open: HashMap<String, String> = [("t1".to_owned(), "cargo test".to_owned())].into();
        let gate = |command: &str| json!({ "kind": "execute", "rawInput": { "command": command } });
        assert!(real_command(&gate("cargo test"), &always, &open));
        assert!(!real_command(&gate("cargo test"), &once, &open), "a Smart DENY override");
        assert!(!real_command(&gate("cargo test --all"), &always, &open));
        assert!(!real_command(&gate("cargo test"), &always, &HashMap::new()));
        assert!(!real_command(&json!({ "kind": "edit", "rawInput": { "command": "cargo test" } }), &always, &open));
        assert!(!real_command(&json!({ "kind": "execute" }), &always, &open));
        let script: HashMap<String, String> = [("t1".to_owned(), "execute_code <<'PY'\nx\nPY".to_owned())].into();
        assert!(!real_command(&gate("execute_code <<'PY'\nx\nPY"), &always, &script), "the code gate's own form");
    }

    #[test]
    fn only_a_plain_edit_is_fit_for_the_reviewer() {
        let path = full_path("a.rs");
        assert!(plain_edit(&edit_call("write_file", &path, &path, "b")));
        assert!(plain_edit(&edit_call("patch", &path, &path, "b")));
        for call in [
            json!({ "title": "x", "kind": "execute", "rawInput": { "command": "ls" } }),
            json!({ "title": "terminal: ls", "kind": "execute" }),
            json!({ "title": "Subscribe to MCP event", "kind": "execute", "rawInput": { "command": "Subscribe to MCP event 'x'?" } }),
            json!({ "title": "read: a", "rawInput": { "path": "a" } }),
            json!({ "title": "x", "content": [{ "type": "diff", "path": path, "newText": "b" }] }),
        ] {
            assert!(!plain_edit(&call), "{call}");
        }
    }

    #[test]
    fn a_path_hermes_resolved_for_itself_is_not_trusted() {
        let shown = full_path("a.rs");
        // After `cd ../other`, Hermes shows this and writes somewhere else.
        assert!(!plain_edit(&edit_call("write_file", "a.rs", &shown, "b")));
        assert!(!plain_edit(&edit_call("write_file", "./a.rs", &shown, "b")));
        assert!(!plain_edit(&edit_call("write_file", "~/a.rs", &shown, "b")));
        assert!(!plain_edit(&edit_call("write_file", &full_path("other.rs"), &shown, "b")));
        assert!(!plain_edit(&edit_call("other_tool", &shown, &shown, "b")));
    }

    #[test]
    fn an_edit_beside_other_content_is_not_plain() {
        let path = full_path("a.rs");
        let mut call = edit_call("write_file", &path, &path, "b");
        call["content"].as_array_mut().unwrap().push(json!({ "type": "content", "content": { "type": "text", "text": "x" } }));
        assert!(!plain_edit(&call));
    }

    #[test]
    fn a_patch_is_never_reviewable_as_the_path_it_shows() {
        let path = full_path("a.rs");
        for text in ["*** Begin Patch\n*** Update File: a\n", "@@\n-x\n+y\n***Add File: C:/x.bat\n+calc\n", "  ***  Delete File: b\n"] {
            assert!(!plain_edit(&edit_call("patch", &path, &path, text)), "{text}");
        }
        let mut by_mode = edit_call("patch", &path, &path, "b");
        by_mode["rawInput"]["arguments"]["mode"] = json!("patch");
        assert!(!plain_edit(&by_mode));
        assert!(plain_edit(&edit_call("write_file", &path, &path, "*** a heading\nFile: b\n")));
    }

    #[test]
    fn the_request_marks_only_what_it_can_vouch_for() {
        let (mut child, mut hermes, input, rx) = scripted();
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 0, "result": {} }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 1, "result": { "sessionId": "s1" } }));
        let options = json!([{ "optionId": "allow_once", "kind": "allow_once" }, { "optionId": "deny", "kind": "reject_once" }]);
        let path = full_path("a.rs");
        let ask = |id: u32, call: Value| json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": { "sessionId": "s1", "options": options, "toolCall": call } });
        hermes_says(&mut hermes, ask(50, edit_call("write_file", &path, &path, "b")));
        let edit = next_matching(&rx, "the edit's card", |v| v["type"] == "control_request");
        assert_eq!(edit["request"]["hermes_reviewable"], true);
        assert!(edit["request"].get("hermes_flag").is_none());
        hermes_says(&mut hermes, ask(51, json!({ "title": "x", "kind": "execute", "rawInput": { "command": "ls" } })));
        let command = next_matching(&rx, "the command's card", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-51");
        assert!(command["request"].get("hermes_reviewable").is_none(), "no terminal call is running it");

        let always = json!([{ "optionId": "allow_once", "kind": "allow_once" }, { "optionId": "allow_session", "kind": "allow_always" }, { "optionId": "allow_always", "kind": "allow_always" }, { "optionId": "deny", "kind": "reject_once" }]);
        let ask_always = |id: u32, call: Value| json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": { "sessionId": "s1", "options": always, "toolCall": call } });
        let update = |u: Value| json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s1", "update": u } });
        let gate = |command: &str| json!({ "title": "recursive delete", "kind": "execute", "rawInput": { "command": command, "description": "recursive delete" } });
        hermes_says(&mut hermes, ask_always(52, gate("rm -r build")));
        let early = next_matching(&rx, "a card before the call", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-52");
        assert!(early["request"].get("hermes_reviewable").is_none());
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t9", "kind": "execute", "title": "terminal: rm -r build", "content": [{ "type": "content", "content": { "type": "text", "text": "$ rm -r build" } }] })));
        next_matching(&rx, "the call", |v| v["type"] == "assistant");
        hermes_says(&mut hermes, ask_always(53, gate("rm -r build")));
        let real = next_matching(&rx, "the real command's card", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-53");
        assert_eq!(real["request"]["hermes_reviewable"], true);
        assert_eq!(real["request"]["hermes_flag"], "recursive delete");
        hermes_says(&mut hermes, ask_always(54, gate("Subscribe to MCP event 'x'?")));
        let consent = next_matching(&rx, "a consent card", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-54");
        assert!(consent["request"].get("hermes_reviewable").is_none());
        assert!(consent["request"].get("hermes_flag").is_none());
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t9", "status": "completed" })));
        next_matching(&rx, "the call's result", |v| v["type"] == "user");
        hermes_says(&mut hermes, ask_always(55, gate("rm -r build")));
        let after = next_matching(&rx, "a card after the call", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-55");
        assert!(after["request"].get("hermes_reviewable").is_none(), "the call has ended");

        // A matching terminal call is open again, but the session has now delegated.
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t10", "kind": "execute", "title": "terminal: rm -r build", "content": [{ "type": "content", "content": { "type": "text", "text": "$ rm -r build" } }] })));
        next_matching(&rx, "the second call", |v| v["type"] == "assistant");
        hermes_says(&mut hermes, ask_always(56, gate("rm -r build")));
        let open_again = next_matching(&rx, "a card before delegating", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-56");
        assert_eq!(open_again["request"]["hermes_reviewable"], true);
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t11", "kind": "other", "title": "delegate_task: do a thing" })));
        next_matching(&rx, "the delegation", |v| v["type"] == "assistant");
        hermes_says(&mut hermes, ask_always(57, gate("rm -r build")));
        let delegated = next_matching(&rx, "a card after delegating", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-57");
        assert!(delegated["request"].get("hermes_reviewable").is_none(), "could be the child's");
        assert!(delegated["request"].get("hermes_flag").is_none());
        let path = full_path("a.rs");
        hermes_says(&mut hermes, ask(58, edit_call("write_file", &path, &path, "b")));
        let edit_after = next_matching(&rx, "an edit after delegating", |v| v["type"] == "control_request" && v["request_id"] == "hermes-perm-58");
        assert_eq!(edit_after["request"]["hermes_reviewable"], true, "an edit shows its own path and content");
        drop(hermes);
        drop(input);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// What Hermes sends for a patch of several files; review.rs must see the list as a list.
    #[test]
    fn a_multi_file_patch_keeps_hermes_comma_joined_path() {
        let (_, input) = tool_use(&json!({ "title": "patch", "content": [{ "type": "diff", "path": "a.rs, b.rs", "newText": "x" }] }));
        assert_eq!(input["file_path"], "a.rs, b.rs");
    }

    /// A translator on a scripted Hermes: what the test writes is Hermes' output, and what the
    /// translator sends Hermes goes to a process that only reads it.
    fn scripted() -> (Child, PipeWriter, mpsc::Sender<String>, mpsc::Receiver<Value>) {
        let dir = std::env::temp_dir().join(format!("hq-hermes-scripted-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let opts = SpawnOptions {
            cwd: dir.to_string_lossy().into_owned(),
            resume: None,
            permission_mode: None,
            model: None,
            effort: None,
            fork: false,
            auto_tasks: false,
            max_agents: None,
            api_key: false,
            mcp: false,
            cell_key: None,
        };
        let mut sink = if cfg!(windows) { Command::new("findstr") } else { Command::new("cat") };
        if cfg!(windows) {
            sink.arg("^");
        }
        sink.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut sink);
        let mut child = sink.spawn().unwrap();
        let (hermes_out, hermes_in) = std::io::pipe().unwrap();
        let (input, lines) = translate(child.stdin.take().unwrap(), hermes_out, dir, opts, json!([])).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(lines).lines() {
                let Ok(line) = line else { break };
                if tx.send(serde_json::from_str(&line).expect("translator wrote invalid JSON")).is_err() {
                    break;
                }
            }
        });
        (child, hermes_in, input, rx)
    }

    fn hermes_says(out: &mut PipeWriter, v: Value) {
        writeln!(out, "{v}").unwrap();
    }

    fn next_matching(rx: &mpsc::Receiver<Value>, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = rx.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if pred(&v) {
                return v;
            }
        }
    }

    #[test]
    fn a_turn_hermes_started_itself_refuses_stop_and_ends_when_quiet() {
        let (mut child, mut hermes, input, rx) = scripted();
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 0, "result": {} }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 1, "result": { "sessionId": "s1" } }));
        let update = |u: Value| json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s1", "update": u } });
        // A background process's notice starts a turn HQ never sent.
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "execute", "title": "terminal: ping", "rawInput": { "command": "ping" } })));
        let init = next_matching(&rx, "system/init", |v| v["type"] == "system");
        assert_eq!(init["session_id"], "s1");

        input.send(json!({ "type": "control_request", "request_id": "stop", "request": { "subtype": "interrupt" } }).to_string()).unwrap();
        let reply = next_matching(&rx, "the interrupt's reply", |v| v["type"] == "control_response");
        assert_eq!(reply["response"]["subtype"], "error", "{reply}");
        assert_eq!(reply["response"]["error"], OWN_TURN_STOP);

        // While its tool runs, the turn is not over, however quiet.
        std::thread::sleep(OWN_TURN_QUIET * 3);
        assert!(rx.try_iter().all(|v| v["type"] != "result"), "ended while a tool still ran");
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [] })));
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Done." } })));
        let result = next_matching(&rx, "result", |v| v["type"] == "result");
        assert_eq!(result["subtype"], "success", "{result}");

        // News of a tool already closed starts no turn; with none left, Stop goes to Hermes as before.
        hermes_says(&mut hermes, update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [] })));
        std::thread::sleep(Duration::from_millis(100));
        input.send(json!({ "type": "control_request", "request_id": "stop2", "request": { "subtype": "interrupt" } }).to_string()).unwrap();
        let reply = next_matching(&rx, "the second interrupt's reply", |v| v["type"] == "control_response");
        assert_eq!(reply["response"]["subtype"], "success", "{reply}");
        drop(input);
        drop(hermes);
        let _ = child.kill();
    }

    #[test]
    fn a_side_question_hermes_will_not_fork_goes_to_a_fresh_session() {
        let (mut child, mut hermes, input, rx) = scripted();
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 0, "result": {} }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 1, "result": { "sessionId": "s1" } }));
        std::thread::sleep(Duration::from_millis(100));
        input.send(json!({ "type": "control_request", "request_id": "side", "request": { "subtype": "side_question", "question": "Sitrep?" } }).to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Mid-turn, Hermes refuses the fork; HQ opens a fresh session (id 3) instead.
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32603, "message": "Internal error", "data": { "details": "session_busy" } } }));
        std::thread::sleep(Duration::from_millis(100));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 3, "result": { "sessionId": "side1" } }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 4, "result": {} }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "side1", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "All on track." } } } }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 5, "result": { "stopReason": "end_turn" } }));
        let reply = next_matching(&rx, "the side answer", |v| v["type"] == "control_response" && v["response"]["request_id"] == "side");
        assert_eq!(reply["response"]["subtype"], "success", "{reply}");
        assert_eq!(reply["response"]["response"]["response"], "All on track.");
        drop(input);
        drop(hermes);
        let _ = child.kill();
    }

    #[test]
    fn a_mode_switch_mid_turn_waits_for_the_turn_and_the_next_prompt_waits_for_it() {
        let (mut child, mut hermes, input, rx) = scripted();
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 0, "result": {} }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 1, "result": { "sessionId": "s1" } }));
        std::thread::sleep(Duration::from_millis(100));
        let prompt = |text: &str| json!({ "type": "user", "message": { "role": "user", "content": text } }).to_string();
        input.send(prompt("first")).unwrap(); // id 2
        let mode = |id: &str, mode: &str| json!({ "type": "control_request", "request_id": id, "request": { "subtype": "set_permission_mode", "mode": mode } }).to_string();
        input.send(mode("m1", "acceptEdits")).unwrap();
        input.send(mode("m2", "bypassPermissions")).unwrap(); // replaces m1, which is answered as done
        input.send(prompt("second")).unwrap();
        let replaced = next_matching(&rx, "the replaced switch's answer", |v| v["type"] == "control_response");
        assert_eq!(replaced["response"]["request_id"], "m1", "{replaced}");
        assert_eq!(replaced["response"]["subtype"], "success", "{replaced}");
        // Nothing went to Hermes mid-turn: an answer to id 3 now matches no call and is dropped.
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 3, "error": { "code": -32603, "message": "Internal error", "data": { "details": "session_busy" } } }));
        std::thread::sleep(Duration::from_millis(100));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } }));
        next_matching(&rx, "the first turn's result", |v| v["type"] == "result");
        // The held switch is id 3 now. The second prompt waits for its answer: it would make Hermes busy again.
        std::thread::sleep(Duration::from_millis(200));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 3, "result": {} }));
        let switched = next_matching(&rx, "the held switch's answer", |v| v["type"] == "control_response");
        assert_eq!(switched["response"]["request_id"], "m2", "{switched}");
        assert_eq!(switched["response"]["subtype"], "success", "{switched}");
        let init = next_matching(&rx, "the second turn's start", |v| v["type"] == "system" && v["subtype"] == "init");
        assert_eq!(init["permissionMode"], "bypassPermissions", "{init}");
        drop(input);
        drop(hermes);
        let _ = child.kill();
    }

    #[test]
    fn a_busy_refused_switch_gives_way_to_a_newer_one_which_is_asked_again() {
        let (mut child, mut hermes, input, rx) = scripted();
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 0, "result": {} }));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 1, "result": { "sessionId": "s1" } }));
        std::thread::sleep(Duration::from_millis(100));
        let mode = |id: &str, mode: &str| json!({ "type": "control_request", "request_id": id, "request": { "subtype": "set_permission_mode", "mode": mode } }).to_string();
        input.send(mode("m1", "acceptEdits")).unwrap(); // idle: sent at once as id 2
        input.send(mode("m2", "default")).unwrap(); // held behind it
        std::thread::sleep(Duration::from_millis(100));
        // Hermes had started a turn of its own: m1 is refused as busy and m2 replaces it.
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32603, "message": "Internal error", "data": { "details": "session_busy" } } }));
        let first = next_matching(&rx, "the replaced switch's answer", |v| v["type"] == "control_response");
        assert_eq!(first["response"]["request_id"], "m1", "{first}");
        assert_eq!(first["response"]["subtype"], "success", "{first}");
        // After `HELD_RETRY`, m2 is asked as id 3.
        std::thread::sleep(Duration::from_millis(600));
        hermes_says(&mut hermes, json!({ "jsonrpc": "2.0", "id": 3, "result": {} }));
        let second = next_matching(&rx, "the newer switch's answer", |v| v["type"] == "control_response");
        assert_eq!(second["response"]["request_id"], "m2", "{second}");
        assert_eq!(second["response"]["subtype"], "success", "{second}");
        drop(input);
        drop(hermes);
        let _ = child.kill();
    }

    #[test]
    fn hermes_error_details_are_shown_and_busy_is_recognised() {
        assert_eq!(rpc_error("Internal error", Some("revision_conflict")), "Internal error (revision_conflict)");
        assert_eq!(rpc_error("No such model", None), "No such model");
        assert!(is_busy("Internal error", Some("session_busy")));
        assert!(is_busy("Mode changes apply when the session is idle; try again after this turn.", None));
        assert!(!is_busy("Internal error", Some("revision_conflict")));
    }

    #[test]
    fn a_session_hermes_never_opens_ends_with_an_error() {
        let bridge = |opening: Option<Instant>| {
            let dir = std::env::temp_dir().join(format!("hq-hermes-open-quiet-{}", std::process::id()));
            let opts = SpawnOptions { cwd: dir.to_string_lossy().into_owned(), resume: None, permission_mode: None, model: None, effort: None, fork: false, auto_tasks: false, max_agents: None, api_key: false, mcp: false, cell_key: None };
            let mut sink = if cfg!(windows) { Command::new("findstr") } else { Command::new("cat") };
            if cfg!(windows) {
                sink.arg("^");
            }
            sink.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
            hide_console(&mut sink);
            let mut child = sink.spawn().unwrap();
            let (lines, out) = std::io::pipe().unwrap();
            let mut b = Bridge::new(child.stdin.take().unwrap(), out, dir, opts, json!([]), mpsc::channel().0);
            b.opening = opening;
            (b, lines, child)
        };
        let read_all = |lines: PipeReader| BufReader::new(lines).lines().map(|l| serde_json::from_str::<Value>(&l.unwrap()).unwrap()).collect::<Vec<_>>();

        // Still within the wait: nothing happens.
        let (mut b, lines, mut child) = bridge(Some(Instant::now()));
        b.open_quiet();
        assert!(b.hermes.is_some());
        b.early.push("held".into());
        b.ready(); // the session opened: the clock stops
        assert!(b.opening.is_none());
        b.open_quiet();
        assert!(b.hermes.is_some());
        drop(b);
        assert!(read_all(lines).iter().all(|v| v["type"] != "result"));
        let _ = child.kill();

        // Silent past the wait: the cell gets an error and Hermes is let go.
        let (mut b, lines, mut child) = bridge(Instant::now().checked_sub(OPEN_QUIET + Duration::from_secs(1)));
        b.open_quiet();
        assert!(b.hermes.is_none());
        assert!(b.opening.is_none());
        drop(b);
        let result = read_all(lines).into_iter().find(|v| v["type"] == "result").expect("an error result");
        assert_eq!(result["is_error"], true);
        assert!(result["result"].as_str().unwrap().contains("did not open the session"), "{result}");
        let _ = child.kill();
    }

    #[test]
    fn a_turn_lost_mid_session_is_discarded_and_its_prompt_resent() {
        let dir = std::env::temp_dir().join(format!("hq-hermes-lost-turn-{}", std::process::id()));
        let opts = SpawnOptions { cwd: dir.to_string_lossy().into_owned(), resume: None, permission_mode: None, model: None, effort: None, fork: false, auto_tasks: false, max_agents: None, api_key: false, mcp: false, cell_key: None };
        let mut sink = if cfg!(windows) { Command::new("findstr") } else { Command::new("cat") };
        if cfg!(windows) {
            sink.arg("^");
        }
        sink.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut sink);
        let mut child = sink.spawn().unwrap();
        let (lines, out) = std::io::pipe().unwrap();
        let mut b = Bridge::new(child.stdin.take().unwrap(), out, dir, opts, json!([]), mpsc::channel().0);
        b.session = Some("local-abc".into());
        let prompt = json!({ "sessionId": "local-abc", "prompt": [{ "type": "text", "text": "go" }] });

        // Hermes took none of the prompt: once the lost turn is discarded it goes again, once.
        b.in_turn = true;
        b.lost_turn_prompt = Some((Instant::now(), prompt.clone(), true));
        b.lost_turn_discarded("local-abc", Ok(()));
        let (_, resent) = b.calls.iter().next().expect("the prompt resent");
        assert!(matches!(resent, Call::Prompt { retried: true, .. }));
        assert!(b.in_turn);
        b.calls.clear();

        // Hermes had accepted it before losing it: not resent, the turn ends saying so.
        b.lost_turn_prompt = Some((Instant::now(), prompt.clone(), false));
        b.lost_turn_discarded("local-abc", Ok(()));
        assert!(b.calls.is_empty() && !b.in_turn);

        // A refusal Hermes reports as "Internal error" shows its reason instead.
        b.in_turn = true;
        b.failed(Call::Prompt { started: Instant::now(), prompt: Value::Null, retried: true }, "Internal error", Some("admitted_turn_failed"));
        drop(b);
        let results: Vec<String> = BufReader::new(lines).lines()
            .map(|l| serde_json::from_str::<Value>(&l.unwrap()).unwrap())
            .filter(|v| v["type"] == "result")
            .map(|v| v["result"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(results, [LOST_ACCEPTED_TURN.to_owned(), "admitted_turn_failed".to_owned()]);
        let _ = child.kill();
    }

    #[test]
    fn a_quiet_turn_gets_a_harmless_nudge_once_a_wait() {
        let dir = std::env::temp_dir().join(format!("hq-hermes-nudge-{}", std::process::id()));
        let opts = SpawnOptions { cwd: dir.to_string_lossy().into_owned(), resume: None, permission_mode: None, model: None, effort: None, fork: false, auto_tasks: false, max_agents: None, api_key: false, mcp: false, cell_key: None };
        let mut sink = if cfg!(windows) { Command::new("findstr") } else { Command::new("cat") };
        if cfg!(windows) {
            sink.arg("^");
        }
        sink.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut sink);
        let mut child = sink.spawn().unwrap();
        let (_lines, out) = std::io::pipe().unwrap();
        let mut b = Bridge::new(child.stdin.take().unwrap(), out, dir, opts, json!([]), mpsc::channel().0);
        let nudges = |b: &Bridge| b.calls.values().filter(|c| matches!(c, Call::Nudge)).count();
        let long_ago = || Instant::now().checked_sub(TURN_NUDGE * 2).unwrap();

        // No turn of HQ's open: however quiet, nothing is written or waited on.
        b.heard = long_ago();
        assert_eq!(b.nudge_wait(), None);
        b.turn_quiet();
        assert_eq!(nudges(&b), 0);

        // A prompt sent after a long idle starts the clock afresh.
        b.session = Some("s1".into());
        b.send_prompt(json!({ "sessionId": "s1", "prompt": [] }), json!("hi"));
        assert!(b.in_turn);
        b.turn_quiet();
        assert_eq!(nudges(&b), 0, "nudged a turn that had only just begun");

        // A turn quiet past the wait gets one nudge, and no second while it is unanswered.
        b.heard = long_ago();
        b.turn_quiet();
        assert_eq!(nudges(&b), 1);
        b.heard = long_ago();
        b.turn_quiet();
        assert_eq!(nudges(&b), 1, "an unanswered nudge was kept beside its successor");

        // Its answer changes nothing, and the next waits a whole wait again.
        let nudge_id = |b: &Bridge| *b.calls.iter().find(|(_, c)| matches!(c, Call::Nudge)).unwrap().0;
        b.from_hermes(&json!({ "jsonrpc": "2.0", "id": nudge_id(&b), "result": { "sessions": [] } }).to_string());
        assert_eq!(nudges(&b), 0);
        assert!(b.in_turn && b.hermes.is_some());
        b.turn_quiet();
        assert_eq!(nudges(&b), 0, "nudged again without waiting");

        // Nor does its failure.
        b.heard = long_ago();
        b.turn_quiet();
        b.from_hermes(&json!({ "jsonrpc": "2.0", "id": nudge_id(&b), "error": { "code": -32603, "message": "no" } }).to_string());
        assert_eq!(nudges(&b), 0);
        assert!(b.in_turn && b.hermes.is_some());

        // The loop waits for the next nudge only while one can be written.
        assert!(b.nudge_wait().is_some_and(|w| w > Duration::ZERO && w <= TURN_NUDGE));
        b.heard = long_ago();
        assert_eq!(b.nudge_wait(), Some(Duration::ZERO));

        // A Hermes whose stdin is gone is neither written to nor waited on, so the loop cannot spin.
        b.hermes = None;
        assert_eq!(b.nudge_wait(), None);
        b.turn_quiet();
        assert_eq!(nudges(&b), 0);
        let _ = child.kill();
    }

    fn options(ids: &[(&str, &str)]) -> Vec<(String, String)> {
        ids.iter().map(|(i, k)| (i.to_string(), k.to_string())).collect()
    }

    #[test]
    fn names_the_command_that_clears_a_lost_turn() {
        let message = lost_turn_message("local-abc", "exit 1: no gateway");
        assert!(message.contains("`hermes sessions discard local-abc --yes`"));
        assert!(message.contains("exit 1: no gateway"));
    }

    #[test]
    fn turns_a_history_replay_into_transcript_lines() {
        let mut replay = Replay::default();
        for u in [
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "list " } }),
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "files" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "execute", "title": "terminal: dir", "rawInput": { "command": "dir" } }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "a.txt" } }] }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "One " } }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "file." } }),
            json!({ "sessionUpdate": "usage_update", "used": 5, "size": 10 }),
        ] {
            replay.push(&u);
        }
        let lines = replay.finish();
        let kinds: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["user", "assistant", "user", "assistant"]);
        assert_eq!(lines[0]["message"]["content"], "list files");
        assert_eq!(lines[1]["message"]["content"][0]["type"], "tool_use");
        assert_eq!(lines[2]["message"]["content"][0]["tool_use_id"], "t1");
        assert_eq!(lines[3]["message"]["content"][0]["text"], "One file.");
    }

    #[test]
    fn picks_the_option_matching_the_answer() {
        let all = options(&[("allow_once", "allow_once"), ("allow_session", "allow_always"), ("allow_always", "allow_always"), ("deny", "reject_once"), ("deny_always", "reject_always")]);
        assert_eq!(pick_option(&all, true, false).as_deref(), Some("allow_once"));
        assert_eq!(pick_option(&all, true, true).as_deref(), Some("allow_session"));
        assert_eq!(pick_option(&all, false, false).as_deref(), Some("deny"));
        let edit = options(&[("allow_once", "allow_once"), ("deny", "reject_once")]);
        assert_eq!(pick_option(&edit, true, true).as_deref(), Some("allow_once"));
        let odd = options(&[("yes", "allow_once"), ("no", "reject_once")]);
        assert_eq!(pick_option(&odd, true, false).as_deref(), Some("yes"));
        assert_eq!(pick_option(&odd, false, false).as_deref(), Some("no"));
        assert_eq!(pick_option(&[], true, false), None);
    }

    #[test]
    fn maps_tool_calls_to_claude_tools() {
        let shell = json!({ "title": "terminal: echo hi", "kind": "execute", "content": [] });
        assert_eq!(tool_use(&shell), ("Bash".into(), json!({ "command": "echo hi", "description": "terminal: echo hi" })));
        let approval = json!({ "title": "git force push", "kind": "execute", "rawInput": { "command": "git push --force" } });
        assert_eq!(tool_use(&approval).1["command"], "git push --force");
        let write = json!({ "title": "Approve edit: a.txt", "kind": "edit", "content": [{ "type": "diff", "path": "a.txt", "newText": "hi" }] });
        assert_eq!(tool_use(&write), ("Write".into(), json!({ "file_path": "a.txt", "content": "hi" })));
        let edit = json!({ "kind": "edit", "content": [{ "type": "diff", "path": "a.txt", "oldText": "a", "newText": "b" }] });
        assert_eq!(tool_use(&edit).0, "Edit");
        let read = json!({ "title": "read_file: src/x.rs", "kind": "read", "locations": [{ "path": "src/x.rs" }] });
        assert_eq!(tool_use(&read), ("read_file".into(), json!({ "path": "src/x.rs", "detail": "src/x.rs" })));
    }

    #[test]
    fn collects_tool_output_text() {
        let u = json!({ "content": [{ "type": "content", "content": { "type": "text", "text": "out" } }, { "type": "diff", "path": "a.txt" }] });
        assert_eq!(tool_text(&u), "out\nEdited a.txt");
        assert_eq!(tool_text(&json!({ "rawOutput": "raw" })), "raw");
    }

    #[test]
    fn rewrites_compact_only_as_a_whole_command() {
        assert_eq!(slash_command("/compact"), "/compress");
        assert_eq!(slash_command("/compact keep the plan"), "/compress keep the plan");
        assert_eq!(slash_command("/compaction"), "/compaction");
        assert_eq!(slash_command("hello"), "hello");
    }

    #[test]
    fn maps_permission_modes_both_ways() {
        assert_eq!(hermes_mode("acceptEdits"), Some("accept_edits"));
        assert_eq!(hermes_mode("bypassPermissions"), Some("dont_ask"));
        assert_eq!(hermes_mode("plan"), None);
        assert_eq!(claude_mode("dont_ask"), "bypassPermissions");
        assert_eq!(claude_mode("default"), "default");
    }

    #[test]
    fn formats_utc_timestamps() {
        let s = now_iso();
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.starts_with("20") && s.ends_with('Z'), "{s}");
    }

    fn todo_call(merge: bool, todos: Value) -> Value {
        json!({ "title": "todo_list: updating", "rawInput": { "merge": merge, "todos": todos } })
    }

    #[test]
    fn takes_only_todo_list_calls_and_skips_unknown_merge_items() {
        let mut list = Todos::new();
        // Another tool with a `todos` argument is not Hermes' list.
        assert!(!merge_todo_call(&mut list, &json!({ "title": "other_tool: x", "rawInput": { "todos": [{ "id": "1", "content": "A" }] } })));
        // A merge into a list this bridge never saw adds only items it can draw.
        assert!(merge_todo_call(&mut list, &todo_call(true, json!([{ "id": "1", "status": "completed" }, { "id": "2", "content": "B", "status": "pending" }]))));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "2");
    }

    #[test]
    fn merges_hermes_todo_lists() {
        let mut list = Todos::new();
        let first = json!({ "title": "todo_list: planning 3 task(s)", "rawInput": { "todos": [
            { "id": "1", "content": "Read f1", "status": "in_progress" },
            { "id": "2", "content": "Read f2", "status": "pending" },
            { "id": "3", "content": "Report", "status": "pending" },
        ] } });
        assert!(merge_todo_call(&mut list, &first));
        assert_eq!(list.len(), 3);
        // A merge upserts by id and keeps what it does not mention.
        assert!(merge_todo_call(&mut list, &todo_call(true, json!([{ "id": "1", "status": "completed" }, { "id": 2, "content": "Read f2", "status": "cancelled" }, { "id": "4", "content": "Extra", "status": "pending" }]))));
        let items = todo_items(&list);
        let shape: Vec<(&str, &str)> = items.iter().map(|t| (t["content"].as_str().unwrap(), t["status"].as_str().unwrap())).collect();
        assert_eq!(shape, [("Read f1", "completed"), ("Read f2", "completed"), ("Report", "pending"), ("Extra", "pending")]);
        assert_eq!(items[0]["activeForm"], "Read f1");
        // Without merge the list is replaced.
        assert!(merge_todo_call(&mut list, &todo_call(false, json!([{ "id": "9", "content": "Only", "status": "pending" }]))));
        assert_eq!(list.len(), 1);
        // A call without todos only reads the list.
        assert!(!merge_todo_call(&mut list, &json!({ "title": "todo_list: reading", "rawInput": {} })));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn replays_todo_calls_as_one_todo_write_each() {
        let mut replay = Replay::default();
        replay.push(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "todo_list: planning 2 task(s)", "rawInput": { "todos": [
            { "id": "1", "content": "A", "status": "in_progress" }, { "id": "2", "content": "B", "status": "pending" } ] } }));
        replay.push(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [] }));
        replay.push(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "todo_list: updating 1 task(s)", "rawInput": { "merge": true, "todos": [
            { "id": "1", "status": "completed" } ] } }));
        let lines = replay.finish();
        let kinds: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["assistant", "user", "assistant", "user"]);
        assert_eq!(lines[0]["message"]["content"][0]["name"], "TodoWrite");
        let last = &lines[2]["message"]["content"][0]["input"]["todos"];
        assert_eq!(last.as_array().unwrap().len(), 2);
        assert_eq!(last[0]["status"], "completed");
        assert_eq!(lines[3]["message"]["content"][0]["tool_use_id"], "t2");
    }

    #[test]
    fn asks_for_a_task_list_once_and_never_on_a_slash_command() {
        let text = |t: &str| json!({ "type": "text", "text": t });
        let mut blocks = vec![text("/compress")];
        assert!(!add_task_prompt(&mut blocks));
        assert_eq!(blocks.len(), 1);
        let mut blocks = vec![text("Fix /src/main.rs")];
        assert!(add_task_prompt(&mut blocks));
        assert_eq!(blocks[1]["text"], TASK_PROMPT);
        // Not beside the composer's nudge; the next prompt gets it.
        let mut blocks = vec![text("Do it"), text(COMPOSER_TASK_HINT)];
        assert!(!add_task_prompt(&mut blocks));
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn replay_leaves_out_hqs_task_note() {
        let mut replay = Replay::default();
        replay.push(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "Fix the build\n" } }));
        replay.push(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": TASK_PROMPT } }));
        replay.push(&json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "On it." } }));
        replay.push(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "Quote (HQ: track this work back" } }));
        replay.push(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": COMPOSER_TASK_HINT } }));
        let lines = replay.finish();
        assert_eq!(lines[0]["message"]["content"], "Fix the build");
        assert_eq!(lines[2]["message"]["content"], "Quote (HQ: track this work back");
    }

    #[test]
    fn describes_the_running_turn_in_a_side_question() {
        let tools: VecDeque<String> = ["read_file: f1.txt", "terminal: dir"].into_iter().map(String::from).collect();
        let todos: Todos = vec![("1".into(), json!({ "id": "1", "content": "Read f1", "status": "completed" })), ("2".into(), json!({ "id": "2", "content": "Sum", "status": "pending" }))];
        let out = side_prompt("Progress?", "Sum the facts", &tools, &todos);
        assert_eq!(
            out,
            "The user's current request (still running) is: \"Sum the facts\". Tools run so far this turn: read_file: f1.txt, terminal: dir. Current todo list: Read f1 [completed]; Sum [pending]. \n\nProgress?"
        );
        let bare = side_prompt("Progress?", "go", &VecDeque::new(), &Vec::new());
        assert_eq!(bare, "The user's current request (still running) is: \"go\". \n\nProgress?");
        let long = "é".repeat(2500);
        let out = side_prompt("q", &long, &VecDeque::new(), &Vec::new());
        assert!(out.contains(&format!("{}...\"", "é".repeat(2000))));
        assert_eq!(prompt_text(&json!([{ "type": "text", "text": "a" }, { "type": "image" }, { "type": "text", "text": "b" }])), "a\nb");
    }

    #[test]
    fn rejects_session_ids_that_could_escape_the_folder() {
        assert!(valid_session_id("a38210d7-7d33-467a-902e-e1c088932fb5"));
        assert!(!valid_session_id("../board"));
        assert!(!valid_session_id(""));
    }
}

/// Runs against the real `hermes` on PATH, so they are ignored by default and leave sessions in
/// Hermes' own state. Run with `cargo test live_ -- --ignored --nocapture --test-threads=1`.
#[cfg(test)]
mod live {
    use super::*;
    use std::process::Child;
    use std::time::Duration;

    struct Live {
        child: Child,
        input: mpsc::Sender<String>,
        lines: mpsc::Receiver<Value>,
        seen: Vec<Value>,
    }

    fn workdir() -> PathBuf {
        let dir = std::env::temp_dir().join("hq-hermes-live");
        fs::create_dir_all(dir.join("mirror")).unwrap();
        dir
    }

    fn start(resume: Option<&str>, fork: bool) -> Live {
        let dir = workdir();
        let opts = SpawnOptions {
            cwd: dir.to_string_lossy().into_owned(),
            resume: resume.map(str::to_owned),
            permission_mode: Some("default".into()),
            model: None,
            effort: None,
            fork,
            auto_tasks: false,
            max_agents: None,
            api_key: false,
            mcp: false,
            cell_key: None,
        };
        let mut cmd = Command::new(hermes_program());
        cmd.arg("acp").current_dir(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        hide_console(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let (input, lines) = translate(child.stdin.take().unwrap(), child.stdout.take().unwrap(), dir.join("mirror"), opts, json!([])).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(lines).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line).expect("translator wrote invalid JSON");
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Live { child, input, lines: rx, seen: Vec::new() }
    }

    impl Live {
        fn send(&self, v: Value) {
            self.input.send(v.to_string()).unwrap();
        }

        fn control(&self, id: &str, request: Value) {
            self.send(json!({ "type": "control_request", "request_id": id, "request": request }));
        }

        fn prompt(&self, text: &str) {
            self.send(json!({ "type": "user", "message": { "role": "user", "content": text }, "parent_tool_use_id": null }));
        }

        /// Collects lines until one matches, and returns that one.
        fn until(&mut self, what: &str, secs: u64, pred: impl Fn(&Value) -> bool) -> Value {
            let deadline = Instant::now() + Duration::from_secs(secs);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok(v) = self.lines.recv_timeout(left) else { panic!("timed out waiting for {what}; saw {:#?}", self.seen) };
                println!("<- {}", truncate(&v.to_string(), 300));
                self.seen.push(v.clone());
                if pred(&v) {
                    return v;
                }
            }
        }

        fn result(&mut self) -> Value {
            self.until("result", 300, |v| v["type"] == "result")
        }

        /// A control response, which may have arrived while waiting for something else.
        fn response(&mut self, id: &str) -> Value {
            let matches = |v: &Value| v["type"] == "control_response" && v["response"]["request_id"] == id;
            if let Some(v) = self.seen.iter().find(|v| matches(v)) {
                return v["response"].clone();
            }
            self.until(id, 300, |v| v["type"] == "control_response" && v["response"]["request_id"] == id)["response"].clone()
        }

        fn texts(&self) -> String {
            let blocks = self.seen.iter().filter(|v| v["type"] == "assistant").flat_map(|v| v["message"]["content"].as_array().cloned().unwrap_or_default());
            blocks.filter_map(|c| c["text"].as_str().map(str::to_owned)).collect::<Vec<_>>().join("\n")
        }

        fn stop(mut self) {
            drop(self.input);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.child.kill();
        }
    }

    fn truncate(s: &str, n: usize) -> String {
        if s.len() <= n { s.to_owned() } else { format!("{}...", &s[..s.floor_char_boundary(n)]) }
    }

    fn allow(h: &Live, ask: &Value) {
        let response = json!({ "behavior": "allow", "updatedInput": ask["request"]["input"] });
        h.send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": ask["request_id"], "response": response } }));
    }

    #[test]
    #[ignore]
    fn live_a_mode_switch_mid_turn_lands_after_the_turn() {
        let mut h = start(None, false);
        h.prompt("Without using any tools, write the numbers 1 to 150 in words, one per line.");
        h.until("the turn's first text", 120, |v| v["type"] == "stream_event" || v["type"] == "assistant");
        h.control("m1", json!({ "subtype": "set_permission_mode", "mode": "acceptEdits" }));
        h.prompt("Reply with just: done");
        let first = h.result();
        assert_eq!(first["subtype"], "success", "{first}");
        let switched = h.response("m1");
        assert_eq!(switched["subtype"], "success", "{switched}");
        let init = h.until("the second turn's start", 60, |v| v["type"] == "system" && v["subtype"] == "init");
        assert_eq!(init["permissionMode"], "acceptEdits", "{init}");
        let second = h.result();
        assert_eq!(second["subtype"], "success", "{second}");
        assert!(!h.texts().contains("when the session is idle"), "Hermes refused the switch: {}", h.texts());
        h.stop();
    }

    #[test]
    #[ignore]
    fn live_turns_tools_resume_fork_and_side_questions() {
        // A turn, with the initialize reply the frontend asks for at once.
        let mut h = start(None, false);
        h.control("init", json!({ "subtype": "initialize" }));
        h.control("settings", json!({ "subtype": "get_settings" }));
        // system/init comes with the first turn, not when the session opens.
        h.prompt("Reply with just the word pong");
        let init = h.until("system/init", 180, |v| v["type"] == "system");
        let id = init["session_id"].as_str().unwrap().to_owned();
        let reply = h.response("init");
        assert!(reply["response"]["models"].as_array().unwrap().len() > 1);
        assert!(reply["response"]["commands"].as_array().unwrap().iter().any(|c| c["name"] == "compress"), "{reply}");
        assert_eq!(h.response("settings")["response"]["applied"]["model"], init["model"]);
        let result = h.result();
        assert_eq!(result["subtype"], "success", "{result}");
        assert!(h.texts().to_lowercase().contains("pong"));
        assert!(h.seen.iter().any(|v| v["type"] == "stream_event"));
        let window = result["modelUsage"].as_object().unwrap().values().next().unwrap()["contextWindow"].as_u64().unwrap();
        assert!(window > 0, "{result}");

        // A shell command shows as Bash with its result.
        h.prompt("Use your terminal tool to run exactly: echo hq-probe-42 . Then reply done.");
        h.result();
        let blocks: Vec<Value> = h.seen.iter().flat_map(|v| v["message"]["content"].as_array().cloned().unwrap_or_default()).collect();
        let tool = blocks.iter().find(|c| c["type"] == "tool_use" && c["name"] == "Bash").expect("no Bash tool_use");
        assert!(blocks.iter().any(|c| c["type"] == "tool_result" && c["tool_use_id"] == tool["id"]), "no result for the Bash call");

        // An edit asks first; deny it, then allow it.
        let file = workdir().join("hq_probe.txt");
        let _ = fs::remove_file(&file);
        h.prompt("Use your write_file tool to create hq_probe.txt in the current folder containing exactly: hi");
        let ask = h.until("can_use_tool", 300, |v| v["type"] == "control_request" || v["type"] == "result");
        assert_eq!(ask["type"], "control_request", "the edit did not ask");
        assert_eq!(ask["request"]["tool_name"], "Write");
        let deny = json!({ "behavior": "deny", "message": "no" });
        h.send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": ask["request_id"], "response": deny } }));
        h.result();
        assert!(!file.exists(), "a denied write still wrote the file");
        h.prompt("Try again now: create hq_probe.txt containing exactly: hi");
        let ask = h.until("can_use_tool", 300, |v| v["type"] == "control_request");
        allow(&h, &ask);
        h.result();
        assert!(file.exists(), "an allowed write did not write the file");

        // Interrupt ends the turn the way Claude's does.
        h.prompt("Count from 1 to 400, one number per line, no other text.");
        h.until("streaming", 120, |v| v["type"] == "stream_event");
        h.control("stop", json!({ "subtype": "interrupt" }));
        let result = h.result();
        assert_eq!(result["subtype"], "error_during_execution", "{result}");

        // Mid-turn, Hermes will not fork, so the side question goes to a fresh session.
        h.prompt("Run the shell command `ping -n 30 127.0.0.1` in the terminal, then say done.");
        h.until("the ping", 180, |v| v["type"] == "assistant" && v.to_string().contains("tool_use"));
        h.control("busy-side", json!({ "subtype": "side_question", "question": "In one sentence, what is the running request?" }));
        let side = h.response("busy-side");
        assert_eq!(side["subtype"], "success", "{side}");
        h.result();

        // A side question gets an answer without joining the conversation.
        h.control("side", json!({ "subtype": "side_question", "question": "What is 2+2? Answer with the digit only." }));
        let side = h.response("side");
        assert!(side["response"]["response"].as_str().unwrap_or_default().contains('4'), "{side}");

        // Switching the model is reported back.
        let models = reply["response"]["models"].as_array().unwrap().clone();
        let current = init["model"].as_str().unwrap().to_owned();
        let other = models.iter().filter_map(|m| m["value"].as_str()).find(|m| *m != "default" && *m != current && m.starts_with("anthropic:")).unwrap().to_owned();
        h.control("model", json!({ "subtype": "set_model", "model": other }));
        assert_eq!(h.response("model")["subtype"], "success");
        h.control("settings2", json!({ "subtype": "get_settings" }));
        assert_eq!(h.response("settings2")["response"]["applied"]["model"], other.as_str());
        h.control("model2", json!({ "subtype": "set_model", "model": "default" }));
        h.response("model2");
        h.stop();

        assert!(workdir().join("mirror").join(format!("{id}.jsonl")).is_file(), "no transcript copy");

        // A new process resumes the session without replaying it, and remembers it.
        let mut h = start(Some(&id), false);
        h.prompt("What single word did you reply with at the very start of this conversation? Answer with that word only.");
        let init = h.until("system/init", 180, |v| v["type"] == "system" || v["type"] == "result");
        assert_eq!(init["session_id"], id.as_str(), "{init}");
        h.result();
        assert!(!h.seen.iter().any(|v| v["type"] == "user" && v["message"]["content"][0]["type"] != "tool_result"), "the load replay leaked through");
        assert!(h.texts().to_lowercase().contains("pong"), "{}", h.texts());

        // /compact becomes /compress, and the session still resumes afterwards.
        h.prompt("/compact");
        let result = h.result();
        assert_eq!(result["is_error"], false, "{result}");
        h.stop();
        let mut h = start(Some(&id), false);
        h.prompt("What single word did you reply with at the very start of this conversation? Answer with that word only.");
        let init = h.until("system/init", 180, |v| v["type"] == "system" || v["type"] == "result");
        assert_eq!(init["type"], "system", "no resume after /compress: {init}");
        h.result();
        println!("after compress: {}", h.texts());
        h.stop();

        // A fork is a new session with a copy of the transcript.
        let mut h = start(Some(&id), true);
        h.prompt("Reply with just the word ok");
        let init = h.until("system/init", 180, |v| v["type"] == "system" || v["type"] == "result");
        let fork = init["session_id"].as_str().unwrap().to_owned();
        assert_ne!(fork, id);
        assert!(workdir().join("mirror").join(format!("{fork}.jsonl")).is_file());
        h.result();
        h.stop();

        // An unknown session is an error, not a silent new session.
        let mut h = start(Some("00000000-0000-0000-0000-000000000000"), false);
        let r = h.until("error", 180, |v| v["type"] == "system" || v["type"] == "result");
        assert_eq!(r["type"], "result", "{r}");
        assert_eq!(r["is_error"], true);
        h.stop();
    }

    /// Needs a Hermes session in the live workdir; the turns test leaves several.
    #[test]
    #[ignore]
    fn live_lists_sessions_and_imports_history() {
        let dir = workdir();
        let cwd = dir.to_str().unwrap();
        let sessions = list_sessions(Some(cwd), 5).unwrap();
        let id = sessions.first().expect("no Hermes sessions in the workdir")["sessionId"].as_str().unwrap().to_owned();
        let path = dir.join("import").join(format!("{id}.jsonl"));
        let _ = fs::remove_file(&path);
        import(&path, &id, cwd).unwrap();
        let lines: Vec<Value> = fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(lines.iter().any(|l| l["type"] == "user"), "{lines:?}");
        assert!(lines.iter().any(|l| l["type"] == "assistant"), "{lines:?}");
        assert!(import(&dir.join("import").join("x.jsonl"), "00000000-0000-0000-0000-000000000000", cwd).is_err());
    }

    /// Takes as long as Hermes' tool timeout or `approvals.timeout`, whichever runs out first (7 to 10 minutes here).
    #[test]
    #[ignore]
    fn live_permission_timeout_dismisses_the_card() {
        let mut h = start(None, false);
        h.prompt("Use your write_file tool to create hq_timeout.txt in the current folder containing exactly: hi");
        let ask = h.until("can_use_tool", 300, |v| v["type"] == "control_request");
        let cancel = h.until("control_cancel_request", 1200, |v| v["type"] == "control_cancel_request");
        assert_eq!(cancel["request_id"], ask["request_id"]);
        // The model may well try again and ask anew, so end the turn here.
        h.control("stop", json!({ "subtype": "interrupt" }));
        h.result();
        h.stop();
    }
}

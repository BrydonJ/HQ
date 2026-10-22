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
fn hermes_program() -> PathBuf {
    if let Some(p) = std::env::var_os("HQ_HERMES").filter(|p| !p.is_empty()) {
        return p.into();
    }
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for ext in ["exe", "cmd", "bat"] {
                let p = dir.join("hermes").with_extension(ext);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    "hermes".into()
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
    let mut child = cmd.spawn().map_err(|e| {
        format!("Could not start `{} acp` ({e}). Is Hermes Agent installed and on PATH?", hermes_program().display())
    })?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        crate::session::kill_group(child.id());
        let _ = child.kill();
        return Err("hermes pipes unavailable".into());
    };
    match translate(stdin, stdout, mirror, opts) {
        Ok((input, lines)) => Ok(app.state::<Sessions>().register(app, child, Some(input), lines, stderr, events)),
        Err(e) => {
            crate::session::kill_group(child.id());
            let _ = child.kill();
            Err(e)
        }
    }
}

/// Starts the translator between Hermes' pipes and the frontend. Returns where the frontend's
/// lines go in, and the pipe its Claude-shaped lines come out of.
fn translate(stdin: ChildStdin, stdout: impl Read + Send + 'static, mirror: PathBuf, opts: SpawnOptions) -> Result<(mpsc::Sender<String>, PipeReader), String> {
    let (lines, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let (events_tx, events_rx) = mpsc::channel::<Event>();
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
    let bridge = Bridge::new(stdin, writer, mirror, opts);
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
}

/// What a JSON-RPC response from Hermes answers.
enum Call {
    Initialize,
    /// `session/new`, `session/load` or `session/fork` for the cell's own session.
    Open { loaded: Option<String>, fork_of: Option<String> },
    Prompt { started: Instant },
    /// A frontend control request, answered with `ok` once Hermes accepts.
    Control { request_id: String, ok: Value, then: After },
    /// `session/fork` for a side question (Handoff, the progress button).
    SideFork { request_id: String, question: String },
    /// `session/set_mode` putting a side question's fork back to asking before edits.
    SideMode { fork: String, question: String },
    SidePrompt { fork: String },
    /// The cell's own model or mode, set before anything the cell sent is passed on.
    Setup(After),
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
    next_rpc: u64,
    calls: HashMap<u64, Call>,
    session: Option<String>,
    /// While `session/load` replays the history, which HQ already drew from its own copy.
    loading: bool,
    /// Frontend lines that arrived before the session was open.
    early: Vec<String>,
    /// Setup calls Hermes has yet to answer; the cell's lines wait in `early` until it has.
    setup: usize,
    /// Prompts sent while a turn runs. Hermes would answer them at once and run them later
    /// without a reply of their own, so they wait here and each gets its own turn and result.
    prompts: VecDeque<(Value, Value)>, // (ACP prompt, the frontend's content for HQ's copy)
    in_turn: bool,
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
    fn new(hermes: ChildStdin, out: PipeWriter, mirror_dir: PathBuf, opts: SpawnOptions) -> Bridge {
        let task_prompt_due = opts.auto_tasks;
        Bridge {
            hermes: Some(hermes),
            out: Some(out),
            mirror_dir,
            mirror: None,
            opts,
            next_rpc: 0,
            calls: HashMap::new(),
            session: None,
            loading: false,
            early: Vec::new(),
            setup: 0,
            prompts: VecDeque::new(),
            in_turn: false,
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
        for event in events {
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
            }
        }
        // Dropping the writer ends the frontend's stream, which then reports the exit.
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
                    Some(err) => self.failed(call, err["message"].as_str().unwrap_or("Hermes reported an error")),
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
            Call::Prompt { started } => self.turn_ended(started, result["stopReason"].as_str().unwrap_or("end_turn"), &result["usage"], None),
            Call::Control { request_id, ok, then } => {
                match then {
                    After::Model(model) => self.model = model,
                    After::Mode(mode) => self.mode = mode,
                }
                self.reply_ok(&request_id, ok);
            }
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
        }
    }

    fn failed(&mut self, call: Call, error: &str) {
        match call {
            Call::Initialize | Call::Open { .. } => self.fatal(&format!("Hermes could not start the session: {error}")),
            Call::Prompt { started } => self.turn_ended(started, "error", &Value::Null, Some(error)),
            Call::Control { request_id, .. } | Call::SideFork { request_id, .. } => self.reply_err(&request_id, error),
            Call::SideMode { fork, .. } | Call::SidePrompt { fork } => {
                if let Some((request_id, _)) = self.sides.remove(&fork) {
                    self.reply_err(&request_id, error);
                }
            }
            // The cell keeps whatever Hermes is really using; system/init reports that.
            Call::Setup(_) => self.setup_done(),
        }
    }

    /// Reports an error the cell cannot recover from, then ends Hermes.
    fn fatal(&mut self, error: &str) {
        self.dismiss(|_| true);
        self.emit(json!({ "type": "result", "subtype": "error", "is_error": true, "result": error }));
        self.hermes = None;
    }

    fn open_session(&mut self) {
        let cwd = self.opts.cwd.clone();
        match self.opts.resume.clone().filter(|id| !id.is_empty()) {
            Some(id) if self.opts.fork => {
                let params = json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] });
                self.call("session/fork", params, Call::Open { loaded: None, fork_of: Some(id) });
            }
            // `session/load`, not `session/resume`: Hermes answers an unknown id for resume
            // with a brand new session, and load says plainly that it found nothing.
            Some(id) => {
                self.loading = true;
                self.todos_known = false; // until the replay brings a full write
                let params = json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] });
                self.call("session/load", params, Call::Open { loaded: Some(id), fork_of: None });
            }
            None => self.call("session/new", json!({ "cwd": cwd, "mcpServers": [] }), Call::Open { loaded: None, fork_of: None }),
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
        let tool = &params["toolCall"];
        let (name, input) = tool_use(tool);
        let title = tool["title"].as_str().unwrap_or_default();
        let request_id = format!("hermes-perm-{}", id.to_string().trim_matches('"'));
        let mut request = json!({
            "subtype": "can_use_tool", "tool_name": name, "input": input,
            // The card reads "Allow Write?  <path>" or "Allow Bash?  <what Hermes says it does>".
            "description": title.strip_prefix("Approve edit: ").unwrap_or(title), "tool_use_id": tool["toolCallId"],
        });
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

    fn turn_ended(&mut self, started: Instant, stop: &str, usage: &Value, error: Option<&str>) {
        self.flush();
        self.in_turn = false;
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
        self.turn_tools.clear();
        self.last_tool = None;
        // A Hermes that never listed its commands still answers initialize.
        self.commands.get_or_insert_with(Vec::new);
        self.answer_init();
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
        if self.in_turn {
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
            "permissionMode": claude_mode(&self.mode), "cwd": self.opts.cwd,
        }));
        self.record(json!({ "type": "user", "message": { "role": "user", "content": content } }));
        self.in_turn = true;
        self.turn_prompt = prompt_text(&content);
        self.turn_tools.clear();
        self.call("session/prompt", prompt, Call::Prompt { started: Instant::now() });
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
                let ok = json!({});
                self.call("session/set_model", json!({ "sessionId": session, "modelId": model }), Call::Control { request_id: request_id.into(), ok, then: After::Model(model) });
            }
            "set_permission_mode" => match req["mode"].as_str().and_then(hermes_mode) {
                Some(mode) => {
                    let params = json!({ "sessionId": session, "modeId": mode });
                    self.call("session/set_mode", params, Call::Control { request_id: request_id.into(), ok: json!({}), then: After::Mode(mode.into()) });
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

    fn options(ids: &[(&str, &str)]) -> Vec<(String, String)> {
        ids.iter().map(|(i, k)| (i.to_string(), k.to_string())).collect()
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
        };
        let mut cmd = Command::new(hermes_program());
        cmd.arg("acp").current_dir(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        hide_console(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let (input, lines) = translate(child.stdin.take().unwrap(), child.stdout.take().unwrap(), dir.join("mirror"), opts).unwrap();
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

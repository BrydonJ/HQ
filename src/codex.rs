//! Codex cells: one `codex app-server` child process per cell, spoken to over its JSON-RPC protocol
//! on stdio, and translated to and from the stream-json lines the frontend already speaks.
//!
//! The app-server is the protocol the Codex VS Code extension uses, and it ships inside the
//! `codex` CLI itself: updating Codex updates the protocol, the model list and the tools, with
//! nothing extra to install. Only the stable part of the protocol is used (no `experimentalApi`),
//! and every message is read tolerantly, so a newer Codex adds what HQ does not know instead of
//! breaking it.
//!
//! Codex keeps each conversation itself, so HQ stores nothing of its own: a restored cell draws its
//! history from Codex's thread (through one short-lived helper shared by every cell), which also
//! shows turns taken in the Codex CLI or VS Code since. Deleting this file, its `mod` line, the
//! `codex` feature and `ui/codex.js` (with the hooks that call it) removes Codex support entirely.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};

use crate::session::{hide_console, kill_group, Sessions, SpawnOptions};
use crate::store::TranscriptChunk;

/// The composer's one-off nudge for a Codex cell (`CODEX_TASK_HINT` in ui/codex.js), cut out of
/// replayed history. Codex has no system prompt to add to over the app-server, so unlike Claude
/// there is no automatic task list: newer Codex models plan without a todo tool.
const COMPOSER_TASK_HINT: &str = "(HQ: track this work with a short plan so the user can follow progress.)";

/// A restored cell draws at most this many history lines (or this many bytes of them); "Load
/// earlier" fetches the rest, as for Claude.
const MAX_HISTORY_LINES: usize = 1500;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

/// The shared helper ends after this long without a question.
const HELPER_IDLE: Duration = Duration::from_secs(45);
/// How long a helper question may take; listing or reading a long thread scans its rollout file.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

/// The `codex` executable chosen in HQ's own settings, which beats `HQ_CODEX`.
static CODEX_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Sets (or, when blank, clears) the executable every `codex` HQ starts uses, from the next start on.
pub fn set_codex_path(path: Option<String>) {
    let path = path.map(|p| p.trim().trim_matches('"').trim().to_owned()).filter(|p| !p.is_empty());
    *CODEX_PATH.lock().unwrap() = path;
    // The helper keeps running the old one until it idles out; it is cheap to restart.
    stop_helper();
}

/// The `codex` executable: HQ's setting, else `HQ_CODEX`, else whatever is on PATH. An npm install
/// on Windows ships only `codex.cmd`, which `Command` never finds by bare name (it only tries
/// `.exe`), so PATH is searched here.
fn codex_program() -> PathBuf {
    if let Some(p) = CODEX_PATH.lock().unwrap().clone() {
        return p.into();
    }
    if let Some(p) = std::env::var_os("HQ_CODEX").filter(|p| !p.is_empty()) {
        return p.into();
    }
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for ext in ["exe", "cmd", "bat"] {
                let p = dir.join("codex").with_extension(ext);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    "codex".into()
}

fn not_found_hint() -> &'static str {
    "Is Codex installed? Install it with `npm install -g @openai/codex` and sign in once with `codex login`, or pick its location in HQ's settings."
}

fn app_server() -> Command {
    let mut cmd = Command::new(codex_program());
    cmd.arg("app-server");
    hide_console(&mut cmd);
    cmd
}

fn valid_thread_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Notifications HQ never shows, which Codex then does not send at all: command output streams by
/// the character, and the whole turn's diff is resent after every edit.
const UNUSED_NOTIFICATIONS: [&str; 10] = [
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "item/plan/delta",
    "turn/diff/updated",
    "thread/status/changed",
    "account/rateLimits/updated",
    "rawResponseItem/completed",
];

fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "hq", "title": "HQ", "version": env!("CARGO_PKG_VERSION") },
        // Stable API only: experimental methods and fields may change between Codex releases.
        "capabilities": { "experimentalApi": false, "optOutNotificationMethods": UNUSED_NOTIFICATIONS },
    })
}

/// Starts `codex app-server` for a cell. Returns the same process id `Sessions::spawn` does, and
/// the frontend talks to it the same way.
pub fn spawn(app: &AppHandle, opts: SpawnOptions, events: Channel<InvokeResponseBody>) -> Result<u64, String> {
    if !Path::new(&opts.cwd).is_dir() {
        return Err(format!("Folder does not exist: {}", opts.cwd));
    }
    let mut cmd = app_server();
    cmd.current_dir(&opts.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start `{} app-server` ({e}). {}", codex_program().display(), not_found_hint()))?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        kill_group(child.id());
        let _ = child.kill();
        return Err("codex pipes unavailable".into());
    };
    match translate(stdin, stdout, opts) {
        Ok((input, lines)) => Ok(app.state::<Sessions>().register(app, child, Some(input), lines, stderr, events)),
        Err(e) => {
            kill_group(child.id());
            let _ = child.kill();
            Err(e)
        }
    }
}

/// Starts the translator between Codex's pipes and the frontend. Returns where the frontend's
/// lines go in, and the pipe its Claude-shaped lines come out of.
fn translate(stdin: ChildStdin, stdout: impl Read + Send + 'static, opts: SpawnOptions) -> Result<(mpsc::Sender<String>, PipeReader), String> {
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
                    if events_tx.send(Event::Codex(line)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = events_tx.send(Event::CodexClosed);
    });
    let bridge = Bridge::new(stdin, writer, opts);
    std::thread::spawn(move || bridge.run(events_rx));
    Ok((input_tx, lines))
}

// ---------- the shared helper: history, the resume list and the model list ----------

/// A `codex app-server` that answers questions for no cell in particular, started on the first
/// one and ended once idle, so a board of restored Codex cells starts one Codex, not one each.
struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Value>,
    next: u64,
    used: Instant,
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);
/// Held while `codex update` runs, so no helper starts a half-replaced binary.
static UPDATING: Mutex<()> = Mutex::new(());

impl Helper {
    fn start() -> Result<Self, String> {
        let mut cmd = app_server();
        // Its own folder, so no project's config layers apply to the questions.
        let dir = std::env::temp_dir().join("hq-codex");
        let _ = std::fs::create_dir_all(&dir);
        cmd.current_dir(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Could not start `{} app-server` ({e}). {}", codex_program().display(), not_found_hint()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill_group(child.id());
            let _ = child.kill();
            return Err("codex pipes unavailable".into());
        };
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(msg) = serde_json::from_slice::<Value>(&buf) {
                            if tx.send(msg).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        let mut helper = Helper { child, stdin: Some(stdin), rx, next: 0, used: Instant::now() };
        helper.ask("initialize", initialize_params())??;
        helper.notify("initialized");
        Ok(helper)
    }

    fn notify(&mut self, method: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "method": method })).and_then(|_| stdin.flush());
        }
    }

    /// The outer error is the helper failing (it is replaced); the inner one is Codex's answer.
    fn ask(&mut self, method: &str, params: Value) -> Result<Result<Value, String>, String> {
        self.next += 1;
        let id = self.next;
        self.used = Instant::now();
        let stdin = self.stdin.as_mut().ok_or("codex closed")?;
        writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("Codex stopped ({e})"))?;
        let deadline = Instant::now() + HELPER_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self.rx.recv_timeout(left).map_err(|_| "Codex did not answer".to_string())?;
            if msg["id"].as_u64() == Some(id) && msg.get("method").is_none() {
                self.used = Instant::now();
                return Ok(match msg.get("error") {
                    Some(err) => Err(err["message"].as_str().unwrap_or("Codex reported an error").to_owned()),
                    None => Ok(msg["result"].clone()),
                });
            }
            // Notifications, and requests from Codex (none are expected here), are not for the question.
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        drop(self.stdin.take());
        kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Asks the shared helper, starting it when needed. One question at a time; a helper that broke
/// is replaced once.
fn helper_ask(method: &str, params: Value) -> Result<Value, String> {
    // Always before HELPER, as in `update_codex`.
    let _update = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    let mut slot = HELPER.lock().unwrap();
    for attempt in 0..2 {
        if slot.is_none() {
            *slot = Some(Helper::start()?);
            spawn_reaper();
        }
        match slot.as_mut().unwrap().ask(method, params.clone()) {
            Ok(answer) => return answer,
            Err(e) => {
                *slot = None;
                if attempt == 1 {
                    return Err(e);
                }
            }
        }
    }
    unreachable!()
}

/// Ends the helper once it has been idle for `HELPER_IDLE`.
fn spawn_reaper() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(5));
        let mut slot = HELPER.lock().unwrap();
        match slot.as_ref() {
            None => return,
            Some(h) if h.used.elapsed() >= HELPER_IDLE => {
                *slot = None;
                return;
            }
            Some(_) => {}
        }
    });
}

/// Ends the helper now, before `codex update` replaces the binary under it, say.
pub fn stop_helper() {
    if let Ok(mut slot) = HELPER.lock() {
        *slot = None;
    }
}

/// Codex's models for the model menu, before any Codex cell has started: the same reply a
/// cell's initialize carries.
pub fn list_models() -> Result<Value, String> {
    let models = all_models(|params| helper_ask("model/list", params))?;
    let default = helper_ask("config/read", json!({})).ok().and_then(|r| r["config"]["model"].as_str().map(str::to_owned));
    Ok(json!({ "models": model_menu(&models, default.as_deref().unwrap_or_default()) }))
}

fn all_models(mut ask: impl FnMut(Value) -> Result<Value, String>) -> Result<Vec<Value>, String> {
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut params = json!({});
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let reply = ask(params)?;
        models.extend(reply["data"].as_array().cloned().unwrap_or_default());
        cursor = reply["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(models)
}

/// Codex's saved threads, newest first, for the resume list, in the shape of Claude's.
pub fn list_sessions(cwd: Option<&str>, limit: usize) -> Result<Vec<Value>, String> {
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut params = json!({ "limit": limit.min(100), "sortKey": "updated_at" });
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let reply = helper_ask("thread/list", params)?;
        for t in reply["data"].as_array().into_iter().flatten() {
            let (Some(id), Some(cwd)) = (t["id"].as_str(), t["cwd"].as_str()) else { continue };
            if !valid_thread_id(id) {
                continue;
            }
            let title = t["name"].as_str().filter(|s| !s.trim().is_empty());
            let preview = t["preview"].as_str().filter(|s| !s.trim().is_empty());
            sessions.push(json!({
                "sessionId": id, "cwd": cwd, "cwdExists": Path::new(cwd).is_dir(),
                "title": title.or(preview), "lastPrompt": if title.is_some() { preview } else { None },
                "modifiedMs": t["updatedAt"].as_u64().unwrap_or(0) * 1000,
                "started": iso(t["createdAt"].as_u64().unwrap_or(0) * 1000),
                "entrypoint": "Codex", "backend": "codex",
            }));
        }
        cursor = reply["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() || sessions.len() >= limit {
            break;
        }
    }
    sessions.truncate(limit);
    Ok(sessions)
}

/// A Codex thread's history for the frontend, as Claude-shaped transcript lines. `before` is the
/// index of the first line already shown (the `start` of the previous chunk).
pub fn load_transcript(thread_id: &str, before: Option<u64>) -> Result<TranscriptChunk, String> {
    if !valid_thread_id(thread_id) {
        return Err("invalid session id".into());
    }
    let mut turns = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..1000 {
        let mut params = json!({ "threadId": thread_id, "itemsView": "full", "sortDirection": "asc", "limit": 200 });
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let reply = match helper_ask("thread/turns/list", params) {
            Ok(r) => r,
            // A thread saved by an older Codex may not page; it is read whole instead.
            Err(_) if cursor.is_none() => {
                let read = helper_ask("thread/read", json!({ "threadId": thread_id, "includeTurns": true }))?;
                turns = read["thread"]["turns"].as_array().cloned().unwrap_or_default();
                break;
            }
            Err(e) => return Err(e),
        };
        turns.extend(reply["data"].as_array().cloned().unwrap_or_default());
        cursor = reply["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    let mut lines: Vec<String> = Vec::new();
    // The title goes first, so a later line can still replace it.
    if before.is_none() {
        if let Ok(read) = helper_ask("thread/read", json!({ "threadId": thread_id })) {
            if let Some(name) = read["thread"]["name"].as_str().filter(|n| !n.trim().is_empty()) {
                lines.push(json!({ "type": "ai-title", "aiTitle": name, "sessionId": thread_id }).to_string());
            }
        }
    }
    let history: Vec<String> = history_lines(&turns).into_iter().map(|l| l.to_string()).collect();
    let end = before.map_or(history.len(), |b| (b as usize).min(history.len()));
    let (mut start, mut bytes) = (end, 0);
    while start > 0 && end - start < MAX_HISTORY_LINES && bytes < MAX_HISTORY_BYTES {
        start -= 1;
        bytes += history[start].len();
    }
    lines.extend(history[start..end].iter().cloned());
    Ok(TranscriptChunk::new(lines, start as u64))
}

/// Runs `codex update` and returns what it printed. Codex only updates itself from its own
/// interactive screens, so HQ runs this while no Codex cell is running (ui/codex.js).
///
/// The updater is left out of any job object: if HQ closes or the wait times out, it carries on
/// and finishes, instead of being killed halfway through replacing the binary.
pub fn update_codex() -> Result<String, String> {
    let _update = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    stop_helper();
    let mut cmd = Command::new(codex_program());
    cmd.arg("update").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_console(&mut cmd);
    let child = cmd.spawn().map_err(|e| format!("Could not start `{}` ({e})", codex_program().display()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = rx
        .recv_timeout(Duration::from_secs(180))
        .map_err(|_| "`codex update` is still running".to_string())?
        .map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text.trim().to_owned())
    } else {
        Err(text.trim().to_owned())
    }
}

// ---------- the bridge ----------

enum Event {
    Frontend(String),
    FrontendClosed,
    Codex(String),
    CodexClosed,
}

/// What a JSON-RPC response from Codex answers.
enum Call {
    Initialize,
    Models { cursor_round: u32 },
    Config,
    /// `thread/start`, `thread/resume` or `thread/fork` for the cell's own thread.
    Open,
    TurnStart,
    Steer { input: Vec<Value> },
    Compact,
    /// A frontend control request, answered with `ok` once Codex accepts.
    Control { request_id: String, ok: Value },
    SideFork { request_id: String, question: String },
    SidePrompt { fork: String },
    Ignore,
}

/// Work waiting for the running turn to end.
enum Job {
    Prompt(Vec<Value>),
    Compact,
}

/// An approval request waiting on the user.
struct Approval {
    rpc_id: Value,
    method: String,
    params: Value,
}

/// A side question (Handoff, Sitrep) asked in a throwaway fork.
struct Side {
    request_id: String,
    texts: Vec<String>,
}

struct Bridge {
    codex: Option<ChildStdin>,
    out: Option<PipeWriter>,
    opts: SpawnOptions,
    next_rpc: u64,
    calls: HashMap<u64, Call>,
    thread: Option<String>,
    /// Frontend lines that arrived before the thread was open.
    early: Vec<String>,
    /// A `turn/start` is on its way and the turn is not live yet: prompts wait in `jobs` (and are
    /// steered in once it is).
    starting: bool,
    turn: Option<String>,
    turn_started: Instant,
    /// Stop was pressed before the turn had an id.
    interrupt_due: bool,
    jobs: VecDeque<Job>,
    models: Vec<Value>,
    models_loaded: bool,
    config_loaded: bool,
    waiting_init: Vec<String>,
    /// What the thread runs, as Codex reports it.
    model: String,
    effort: Option<String>,
    /// The model of Codex's own configuration, which "Default" means.
    default_model: String,
    /// The effort of Codex's own configuration, which applies until the cell picks one.
    config_effort: Option<String>,
    /// What the cell picked; sent with the next turn, which keeps it for the turns after.
    model_due: Option<String>,
    effort_due: Option<String>,
    mode: String,
    mode_due: bool,
    title: Option<String>,
    ctx_size: u64,
    ctx_used: u64,
    totals: (u64, u64, u64),
    /// The agent message being streamed (its item id).
    live: Option<String>,
    /// Started tool items by id, for approvals that only name the item.
    items: HashMap<String, Value>,
    approvals: HashMap<String, Approval>,
    sides: HashMap<String, Side>,
    plan_seq: u64,
    usage_seq: u64,
    compacting: bool,
    turn_prompt: String,
    turn_tools: VecDeque<String>,
}

impl Bridge {
    fn new(codex: ChildStdin, out: PipeWriter, opts: SpawnOptions) -> Bridge {
        let mode = opts.permission_mode.clone().filter(|m| policy(m).is_some()).unwrap_or_else(|| "acceptEdits".into());
        Bridge {
            codex: Some(codex),
            out: Some(out),
            opts,
            next_rpc: 0,
            calls: HashMap::new(),
            thread: None,
            early: Vec::new(),
            starting: false,
            turn: None,
            turn_started: Instant::now(),
            interrupt_due: false,
            jobs: VecDeque::new(),
            models: Vec::new(),
            models_loaded: false,
            config_loaded: false,
            waiting_init: Vec::new(),
            model: String::new(),
            effort: None,
            default_model: String::new(),
            config_effort: None,
            model_due: None,
            effort_due: None,
            mode,
            mode_due: false,
            title: None,
            ctx_size: 0,
            ctx_used: 0,
            totals: (0, 0, 0),
            live: None,
            items: HashMap::new(),
            approvals: HashMap::new(),
            sides: HashMap::new(),
            plan_seq: 0,
            usage_seq: 0,
            compacting: false,
            turn_prompt: String::new(),
            turn_tools: VecDeque::new(),
        }
    }

    fn run(mut self, events: mpsc::Receiver<Event>) {
        self.call("initialize", initialize_params(), Call::Initialize);
        for event in events {
            match event {
                Event::Frontend(line) => {
                    if self.thread.is_none() {
                        self.early.push(line);
                    } else {
                        self.frontend(&line);
                    }
                }
                // Closing Codex's stdin ends it; its stdout closing then ends this loop.
                Event::FrontendClosed => self.codex = None,
                Event::Codex(line) => self.from_codex(&line),
                Event::CodexClosed => break,
            }
        }
        // Dropping the writer ends the frontend's stream, which then reports the exit.
    }

    // ---------- writing ----------

    fn send_codex(&mut self, msg: Value) {
        let Some(stdin) = self.codex.as_mut() else { return };
        if writeln!(stdin, "{msg}").and_then(|_| stdin.flush()).is_err() {
            self.codex = None; // Codex is gone; its stdout closing reports the exit
        }
    }

    fn call(&mut self, method: &str, params: Value, call: Call) {
        let id = self.next_rpc;
        self.next_rpc += 1;
        self.calls.insert(id, call);
        self.send_codex(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    fn answer_codex(&mut self, id: Value, result: Value) {
        self.send_codex(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
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

    /// Reports an error the cell cannot recover from, then ends Codex.
    fn fatal(&mut self, error: &str) {
        self.emit(json!({ "type": "result", "subtype": "error", "is_error": true, "result": error }));
        self.codex = None;
    }

    // ---------- from Codex ----------

    fn from_codex(&mut self, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
            (Some(method), Some(id)) => self.codex_request(method, id.clone(), &msg["params"]),
            (Some(method), None) => self.notification(method, &msg["params"]),
            (None, Some(id)) => {
                let Some(call) = id.as_u64().and_then(|id| self.calls.remove(&id)) else { return };
                match msg.get("error") {
                    Some(err) => self.failed(call, err["message"].as_str().unwrap_or("Codex reported an error")),
                    None => self.answered(call, &msg["result"]),
                }
            }
            (None, None) => {}
        }
    }

    fn answered(&mut self, call: Call, result: &Value) {
        match call {
            Call::Initialize => {
                self.send_codex(json!({ "jsonrpc": "2.0", "method": "initialized" }));
                self.call("model/list", json!({}), Call::Models { cursor_round: 0 });
                self.call("config/read", json!({ "cwd": self.opts.cwd }), Call::Config);
                self.open_thread();
            }
            Call::Models { cursor_round } => {
                self.models.extend(result["data"].as_array().cloned().unwrap_or_default());
                match result["nextCursor"].as_str() {
                    Some(c) if cursor_round < 10 => self.call("model/list", json!({ "cursor": c }), Call::Models { cursor_round: cursor_round + 1 }),
                    _ => {
                        self.models_loaded = true;
                        self.answer_init();
                    }
                }
            }
            Call::Config => {
                if let Some(m) = result["config"]["model"].as_str().filter(|m| !m.is_empty()) {
                    self.default_model = m.to_owned();
                }
                self.config_effort = result["config"]["model_reasoning_effort"].as_str().map(str::to_owned);
                self.config_loaded = true;
                self.answer_init();
            }
            Call::Open => self.opened(result),
            // The turn is only live (to steer or stop) once `turn/started` says so.
            Call::TurnStart | Call::Steer { .. } | Call::Ignore => {}
            Call::Compact => {}
            Call::Control { request_id, ok } => self.reply_ok(&request_id, ok),
            Call::SideFork { request_id, question } => {
                let Some(fork) = result["thread"]["id"].as_str().map(str::to_owned) else {
                    return self.reply_err(&request_id, "Codex did not fork the session");
                };
                self.sides.insert(fork.clone(), Side { request_id, texts: Vec::new() });
                let params = json!({ "threadId": fork, "input": [text_input(&question)] });
                self.call("turn/start", params, Call::SidePrompt { fork });
            }
            Call::SidePrompt { .. } => {}
        }
    }

    fn failed(&mut self, call: Call, error: &str) {
        match call {
            Call::Initialize | Call::Open => self.fatal(&format!("Codex could not start the session: {error}")),
            Call::Models { .. } => {
                self.models_loaded = true;
                self.answer_init();
            }
            Call::Config => {
                self.config_loaded = true;
                self.answer_init();
            }
            Call::TurnStart => {
                self.starting = false;
                self.turn_ended("failed", Some(error), None);
            }
            // The turn ended before the prompt reached it: it starts the next one instead.
            Call::Steer { input } => {
                self.jobs.push_front(Job::Prompt(input));
                if !self.busy() {
                    self.next_job();
                }
            }
            Call::Compact => {
                self.compacting = false;
                self.emit(json!({ "type": "system", "subtype": "status", "status": null, "compact_result": "failed", "compact_error": error, "session_id": self.thread }));
                self.next_job();
            }
            Call::Control { request_id, .. } | Call::SideFork { request_id, .. } => self.reply_err(&request_id, error),
            Call::SidePrompt { fork } => {
                if let Some(side) = self.sides.remove(&fork) {
                    self.reply_err(&side.request_id, error);
                }
                self.call("thread/unsubscribe", json!({ "threadId": fork }), Call::Ignore);
            }
            Call::Ignore => {}
        }
    }

    fn open_thread(&mut self) {
        let (approval, sandbox, _) = policy(&self.mode).unwrap_or(policy("acceptEdits").unwrap());
        let mut params = json!({ "cwd": self.opts.cwd, "approvalPolicy": approval, "sandbox": sandbox });
        let model = self.opts.model.clone().filter(|m| !m.is_empty() && m != "default");
        if let Some(model) = &model {
            params["model"] = json!(model);
        }
        if let Some(effort) = self.opts.effort.clone().filter(|e| !e.is_empty()) {
            // Applied with the first turn; a thread takes no effort of its own.
            self.effort_due = Some(effort);
        }
        match self.opts.resume.clone().filter(|id| !id.is_empty()) {
            Some(id) => {
                params["threadId"] = json!(id);
                params["excludeTurns"] = json!(true);
                let method = if self.opts.fork { "thread/fork" } else { "thread/resume" };
                self.call(method, params, Call::Open);
            }
            None => self.call("thread/start", params, Call::Open),
        }
    }

    fn opened(&mut self, result: &Value) {
        let Some(id) = result["thread"]["id"].as_str().map(str::to_owned) else {
            return self.fatal("Codex did not report a thread id");
        };
        self.thread = Some(id);
        self.model = result["model"].as_str().unwrap_or_default().to_owned();
        self.effort = result["reasoningEffort"].as_str().map(str::to_owned);
        if let Some(name) = result["thread"]["name"].as_str().filter(|n| !n.trim().is_empty()) {
            self.title = Some(name.to_owned());
        }
        // Only now does the cell's own work reach Codex.
        for line in std::mem::take(&mut self.early) {
            self.frontend(&line);
        }
    }

    fn codex_request(&mut self, method: &str, id: Value, params: &Value) {
        let thread = params["threadId"].as_str().unwrap_or_default().to_owned();
        let side = self.sides.contains_key(&thread);
        match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" | "item/permissions/requestApproval" | "item/tool/requestUserInput" => {
                // A side question only asks: anything it wants approved is refused.
                if side {
                    let refusal = refusal(method, params);
                    return self.answer_codex(id, refusal);
                }
                self.ask_user(method, id, params);
            }
            // HQ shows no MCP forms: the server is told no, as an unattended client would.
            "mcpServer/elicitation/request" => self.answer_codex(id, json!({ "action": "decline", "content": null, "_meta": null })),
            _ => self.send_codex(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("HQ does not handle {method}") } })),
        }
    }

    /// Shows an approval request as Claude's permission card (or question card).
    fn ask_user(&mut self, method: &str, id: Value, params: &Value) {
        let item_id = params["itemId"].as_str().unwrap_or_default().to_owned();
        let request_id = format!("codex-perm-{}", id.to_string().trim_matches('"'));
        let mut request = match method {
            "item/commandExecution/requestApproval" => {
                let command = params["command"].as_str().map(unwrap_shell).unwrap_or_default();
                let mut r = json!({
                    "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": command },
                    "description": params["reason"].as_str().unwrap_or_default(), "tool_use_id": item_id,
                });
                let decisions = &params["availableDecisions"];
                let offers = |d: &str| decisions.as_array().is_none_or(|a| a.iter().any(|x| x == d || x.get(d).is_some()));
                if offers("acceptForSession") {
                    r["permission_suggestions"] = json!([{ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": command }], "destination": "session" }]);
                } else if offers("acceptWithExecpolicyAmendment") && params["proposedExecpolicyAmendment"].is_array() {
                    // Codex's own "don't ask again": a rule it keeps for good.
                    let prefix: Vec<&str> = params["proposedExecpolicyAmendment"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
                    r["permission_suggestions"] = json!([{ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": format!("{} *", prefix.join(" ")) }], "destination": "userSettings" }]);
                }
                r
            }
            "item/fileChange/requestApproval" => {
                let item = self.items.get(&item_id).cloned().unwrap_or(Value::Null);
                let changes = item["changes"].as_array().cloned().unwrap_or_default();
                let (name, input) = changes.first().map(change_tool).unwrap_or(("Edit".into(), json!({})));
                let mut description = params["reason"].as_str().unwrap_or_default().to_owned();
                if changes.len() > 1 {
                    let files: Vec<&str> = changes.iter().filter_map(|c| c["path"].as_str()).collect();
                    description = format!("{} files: {}", changes.len(), files.join(", "));
                }
                json!({
                    "subtype": "can_use_tool", "tool_name": name, "input": input, "description": description, "tool_use_id": item_id,
                    "permission_suggestions": [{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }],
                })
            }
            "item/permissions/requestApproval" => json!({
                "subtype": "can_use_tool", "tool_name": "Permissions", "input": params["permissions"],
                "description": params["reason"].as_str().unwrap_or("Codex asks for more access"), "tool_use_id": item_id,
                "permission_suggestions": [{ "type": "addRules", "rules": [{ "toolName": "Permissions" }], "destination": "session" }],
            }),
            _ => {
                // request_user_input, drawn as Claude's AskUserQuestion.
                let questions: Vec<Value> = params["questions"].as_array().into_iter().flatten().map(|q| json!({
                    "question": q["question"], "header": q["header"], "multiSelect": false,
                    "options": q["options"].as_array().cloned().unwrap_or_default(),
                })).collect();
                json!({ "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": { "questions": questions }, "tool_use_id": item_id })
            }
        };
        if request.get("tool_use_id").is_some_and(|v| v == "") {
            request.as_object_mut().unwrap().remove("tool_use_id");
        }
        self.approvals.insert(request_id.clone(), Approval { rpc_id: id, method: method.to_owned(), params: params.clone() });
        self.emit(json!({ "type": "control_request", "request_id": request_id, "request": request }));
    }

    fn notification(&mut self, method: &str, params: &Value) {
        let thread = params["threadId"].as_str().unwrap_or_default();
        if let Some(side) = self.sides.get_mut(thread) {
            match method {
                "item/completed" if params["item"]["type"] == "agentMessage" => {
                    if let Some(text) = params["item"]["text"].as_str().filter(|t| !t.trim().is_empty()) {
                        side.texts.push(text.trim().to_owned());
                    }
                }
                "turn/completed" => {
                    let fork = thread.to_owned();
                    let side = self.sides.remove(&fork).unwrap();
                    let turn = &params["turn"];
                    match turn["status"].as_str() {
                        // A stopped answer is cut short; Handoff must not save it as complete.
                        Some("interrupted") => self.reply_err(&side.request_id, "Stopped before Codex finished answering"),
                        Some("failed") => self.reply_err(&side.request_id, turn["error"]["message"].as_str().unwrap_or("Codex could not answer")),
                        _ => self.reply_ok(&side.request_id, json!({ "response": side.texts.join("\n\n") })),
                    }
                    // The fork is ephemeral: unloading it leaves nothing behind.
                    self.call("thread/unsubscribe", json!({ "threadId": fork }), Call::Ignore);
                }
                _ => {}
            }
            return;
        }
        if method == "serverRequest/resolved" {
            // Codex settled the request itself (the turn ended, say): its card goes.
            let rpc_id = params["requestId"].clone();
            let gone: Vec<String> = self.approvals.iter().filter(|(_, a)| a.rpc_id == rpc_id).map(|(r, _)| r.clone()).collect();
            for request_id in gone {
                self.approvals.remove(&request_id);
                self.emit(json!({ "type": "control_cancel_request", "request_id": request_id }));
            }
            return;
        }
        if method == "account/rateLimits/updated" {
            return;
        }
        // Subagents' threads run in this process too; their work shows as the tool call that started them.
        if self.thread.as_deref() != Some(thread) {
            return;
        }
        match method {
            "turn/started" => {
                if !self.starting && !self.compacting && self.turn.is_none() {
                    // A turn HQ did not start: Codex carrying on by itself.
                    self.begin_turn();
                }
                self.starting = false;
                if self.turn.is_none() {
                    self.turn = params["turn"]["id"].as_str().map(str::to_owned);
                    if self.interrupt_due {
                        self.interrupt_due = false;
                        self.interrupt();
                    }
                    self.steer_jobs();
                }
            }
            "turn/completed" => {
                let turn = &params["turn"];
                let status = turn["status"].as_str().unwrap_or("completed").to_owned();
                let error = turn["error"]["message"].as_str().map(|m| match turn["error"]["additionalDetails"].as_str().filter(|d| !d.is_empty()) {
                    Some(d) => format!("{m}\n{d}"),
                    None => m.to_owned(),
                });
                self.starting = false;
                if turn["error"]["codexErrorInfo"] == "usageLimitExceeded" {
                    // Drawn as the plan-limit card, the way Claude reports its own limit.
                    let text = error.clone().unwrap_or_else(|| "You've hit your Codex usage limit".into());
                    self.emit(json!({
                        "type": "assistant", "session_id": self.thread, "parent_tool_use_id": null, "error": "rate_limit",
                        "message": { "id": format!("codex-limit-{}", turn["id"].as_str().unwrap_or_default()), "role": "assistant", "model": "<synthetic>", "content": [{ "type": "text", "text": text }] },
                    }));
                }
                self.turn_ended(&status, error.as_deref(), turn["durationMs"].as_u64());
            }
            "item/started" => self.item_started(&params["item"]),
            "item/completed" => self.item_completed(&params["item"]),
            "item/agentMessage/delta" => {
                let item = params["itemId"].as_str().unwrap_or_default().to_owned();
                let Some(delta) = params["delta"].as_str().filter(|d| !d.is_empty()) else { return };
                if self.live.as_deref() != Some(item.as_str()) {
                    self.start_reply(&item);
                }
                self.emit(json!({
                    "type": "stream_event", "parent_tool_use_id": null, "session_id": self.thread,
                    "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": delta } },
                }));
            }
            "turn/plan/updated" => {
                let todos: Vec<Value> = params["plan"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        let status = match s["status"].as_str() {
                            Some("inProgress") => "in_progress",
                            Some("completed") => "completed",
                            _ => "pending",
                        };
                        json!({ "content": s["step"], "activeForm": s["step"], "status": status })
                    })
                    .collect();
                self.plan_seq += 1;
                let id = format!("codex-plan-{}", self.plan_seq);
                self.assistant(format!("{id}-msg"), json!([{ "type": "tool_use", "id": id, "name": "TodoWrite", "input": { "todos": todos } }]));
                self.tool_result(&id, json!("Plan updated"), false, None);
            }
            "thread/tokenUsage/updated" => {
                let usage = &params["tokenUsage"];
                self.ctx_size = usage["modelContextWindow"].as_u64().unwrap_or(self.ctx_size);
                self.ctx_used = usage["last"]["totalTokens"].as_u64().unwrap_or(self.ctx_used);
                let total = &usage["total"];
                let n = |k: &str| total[k].as_u64().unwrap_or(0);
                self.totals = (n("inputTokens").saturating_sub(n("cachedInputTokens")), n("outputTokens"), n("cachedInputTokens"));
                // Codex counts a request after its messages, so the context ring hears of it here,
                // from an empty message that only carries the count.
                self.usage_seq += 1;
                let id = format!("codex-usage-{}", self.usage_seq);
                self.assistant(id, json!([]));
            }
            "thread/name/updated" => {
                if let Some(name) = params["threadName"].as_str().filter(|n| !n.trim().is_empty()) {
                    self.title = Some(name.to_owned());
                    self.emit(json!({ "type": "ai-title", "aiTitle": name, "sessionId": self.thread }));
                }
            }
            "model/rerouted" => {
                if let Some(m) = params["toModel"].as_str().or(params["model"].as_str()) {
                    self.model = m.to_owned();
                }
            }
            "thread/compacted" => self.compacted(),
            _ => {}
        }
    }

    fn item_started(&mut self, item: &Value) {
        let Some(id) = item["id"].as_str().map(str::to_owned) else { return };
        match item["type"].as_str().unwrap_or_default() {
            "agentMessage" => {
                if self.live.as_deref() != Some(id.as_str()) {
                    self.start_reply(&id);
                }
            }
            "contextCompaction" => {
                if !self.compacting {
                    self.compacting = true;
                    self.emit(json!({ "type": "system", "subtype": "status", "status": "compacting", "session_id": self.thread }));
                }
            }
            "userMessage" | "reasoning" | "hookPrompt" | "plan" | "subAgentActivity" | "enteredReviewMode" | "exitedReviewMode" => {}
            _ => {
                let blocks = tool_uses(item);
                if blocks.is_empty() {
                    return;
                }
                self.end_reply();
                if let Some(title) = blocks.iter().find_map(|b| b["input"]["command"].as_str().or(b["input"]["file_path"].as_str()).map(str::to_owned)) {
                    if self.turn_tools.len() >= 30 {
                        self.turn_tools.pop_front();
                    }
                    self.turn_tools.push_back(title);
                }
                self.items.insert(id.clone(), item.clone());
                self.assistant(format!("codex-tool-{id}"), Value::Array(blocks));
            }
        }
    }

    fn item_completed(&mut self, item: &Value) {
        let Some(id) = item["id"].as_str().map(str::to_owned) else { return };
        match item["type"].as_str().unwrap_or_default() {
            "agentMessage" => {
                let text = item["text"].as_str().unwrap_or_default().trim().to_owned();
                if self.live.as_deref() == Some(id.as_str()) {
                    self.live = None;
                }
                if !text.is_empty() {
                    self.assistant(id, json!([{ "type": "text", "text": text }]));
                }
            }
            "reasoning" => {
                let thinking = reasoning_text(item);
                if !thinking.is_empty() {
                    self.end_reply();
                    self.assistant(id, json!([{ "type": "thinking", "thinking": thinking }]));
                }
            }
            "contextCompaction" => self.compacted(),
            "userMessage" | "hookPrompt" | "plan" | "subAgentActivity" | "enteredReviewMode" | "exitedReviewMode" => {}
            _ => {
                // A tool that was never seen starting (it began before a resume) is drawn whole.
                let started = self.items.remove(&id);
                let item = match started {
                    Some(_) => item.clone(),
                    None => {
                        let blocks = tool_uses(item);
                        if blocks.is_empty() {
                            return;
                        }
                        self.assistant(format!("codex-tool-{id}"), Value::Array(blocks));
                        item.clone()
                    }
                };
                for (tool_id, content, is_error, structured) in tool_results(&item) {
                    self.tool_result(&tool_id, content, is_error, structured);
                }
            }
        }
    }

    fn compacted(&mut self) {
        if !self.compacting {
            return;
        }
        self.compacting = false;
        self.emit(json!({ "type": "system", "subtype": "compact_boundary", "session_id": self.thread, "compact_metadata": { "trigger": "manual" } }));
    }

    fn start_reply(&mut self, id: &str) {
        self.live = Some(id.to_owned());
        let base = json!({ "type": "stream_event", "parent_tool_use_id": null, "session_id": self.thread });
        let mut start = base.clone();
        start["event"] = json!({ "type": "message_start", "message": { "id": id, "role": "assistant", "model": self.model } });
        self.emit(start);
        let mut block = base;
        block["event"] = json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } });
        self.emit(block);
    }

    /// A reply cut off by a tool call ends here; its text arrives with its own `item/completed`.
    fn end_reply(&mut self) {
        self.live = None;
    }

    fn assistant(&mut self, id: String, content: Value) {
        self.emit(json!({
            "type": "assistant", "session_id": self.thread, "parent_tool_use_id": null,
            "message": {
                "id": id, "type": "message", "role": "assistant", "model": self.model, "content": content,
                "usage": { "input_tokens": self.ctx_used },
            },
        }));
    }

    fn tool_result(&mut self, id: &str, content: Value, is_error: bool, structured: Option<Value>) {
        let mut line = json!({
            "type": "user", "session_id": self.thread, "parent_tool_use_id": null,
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error }] },
        });
        if let Some(s) = structured {
            line["tool_use_result"] = s;
        }
        self.emit(line);
    }

    /// Marks the start of a turn the way Claude does, with `system/init`.
    fn begin_turn(&mut self) {
        self.turn_started = Instant::now();
        self.turn_tools.clear();
        self.emit(json!({
            "type": "system", "subtype": "init", "session_id": self.thread, "model": self.model,
            "permissionMode": self.mode, "cwd": self.opts.cwd,
        }));
    }

    fn turn_ended(&mut self, status: &str, error: Option<&str>, duration: Option<u64>) {
        self.end_reply();
        self.turn = None;
        self.interrupt_due = false;
        let mut model_usage = Map::new();
        model_usage.insert(self.model.clone(), json!({
            "contextWindow": self.ctx_size, "inputTokens": self.totals.0,
            "outputTokens": self.totals.1, "cacheReadInputTokens": self.totals.2,
        }));
        let (subtype, error) = match (status, error) {
            ("interrupted", _) => ("error_during_execution", None),
            ("failed", e) => ("error", Some(e.unwrap_or("Codex could not finish this turn."))),
            _ => ("success", None),
        };
        let duration = duration.unwrap_or_else(|| self.turn_started.elapsed().as_millis() as u64);
        let mut result = json!({
            "type": "result", "subtype": subtype, "is_error": subtype != "success", "session_id": self.thread,
            "duration_ms": duration, "modelUsage": model_usage,
        });
        if let Some(error) = error {
            result["result"] = json!(error);
        }
        if self.compacting {
            self.compacting = false;
            self.emit(json!({ "type": "system", "subtype": "status", "status": null, "compact_result": if subtype == "success" { "success" } else { "failed" }, "session_id": self.thread }));
        }
        self.emit(result);
        // Cards for this turn are gone from the frontend; any still open in Codex are void.
        for (_, a) in std::mem::take(&mut self.approvals) {
            let refusal = refusal(&a.method, &a.params);
            self.answer_codex(a.rpc_id, refusal);
        }
        self.items.clear();
        self.turn_tools.clear();
        self.next_job();
    }

    // ---------- from the frontend ----------

    fn frontend(&mut self, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        match msg["type"].as_str().unwrap_or_default() {
            "user" => self.prompt(&msg["message"]["content"]),
            "control_request" => {
                let request_id = msg["request_id"].as_str().unwrap_or_default().to_owned();
                self.control(&request_id, &msg["request"]);
            }
            "control_response" => self.permission_answer(&msg["response"]),
            _ => {}
        }
    }

    fn busy(&self) -> bool {
        self.starting || self.turn.is_some() || self.compacting
    }

    fn prompt(&mut self, content: &Value) {
        let input = user_input(content);
        if input.is_empty() {
            return;
        }
        // HQ's compact button (and a typed /compact) sends Claude's `/compact`.
        let job = match prompt_text(content).trim() {
            "/compact" => Job::Compact,
            _ => Job::Prompt(input),
        };
        if !self.busy() {
            self.jobs.push_back(job);
            return self.next_job();
        }
        match job {
            // Read by the running turn, as Claude reads a message sent while it works.
            Job::Prompt(input) if self.turn.is_some() && !self.compacting => self.steer(input),
            job => self.jobs.push_back(job),
        }
    }

    fn steer(&mut self, input: Vec<Value>) {
        let (Some(thread), Some(turn)) = (self.thread.clone(), self.turn.clone()) else { return };
        let params = json!({ "threadId": thread, "input": input, "expectedTurnId": turn });
        self.call("turn/steer", params, Call::Steer { input });
    }

    /// Prompts that came in while the turn was starting go into it once it has an id.
    fn steer_jobs(&mut self) {
        while matches!(self.jobs.front(), Some(Job::Prompt(_))) {
            if let Some(Job::Prompt(input)) = self.jobs.pop_front() {
                self.steer(input);
            }
        }
    }

    fn next_job(&mut self) {
        if self.busy() {
            return;
        }
        let Some(thread) = self.thread.clone() else { return };
        match self.jobs.pop_front() {
            Some(Job::Prompt(input)) => {
                self.turn_prompt = input.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n");
                let mut params = json!({ "threadId": thread, "input": input });
                if let Some(model) = self.model_due.take() {
                    params["model"] = json!(model);
                    self.model = model;
                }
                if let Some(effort) = self.effort_due.take().filter(|e| self.supports_effort(e)) {
                    params["effort"] = json!(effort);
                    self.effort = Some(effort);
                }
                if self.mode_due {
                    self.mode_due = false;
                    if let Some((approval, _, sandbox)) = policy(&self.mode) {
                        params["approvalPolicy"] = json!(approval);
                        params["sandboxPolicy"] = sandbox;
                    }
                }
                self.starting = true;
                self.begin_turn();
                self.call("turn/start", params, Call::TurnStart);
            }
            Some(Job::Compact) => {
                self.compacting = true;
                self.begin_turn();
                self.emit(json!({ "type": "system", "subtype": "status", "status": "compacting", "session_id": self.thread }));
                self.call("thread/compact/start", json!({ "threadId": thread }), Call::Compact);
            }
            None => {}
        }
    }

    fn interrupt(&mut self) {
        let (Some(thread), Some(turn)) = (self.thread.clone(), self.turn.clone()) else { return };
        self.call("turn/interrupt", json!({ "threadId": thread, "turnId": turn }), Call::Ignore);
    }

    fn supports_effort(&self, effort: &str) -> bool {
        let model = if self.model.is_empty() { &self.default_model } else { &self.model };
        match self.models.iter().find(|m| m["model"] == model.as_str() || m["id"] == model.as_str()) {
            Some(m) => m["supportedReasoningEfforts"].as_array().is_some_and(|a| a.iter().any(|e| e["reasoningEffort"] == effort)),
            None => true, // a model Codex did not list: let it decide
        }
    }

    /// Answers the frontend's initialize requests once the model list and config are in.
    fn answer_init(&mut self) {
        if !self.models_loaded || !self.config_loaded || self.waiting_init.is_empty() {
            return;
        }
        let default = if self.default_model.is_empty() { self.model.clone() } else { self.default_model.clone() };
        let commands = json!([{ "name": "compact", "description": "Summarize the conversation to free up context", "argumentHint": "", "aliases": [] }]);
        let response = json!({ "models": model_menu(&self.models, &default), "commands": commands, "remote_control_available": false });
        for request_id in std::mem::take(&mut self.waiting_init) {
            self.reply_ok(&request_id, response.clone());
        }
    }

    fn control(&mut self, request_id: &str, req: &Value) {
        let thread = self.thread.clone();
        match req["subtype"].as_str().unwrap_or_default() {
            "initialize" => {
                self.waiting_init.push(request_id.to_owned());
                self.answer_init();
            }
            "get_settings" => {
                let effort = self.effort_due.clone().or_else(|| self.effort.clone()).or_else(|| self.config_effort.clone()).or_else(|| self.default_effort());
                let model = self.model_due.clone().unwrap_or_else(|| self.model.clone());
                self.reply_ok(request_id, json!({ "applied": { "model": model, "effort": effort } }));
            }
            "interrupt" => {
                // Prompts still queued are dropped with the turn, as in Claude.
                self.jobs.clear();
                if self.turn.is_some() {
                    self.interrupt();
                } else if self.starting {
                    self.interrupt_due = true;
                }
                // Side questions run on: Stop is for the conversation.
                self.reply_ok(request_id, json!({}));
            }
            "set_model" => {
                let model = match req["model"].as_str().unwrap_or("default") {
                    "default" if !self.default_model.is_empty() => self.default_model.clone(),
                    "default" => return self.reply_err(request_id, "Codex reported no default model"),
                    m => m.to_owned(),
                };
                // Applied with the next turn, which keeps it for the turns after.
                self.model_due = Some(model);
                if self.effort_due.as_deref().or(self.effort.as_deref()).is_some_and(|e| !self.model_supports(self.model_due.as_deref().unwrap(), e)) {
                    self.effort_due = None;
                    self.effort = None;
                }
                self.reply_ok(request_id, json!({}));
            }
            "apply_flag_settings" => match req["settings"]["effortLevel"].as_str() {
                Some(level) => {
                    self.effort_due = Some(level.to_owned());
                    self.reply_ok(request_id, json!({}));
                }
                None => self.reply_err(request_id, "Codex only takes an effort level here"),
            },
            "set_permission_mode" => match req["mode"].as_str().filter(|m| policy(m).is_some()) {
                Some(mode) => {
                    self.mode = mode.to_owned();
                    self.mode_due = true;
                    self.reply_ok(request_id, json!({}));
                }
                None => self.reply_err(request_id, "Codex has no such mode"),
            },
            "get_context_usage" => {
                let percent = if self.ctx_size > 0 { self.ctx_used as f64 * 100.0 / self.ctx_size as f64 } else { 0.0 };
                let response = json!({ "totalTokens": self.ctx_used, "maxTokens": self.ctx_size, "percentage": percent, "model": self.model, "categories": [] });
                self.reply_ok(request_id, response);
            }
            "generate_session_title" => match self.title.clone() {
                Some(title) => self.reply_ok(request_id, json!({ "title": title })),
                None => self.reply_err(request_id, "Codex has not titled this session yet"),
            },
            "rename_session" => {
                let title = req["title"].as_str().unwrap_or_default().to_owned();
                let ok = json!({ "title": title });
                self.title = Some(title.clone()).filter(|t| !t.is_empty());
                self.call("thread/name/set", json!({ "threadId": thread, "name": title }), Call::Control { request_id: request_id.into(), ok });
            }
            "side_question" => {
                let mut question = req["question"].as_str().unwrap_or_default().to_owned();
                if self.busy() {
                    // The fork has only the saved history, which lacks the running turn.
                    question = side_prompt(&question, &self.turn_prompt, &self.turn_tools);
                }
                // Read-only and never asking, so it can look but not touch; ephemeral, so nothing is saved.
                let params = json!({ "threadId": thread, "ephemeral": true, "approvalPolicy": "never", "sandbox": "read-only", "excludeTurns": true });
                self.call("thread/fork", params, Call::SideFork { request_id: request_id.into(), question });
            }
            other => self.reply_err(request_id, &format!("Codex does not support {other}")),
        }
    }

    fn model_supports(&self, model: &str, effort: &str) -> bool {
        match self.models.iter().find(|m| m["model"] == model || m["id"] == model) {
            Some(m) => m["supportedReasoningEfforts"].as_array().is_some_and(|a| a.iter().any(|e| e["reasoningEffort"] == effort)),
            None => true,
        }
    }

    fn default_effort(&self) -> Option<String> {
        let model = if self.model.is_empty() { &self.default_model } else { &self.model };
        self.models.iter().find(|m| m["model"] == model.as_str() || m["id"] == model.as_str()).and_then(|m| m["defaultReasoningEffort"].as_str().map(str::to_owned))
    }

    fn permission_answer(&mut self, response: &Value) {
        let Some(request_id) = response["request_id"].as_str() else { return };
        let Some(a) = self.approvals.remove(request_id) else { return };
        let decision = &response["response"];
        let allow = response["subtype"] == "success" && decision["behavior"] == "allow";
        let remember = decision["updatedPermissions"].as_array().is_some_and(|p| !p.is_empty());
        let result = match a.method.as_str() {
            "item/commandExecution/requestApproval" => {
                let offered = &a.params["availableDecisions"];
                let offers = |d: &str| offered.as_array().is_none_or(|list| list.iter().any(|x| x == d || x.get(d).is_some()));
                let choice = match (allow, remember) {
                    (true, true) if offers("acceptForSession") => json!("acceptForSession"),
                    (true, true) if offers("acceptWithExecpolicyAmendment") && a.params["proposedExecpolicyAmendment"].is_array() => {
                        json!({ "acceptWithExecpolicyAmendment": { "execpolicy_amendment": a.params["proposedExecpolicyAmendment"] } })
                    }
                    (true, _) => json!("accept"),
                    // "decline" lets the turn go on without the command; some requests only offer "cancel".
                    (false, _) if offers("decline") => json!("decline"),
                    (false, _) => json!("cancel"),
                };
                json!({ "decision": choice })
            }
            "item/fileChange/requestApproval" => {
                let choice = match (allow, remember) {
                    (true, true) => "acceptForSession",
                    (true, false) => "accept",
                    (false, _) => "decline",
                };
                if allow && remember {
                    // "Allow all edits this session" is what Edit automatically does.
                    self.mode = "acceptEdits".into();
                    self.mode_due = true;
                }
                json!({ "decision": choice })
            }
            "item/permissions/requestApproval" => {
                if allow {
                    let mut granted = Map::new();
                    for (k, v) in a.params["permissions"].as_object().into_iter().flatten() {
                        if !v.is_null() {
                            granted.insert(k.clone(), v.clone());
                        }
                    }
                    json!({ "permissions": granted, "scope": if remember { "session" } else { "turn" } })
                } else {
                    refusal(&a.method, &a.params)
                }
            }
            _ => {
                // AskUserQuestion's answers are keyed by question text; Codex keys them by id.
                let answers = &decision["updatedInput"]["answers"];
                let mut out = Map::new();
                if allow {
                    for q in a.params["questions"].as_array().into_iter().flatten() {
                        if let (Some(id), Some(answer)) = (q["id"].as_str(), q["question"].as_str().and_then(|t| answers[t].as_str())) {
                            out.insert(id.to_owned(), json!({ "answers": [answer] }));
                        }
                    }
                }
                json!({ "answers": out })
            }
        };
        self.answer_codex(a.rpc_id, result);
    }
}

/// What an approval request is answered with when nobody can approve it.
fn refusal(method: &str, params: &Value) -> Value {
    match method {
        "item/commandExecution/requestApproval" => {
            let offers_decline = params["availableDecisions"].as_array().is_none_or(|a| a.iter().any(|x| x == "decline"));
            json!({ "decision": if offers_decline { "decline" } else { "cancel" } })
        }
        "item/fileChange/requestApproval" => json!({ "decision": "decline" }),
        "item/permissions/requestApproval" => json!({ "permissions": {}, "scope": "turn" }),
        _ => json!({ "answers": {} }),
    }
}

/// HQ's permission modes (Claude's names) as Codex's approval policy, sandbox mode (for a new
/// thread) and sandbox policy (for a turn). These are the three presets Codex itself offers.
fn policy(mode: &str) -> Option<(&'static str, &'static str, Value)> {
    match mode {
        // Read only: Codex asks before it edits a file or runs anything that needs more.
        "default" | "plan" => Some(("on-request", "read-only", json!({ "type": "readOnly", "networkAccess": false }))),
        // Codex's own default: edits and commands inside the folder, asking for anything beyond it.
        "acceptEdits" | "auto" => Some((
            "on-request",
            "workspace-write",
            json!({ "type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false }),
        )),
        // Full access, never asking.
        "bypassPermissions" => Some(("never", "danger-full-access", json!({ "type": "dangerFullAccess" }))),
        _ => None,
    }
}

/// Codex's model list as the frontend's model menu: "Default" (Codex's configured model) first.
fn model_menu(models: &[Value], default: &str) -> Vec<Value> {
    let efforts = |m: &Value| -> Vec<Value> {
        m["supportedReasoningEfforts"].as_array().into_iter().flatten().filter_map(|e| e["reasoningEffort"].as_str()).map(|e| json!(e)).collect()
    };
    let listed = models.iter().filter(|m| m["hidden"] != true);
    let default_entry = models
        .iter()
        .find(|m| !default.is_empty() && (m["model"] == default || m["id"] == default))
        .or_else(|| models.iter().find(|m| m["isDefault"] == true));
    let resolved = default_entry.and_then(|m| m["model"].as_str()).unwrap_or(default);
    let mut menu = vec![json!({
        "value": "default", "displayName": "Default",
        "description": if resolved.is_empty() { "Codex's configured model".to_owned() } else { format!("Codex's configured model ({resolved})") },
        "resolvedModel": resolved,
        "supportsEffort": default_entry.is_none_or(|m| !efforts(m).is_empty()),
        "supportedEffortLevels": default_entry.map(efforts).unwrap_or_default(),
    })];
    menu.extend(listed.map(|m| {
        let levels = efforts(m);
        json!({
            "value": m["id"], "displayName": m["displayName"], "description": m["description"], "resolvedModel": m["model"],
            "supportsEffort": !levels.is_empty(), "supportedEffortLevels": levels,
        })
    }));
    menu
}

/// The frontend's prompt (a string, or Claude content blocks) as Codex user input.
fn user_input(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) if !text.trim().is_empty() => vec![text_input(text)],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item["type"].as_str() {
                Some("text") => item["text"].as_str().filter(|t| !t.is_empty()).map(text_input),
                Some("image") => {
                    let (mime, data) = (item["source"]["media_type"].as_str()?, item["source"]["data"].as_str()?);
                    Some(json!({ "type": "image", "url": format!("data:{mime};base64,{data}") }))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn text_input(text: &str) -> Value {
    json!({ "type": "text", "text": text, "text_elements": [] })
}

/// The text of a frontend prompt (a string, or content blocks).
fn prompt_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter(|i| i["type"] == "text").filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// `/bin/bash -lc 'ls -la'` as `ls -la`: Codex runs every command through a login shell, which
/// says nothing to the user. Anything else is left as it is.
fn unwrap_shell(command: &str) -> String {
    let trimmed = command.trim();
    for shell in ["bash", "zsh", "sh"] {
        for flag in ["-lc", "-c"] {
            let Some(at) = trimmed.find(&format!("{shell} {flag} ")) else { continue };
            let program = &trimmed[..at];
            // Only the shell itself, by name or path, may come before it.
            if !(program.is_empty() || program.ends_with('/')) || program.contains(' ') {
                continue;
            }
            let rest = trimmed[at + shell.len() + flag.len() + 2..].trim();
            if rest.len() >= 2 && rest.starts_with('\'') && rest.ends_with('\'') {
                return rest[1..rest.len() - 1].replace("'\\''", "'");
            }
            if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
                return rest[1..rest.len() - 1].replace("\\\"", "\"");
            }
            return rest.to_owned();
        }
    }
    trimmed.to_owned()
}

/// A reasoning item's text: its summary, or else its raw reasoning when Codex shows that.
fn reasoning_text(item: &Value) -> String {
    let join = |k: &str| item[k].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    let summary = join("summary");
    if summary.is_empty() {
        join("content")
    } else {
        summary
    }
}

/// One file change as the Claude tool that shows it best: a new file as Write, the rest as Edit
/// (old and new text rebuilt from the diff, so the card draws it as a diff).
fn change_tool(change: &Value) -> (String, Value) {
    let path = change["path"].clone();
    let diff = change["diff"].as_str().unwrap_or_default();
    match change["kind"]["type"].as_str() {
        Some("add") => ("Write".into(), json!({ "file_path": path, "content": added_text(diff) })),
        Some("delete") => ("Edit".into(), json!({ "file_path": path, "old_string": diff, "new_string": "" })),
        _ => {
            let (old, new) = sides_of(diff);
            let mut input = json!({ "file_path": path, "old_string": old, "new_string": new });
            if let Some(to) = change["kind"]["move_path"].as_str() {
                input["move_to"] = json!(to);
            }
            ("Edit".into(), input)
        }
    }
}

/// A new file's text. Codex sends it bare, but a unified diff of `+` lines is read too.
fn added_text(diff: &str) -> String {
    if !diff.starts_with("@@") && !diff.starts_with("--- ") {
        return diff.to_owned();
    }
    diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).map(|l| &l[1..]).collect::<Vec<_>>().join("\n")
}

/// The old and new text of a unified diff's hunks (context lines in both).
fn sides_of(diff: &str) -> (String, String) {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    for line in diff.lines() {
        if line.starts_with("@@") || line.starts_with("---") || line.starts_with("+++") || line.starts_with('\\') {
            continue;
        }
        match line.split_at(line.len().min(1)) {
            ("-", rest) => old.push(rest),
            ("+", rest) => new.push(rest),
            (_, _) => {
                let rest = line.strip_prefix(' ').unwrap_or(line);
                old.push(rest);
                new.push(rest);
            }
        }
    }
    (old.join("\n"), new.join("\n"))
}

/// A unified diff as Claude's structured patch, which the card draws with real line numbers.
fn structured_patch(diff: &str) -> Vec<Value> {
    let mut hunks: Vec<Value> = Vec::new();
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@") {
            // "@@ -1,2 +1,3 @@": starts and counts; a bare "@@" has none.
            let nums: Vec<u64> = header
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            let (old_start, old_lines, new_start, new_lines) = match nums.as_slice() {
                [a, b, c, d, ..] => (*a, *b, *c, *d),
                [a, c] => (*a, 1, *c, 1),
                _ => (0, 0, 0, 0),
            };
            hunks.push(json!({ "oldStart": old_start, "oldLines": old_lines, "newStart": new_start, "newLines": new_lines, "lines": [] }));
        } else if line.starts_with("---") || line.starts_with("+++") {
            continue;
        } else if let Some(h) = hunks.last_mut() {
            h["lines"].as_array_mut().unwrap().push(json!(line));
        }
    }
    hunks.retain(|h| h["lines"].as_array().is_some_and(|l| !l.is_empty()));
    hunks
}

/// The Claude tool_use blocks that draw a Codex tool item.
fn tool_uses(item: &Value) -> Vec<Value> {
    let id = item["id"].as_str().unwrap_or_default();
    let block = |name: &str, input: Value| json!({ "type": "tool_use", "id": id, "name": name, "input": input });
    match item["type"].as_str().unwrap_or_default() {
        "commandExecution" => vec![block("Bash", json!({ "command": unwrap_shell(item["command"].as_str().unwrap_or_default()) }))],
        "fileChange" => {
            let changes = item["changes"].as_array().cloned().unwrap_or_default();
            let single = changes.len() == 1;
            changes
                .iter()
                .enumerate()
                .map(|(n, c)| {
                    let (name, input) = change_tool(c);
                    let tool_id = if single { id.to_owned() } else { format!("{id}#{n}") };
                    json!({ "type": "tool_use", "id": tool_id, "name": name, "input": input })
                })
                .collect()
        }
        "mcpToolCall" => {
            let name = format!("mcp__{}__{}", item["server"].as_str().unwrap_or("server"), item["tool"].as_str().unwrap_or("tool"));
            let input = if item["arguments"].is_object() { item["arguments"].clone() } else { json!({ "arguments": item["arguments"] }) };
            vec![block(&name, input)]
        }
        "dynamicToolCall" => {
            let input = if item["arguments"].is_object() { item["arguments"].clone() } else { json!({ "arguments": item["arguments"] }) };
            vec![block(item["tool"].as_str().unwrap_or("tool"), input)]
        }
        "webSearch" => {
            let action = &item["action"];
            let query = item["query"].as_str().filter(|q| !q.is_empty()).or(action["query"].as_str()).or(action["url"].as_str()).unwrap_or_default();
            vec![block("WebSearch", json!({ "query": query }))]
        }
        "imageView" => vec![block("Read", json!({ "file_path": item["path"] }))],
        "collabAgentToolCall" => {
            let prompt = item["prompt"].as_str().unwrap_or_default();
            let tool = item["tool"].as_str().unwrap_or("agent");
            let description = if prompt.is_empty() { tool.to_owned() } else { format!("{tool}: {}", first_line(prompt, 80)) };
            vec![block("Agent", json!({ "description": description, "prompt": prompt }))]
        }
        "imageGeneration" => vec![block("ImageGeneration", json!({ "prompt": item["revisedPrompt"].as_str().or(item["prompt"].as_str()).unwrap_or_default() }))],
        _ => Vec::new(),
    }
}

/// A finished Codex tool item as (tool_use id, result content, is_error, structured result).
fn tool_results(item: &Value) -> Vec<(String, Value, bool, Option<Value>)> {
    let id = item["id"].as_str().unwrap_or_default().to_owned();
    let status = item["status"].as_str().unwrap_or("completed");
    let failed = matches!(status, "failed" | "declined");
    match item["type"].as_str().unwrap_or_default() {
        "commandExecution" => {
            let mut text = item["aggregatedOutput"].as_str().unwrap_or_default().trim_end().to_owned();
            let code = item["exitCode"].as_i64();
            if status == "declined" {
                text = "The user declined this command.".into();
            } else if let Some(code) = code.filter(|c| *c != 0) {
                text = if text.is_empty() { format!("Exit code {code}") } else { format!("{text}\nExit code {code}") };
            }
            vec![(id, json!(text), failed || code.is_some_and(|c| c != 0), None)]
        }
        "fileChange" => {
            let changes = item["changes"].as_array().cloned().unwrap_or_default();
            let single = changes.len() == 1;
            changes
                .iter()
                .enumerate()
                .map(|(n, c)| {
                    let tool_id = if single { id.clone() } else { format!("{id}#{n}") };
                    let path = c["path"].as_str().unwrap_or("the file");
                    let text = match status {
                        "declined" => "The user declined this edit.".to_owned(),
                        "failed" => format!("Could not change {path}"),
                        _ if c["kind"]["type"] == "add" => format!("Created {path}"),
                        _ if c["kind"]["type"] == "delete" => format!("Deleted {path}"),
                        _ => format!("Updated {path}"),
                    };
                    let patch = (c["kind"]["type"] == "update").then(|| structured_patch(c["diff"].as_str().unwrap_or_default())).filter(|p| !p.is_empty());
                    (tool_id, json!(text), failed, patch.map(|p| json!({ "structuredPatch": p })))
                })
                .collect()
        }
        "mcpToolCall" => {
            let text = match item["error"]["message"].as_str() {
                Some(e) => e.to_owned(),
                None => item["result"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| c["text"].as_str().map_or_else(|| format!("[{}]", c["type"].as_str().unwrap_or("content")), str::to_owned))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            vec![(id, json!(text), failed || !item["error"].is_null(), None)]
        }
        "dynamicToolCall" => {
            let text = item["contentItems"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| c["text"].as_str().map_or_else(|| format!("[{}]", c["type"].as_str().unwrap_or("content")), str::to_owned))
                .collect::<Vec<_>>()
                .join("\n");
            vec![(id, json!(text), failed || item["success"] == false, None)]
        }
        "webSearch" => vec![(id, json!(format!("Searched: {}", item["query"].as_str().unwrap_or_default())), false, None)],
        "imageView" => vec![(id, json!("Viewed the image"), false, None)],
        "collabAgentToolCall" => {
            let states: Vec<String> = item["agentsStates"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(agent, s)| format!("{agent}: {}", s["status"].as_str().or(s.as_str()).unwrap_or("done")))
                .collect();
            vec![(id, json!(states.join("\n")), failed, None)]
        }
        "imageGeneration" => vec![(id, json!(item["savedPath"].as_str().map_or("Generated an image".to_owned(), |p| format!("Saved {p}"))), failed, None)],
        _ => Vec::new(),
    }
}

/// A thread's saved turns as the Claude-shaped lines a cell replays.
fn history_lines(turns: &[Value]) -> Vec<Value> {
    let mut lines = Vec::new();
    for turn in turns {
        let started = iso(turn["startedAt"].as_u64().unwrap_or(0) * 1000);
        let ended = turn["completedAt"].as_u64().map_or_else(|| started.clone(), |s| iso(s * 1000));
        for item in turn["items"].as_array().into_iter().flatten() {
            let id = item["id"].as_str().unwrap_or_default();
            let mut push = |mut line: Value, ts: &str| {
                line["timestamp"] = json!(ts);
                lines.push(line);
            };
            match item["type"].as_str().unwrap_or_default() {
                "userMessage" => {
                    let mut content = Vec::new();
                    for input in item["content"].as_array().into_iter().flatten() {
                        match input["type"].as_str() {
                            Some("text") => {
                                let text = strip_hint(input["text"].as_str().unwrap_or_default());
                                if !text.is_empty() {
                                    content.push(json!({ "type": "text", "text": text }));
                                }
                            }
                            // The image itself stays in Codex; the bubble notes it was there.
                            Some("image") | Some("localImage") => content.push(json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "" } })),
                            _ => {}
                        }
                    }
                    if !content.is_empty() {
                        push(json!({ "type": "user", "message": { "role": "user", "content": content } }), &started);
                    }
                }
                "agentMessage" => {
                    let text = item["text"].as_str().unwrap_or_default().trim();
                    if !text.is_empty() {
                        push(json!({ "type": "assistant", "message": { "id": id, "type": "message", "role": "assistant", "content": [{ "type": "text", "text": text }] } }), &ended);
                    }
                }
                "reasoning" => {
                    let thinking = reasoning_text(item);
                    if !thinking.is_empty() {
                        push(json!({ "type": "assistant", "message": { "id": id, "type": "message", "role": "assistant", "content": [{ "type": "thinking", "thinking": thinking }] } }), &ended);
                    }
                }
                "contextCompaction" => push(json!({ "type": "system", "subtype": "compact_boundary", "compact_metadata": { "trigger": "auto" } }), &ended),
                _ => {
                    let blocks = tool_uses(item);
                    if blocks.is_empty() {
                        continue;
                    }
                    push(json!({ "type": "assistant", "message": { "id": format!("codex-tool-{id}"), "type": "message", "role": "assistant", "content": blocks } }), &ended);
                    for (tool_id, content, is_error, structured) in tool_results(item) {
                        let mut line = json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": tool_id, "content": content, "is_error": is_error }] } });
                        if let Some(s) = structured {
                            line["tool_use_result"] = s;
                        }
                        push(line, &ended);
                    }
                }
            }
        }
        if turn["status"] == "interrupted" {
            let mut line = json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "text", "text": "[Request interrupted by user]" }] } });
            line["timestamp"] = json!(ended);
            lines.push(line);
        }
    }
    lines
}

/// A prompt as the user wrote it: the composer's task nudge, which only Codex was meant to see, is cut off.
fn strip_hint(text: &str) -> String {
    match text.find(COMPOSER_TASK_HINT) {
        Some(at) => format!("{}{}", text[..at].trim_end(), &text[at + COMPOSER_TASK_HINT.len()..]).trim().to_owned(),
        None => text.trim().to_owned(),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default();
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

/// A side question asked while a turn runs. The fork that answers it sees only the saved
/// history, which lacks the running turn, so the turn is described in front of the question.
fn side_prompt(question: &str, prompt: &str, tools: &VecDeque<String>) -> String {
    let mut out = String::new();
    if prompt.trim().is_empty() {
        out.push_str("A request is still running. ");
    } else {
        let clipped: String = prompt.chars().take(2000).collect();
        let dots = if clipped.len() < prompt.len() { "..." } else { "" };
        out.push_str(&format!("The user's current request (still running) is: \"{clipped}{dots}\". "));
    }
    if !tools.is_empty() {
        out.push_str("Tools run so far this turn: ");
        out.push_str(&tools.iter().cloned().collect::<Vec<_>>().join(", "));
        out.push_str(". ");
    }
    out.push_str("\n\n");
    out.push_str(question);
    out
}

/// A time in ms since 1970 as Claude's transcripts write it, e.g. `2026-10-08T08:15:02.123Z`.
/// Zero means now.
fn iso(ms: u64) -> String {
    let ms = if ms == 0 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) } else { ms };
    let secs = ms / 1000;
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
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600, rem % 3_600 / 60, rem % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_login_shells_only() {
        assert_eq!(unwrap_shell("/bin/bash -lc 'ls -la'"), "ls -la");
        assert_eq!(unwrap_shell("bash -lc 'echo '\\''hi'\\'''"), "echo 'hi'");
        assert_eq!(unwrap_shell("/usr/bin/zsh -c \"git status\""), "git status");
        assert_eq!(unwrap_shell("cargo test -- bash -c 'x'"), "cargo test -- bash -c 'x'");
        assert_eq!(unwrap_shell("pwsh -Command Get-ChildItem"), "pwsh -Command Get-ChildItem");
    }

    #[test]
    fn maps_permission_modes_to_codex_presets() {
        assert_eq!(policy("default").map(|p| (p.0, p.1)), Some(("on-request", "read-only")));
        assert_eq!(policy("acceptEdits").map(|p| (p.0, p.1)), Some(("on-request", "workspace-write")));
        assert_eq!(policy("bypassPermissions").map(|p| (p.0, p.1)), Some(("never", "danger-full-access")));
        assert!(policy("dontAsk").is_none());
    }

    #[test]
    fn draws_file_changes_as_write_and_edit() {
        let add = json!({ "path": "/w/a.txt", "kind": { "type": "add" }, "diff": "hello\nworld\n" });
        assert_eq!(change_tool(&add), ("Write".into(), json!({ "file_path": "/w/a.txt", "content": "hello\nworld\n" })));
        let update = json!({ "path": "/w/a.txt", "kind": { "type": "update", "move_path": null }, "diff": "@@ -1,2 +1,2 @@\n hello\n-world\n+there\n" });
        assert_eq!(change_tool(&update).1, json!({ "file_path": "/w/a.txt", "old_string": "hello\nworld", "new_string": "hello\nthere" }));
        assert_eq!(
            structured_patch("@@ -1,2 +1,2 @@\n hello\n-world\n+there\n"),
            vec![json!({ "oldStart": 1, "oldLines": 2, "newStart": 1, "newLines": 2, "lines": [" hello", "-world", "+there"] })]
        );
        let item = json!({ "type": "fileChange", "id": "c1", "status": "completed", "changes": [add, update] });
        let uses = tool_uses(&item);
        assert_eq!((uses[0]["id"].as_str(), uses[1]["id"].as_str()), (Some("c1#0"), Some("c1#1")));
        let results = tool_results(&item);
        assert_eq!(results[0].1, "Created /w/a.txt");
        assert!(results[1].3.is_some());
    }

    #[test]
    fn turns_saved_turns_into_transcript_lines() {
        let turns = json!([{
            "id": "t1", "status": "interrupted", "startedAt": 1792677199, "completedAt": 1792677200,
            "items": [
                { "type": "userMessage", "id": "u1", "content": [{ "type": "text", "text": format!("Fix it\n{COMPOSER_TASK_HINT}"), "text_elements": [] }] },
                { "type": "reasoning", "id": "r1", "summary": ["Looking"], "content": [] },
                { "type": "commandExecution", "id": "x1", "command": "/bin/bash -lc 'false'", "status": "failed", "aggregatedOutput": "", "exitCode": 1 },
                { "type": "agentMessage", "id": "m1", "text": "Done." },
            ],
        }]);
        let lines = history_lines(turns.as_array().unwrap());
        let kinds: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["user", "assistant", "assistant", "user", "assistant", "user"]);
        assert_eq!(lines[0]["message"]["content"][0]["text"], "Fix it");
        assert_eq!(lines[0]["timestamp"], "2026-10-22T13:53:19.000Z");
        assert_eq!(lines[2]["message"]["content"][0]["input"]["command"], "false");
        assert_eq!(lines[3]["message"]["content"][0]["is_error"], true);
        assert_eq!(lines[3]["message"]["content"][0]["content"], "Exit code 1");
        assert_eq!(lines[5]["message"]["content"][0]["text"], "[Request interrupted by user]");
    }

    #[test]
    fn lists_default_first_with_each_models_efforts() {
        let models = [
            json!({ "id": "a", "model": "a", "displayName": "A", "description": "", "hidden": false, "isDefault": true,
                "supportedReasoningEfforts": [{ "reasoningEffort": "low" }, { "reasoningEffort": "high" }] }),
            json!({ "id": "h", "model": "h", "displayName": "H", "hidden": true, "supportedReasoningEfforts": [] }),
            json!({ "id": "b", "model": "b", "displayName": "B", "hidden": false, "supportedReasoningEfforts": [] }),
        ];
        let menu = model_menu(&models, "");
        let values: Vec<&str> = menu.iter().map(|m| m["value"].as_str().unwrap()).collect();
        assert_eq!(values, ["default", "a", "b"]);
        assert_eq!(menu[0]["resolvedModel"], "a");
        assert_eq!(menu[0]["supportedEffortLevels"], json!(["low", "high"]));
        assert_eq!(menu[2]["supportsEffort"], false);
        // The configured model wins over the catalog's default.
        assert_eq!(model_menu(&models, "b")[0]["resolvedModel"], "b");
    }

    #[test]
    fn sends_images_as_data_urls() {
        let content = json!([{ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }, { "type": "text", "text": "what is this" }]);
        let input = user_input(&content);
        assert_eq!(input[0], json!({ "type": "image", "url": "data:image/png;base64,AAAA" }));
        assert_eq!(input[1]["text"], "what is this");
        assert!(user_input(&json!("  ")).is_empty());
    }
}

/// Runs the bridge against the real `codex` on PATH, talking to a fake model server, so they need
/// no account; ignored by default. Run with `cargo test codex::live -- --ignored --test-threads=1`.
#[cfg(test)]
mod live {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// A minimal OpenAI Responses API: each request gets a scripted reply, picked by how many tool
    /// outputs it carries and by words in the last prompt.
    fn mock_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { continue };
                std::thread::spawn(move || serve(stream, n));
            }
        });
        port
    }

    fn serve(mut stream: TcpStream, n: usize) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (mut len, mut line) = (0usize, String::new());
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let input = req["input"].as_array().cloned().unwrap_or_default();
        let outputs = input.iter().filter(|i| i["type"] == "function_call_output" || i["type"] == "custom_tool_call_output").count();
        let last_user = input.iter().rev().find(|i| i["role"] == "user").map(|i| i["content"].to_string()).unwrap_or_default();
        let text = |id: &str, t: &str| {
            vec![
                json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "message", "id": id, "role": "assistant", "content": [] } }),
                json!({ "type": "response.output_text.delta", "item_id": id, "output_index": 0, "content_index": 0, "delta": t }),
                json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "message", "id": id, "role": "assistant", "content": [{ "type": "output_text", "text": t }] } }),
            ]
        };
        let call = |name: &str, args: Value| vec![json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "function_call", "id": format!("fc{n}"), "call_id": format!("call{n}"), "name": name, "arguments": args.to_string() } })];
        let patch = |p: &str| vec![json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "custom_tool_call", "id": format!("ct{n}"), "call_id": format!("call{n}"), "name": "apply_patch", "input": p } })];
        let reasoning = vec![json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "reasoning", "id": format!("rs{n}"), "summary": [{ "type": "summary_text", "text": "Thinking it over" }], "content": [] } })];
        if last_user.contains("SLOW") {
            std::thread::sleep(Duration::from_secs(4));
        }
        let mut events = if last_user.contains("PLAIN") || last_user.contains("SLOW") || last_user.contains("Progress check") || outputs >= 3 {
            let mut e = reasoning.clone();
            e.extend(text(&format!("m{n}"), "Progress: 50%\nAll done here."));
            e
        } else if outputs == 0 {
            let mut e = reasoning.clone();
            e.extend(text(&format!("m{n}"), "Let me look."));
            e.extend(call("exec_command", json!({ "cmd": "ls" })));
            e
        } else if outputs == 1 {
            patch("*** Begin Patch\n*** Add File: hello.txt\n+hello\n+world\n*** End Patch\n")
        } else {
            patch("*** Begin Patch\n*** Update File: hello.txt\n@@\n hello\n-world\n+there\n*** End Patch\n")
        };
        events.push(json!({ "type": "response.completed", "response": { "id": format!("resp{n}"), "usage": { "input_tokens": 1000, "input_tokens_details": { "cached_tokens": 100 }, "output_tokens": 20, "output_tokens_details": { "reasoning_tokens": 5 }, "total_tokens": 1020 } } }));
        let mut out = String::from("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
        for e in events {
            out.push_str(&format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()));
        }
        let _ = stream.write_all(out.as_bytes());
    }

    /// A CODEX_HOME that points Codex at the fake server, and a work folder.
    fn setup() -> PathBuf {
        static ONCE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let port = mock_server();
            let root = std::env::temp_dir().join(format!("hq-codex-live-{}", std::process::id()));
            fs_create(&root.join("home"));
            fs_create(&root.join("work"));
            // gpt-5.5 gets the classic tools (exec_command, apply_patch), which the fake server scripts.
            let config = format!("model = \"gpt-5.5\"\nmodel_provider = \"mock\"\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nwire_api = \"responses\"\n");
            std::fs::write(root.join("home").join("config.toml"), config).unwrap();
            std::env::set_var("CODEX_HOME", root.join("home"));
            root.join("work")
        })
        .clone()
    }

    fn fs_create(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
    }

    struct Live {
        child: Child,
        input: mpsc::Sender<String>,
        lines: mpsc::Receiver<Value>,
        seen: Vec<Value>,
    }

    fn start(resume: Option<&str>, fork: bool, mode: &str) -> Live {
        let work = setup();
        let opts = SpawnOptions {
            cwd: work.to_string_lossy().into_owned(),
            resume: resume.map(str::to_owned),
            permission_mode: Some(mode.into()),
            model: None,
            effort: None,
            fork,
            auto_tasks: false,
            max_agents: None,
        };
        let mut cmd = app_server();
        cmd.current_dir(&work).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn().expect("codex on PATH");
        let (input, lines) = translate(child.stdin.take().unwrap(), child.stdout.take().unwrap(), opts).unwrap();
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

        fn until(&mut self, what: &str, secs: u64, matches: impl Fn(&Value) -> bool) -> Value {
            let deadline = Instant::now() + Duration::from_secs(secs);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let v = self.lines.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}; seen: {:#?}", self.seen));
                self.seen.push(v.clone());
                if matches(&v) {
                    return v;
                }
            }
        }

        fn reply(&mut self, id: &str) -> Value {
            let id = id.to_owned();
            self.until(&id, 60, |v| v["type"] == "control_response" && v["response"]["request_id"] == id.as_str())["response"].clone()
        }

        fn result(&mut self) -> Value {
            self.until("result", 90, |v| v["type"] == "result")
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            kill_group(self.child.id());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    #[ignore]
    fn live_turn_with_tools_approvals_history_and_resume() {
        let work = setup();
        let _ = std::fs::remove_file(work.join("hello.txt"));
        let mut c = start(None, false, "default");
        c.control("init", json!({ "subtype": "initialize" }));
        let init = c.reply("init");
        let models = init["response"]["models"].as_array().unwrap();
        assert_eq!(models[0]["value"], "default");
        assert!(models.len() > 2, "{models:?}");

        c.prompt("do the thing");
        let begun = c.until("init", 30, |v| v["type"] == "system" && v["subtype"] == "init");
        let thread = begun["session_id"].as_str().unwrap().to_owned();
        assert_eq!(begun["permissionMode"], "default");
        let bash = c.until("Bash", 30, |v| v["type"] == "assistant" && v["message"]["content"][0]["name"] == "Bash");
        assert_eq!(bash["message"]["content"][0]["input"]["command"], "ls");
        // Read-only mode asks before the new file is written.
        let ask = c.until("write approval", 30, |v| v["type"] == "control_request");
        assert_eq!(ask["request"]["tool_name"], "Write");
        assert_eq!(ask["request"]["input"]["content"], "hello\nworld\n");
        let rid = ask["request_id"].as_str().unwrap().to_owned();
        c.send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": rid, "response": { "behavior": "allow", "updatedInput": {} } } }));
        let ask = c.until("edit approval", 30, |v| v["type"] == "control_request");
        assert_eq!(ask["request"]["tool_name"], "Edit");
        assert_eq!(ask["request"]["input"]["new_string"], "hello\nthere");
        let rid = ask["request_id"].as_str().unwrap().to_owned();
        c.send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": rid, "response": { "behavior": "allow", "updatedInput": {}, "updatedPermissions": [{ "type": "setMode" }] } } }));
        let patched = c.until("patch result", 30, |v| v["type"] == "user" && v.get("tool_use_result").is_some());
        assert_eq!(patched["tool_use_result"]["structuredPatch"][0]["lines"], json!([" hello", "-world", "+there"]));
        let result = c.result();
        assert_eq!(result["subtype"], "success", "{:#?}", c.seen);
        assert_eq!(std::fs::read_to_string(work.join("hello.txt")).unwrap().trim_end(), "hello\nthere");
        assert!(c.seen.iter().any(|v| v["type"] == "stream_event" && v["event"]["delta"]["text"] == "Let me look."));
        assert!(c.seen.iter().any(|v| v["type"] == "assistant" && v["message"]["content"][0]["type"] == "thinking"));
        // "Allow all edits this session" moved the cell to Edit automatically.
        c.control("s", json!({ "subtype": "get_settings" }));
        assert!(c.reply("s")["response"]["applied"]["model"].as_str().is_some());

        // A side question answers from a throwaway fork.
        c.control("side", json!({ "subtype": "side_question", "question": "Progress check" }));
        let side = c.reply("side");
        assert_eq!(side["subtype"], "success", "{side:?}");
        assert!(side["response"]["response"].as_str().unwrap().contains("Progress: 50%"));
        drop(c);

        // The history comes back from Codex, through the helper.
        let chunk = load_transcript(&thread, None).unwrap();
        let text = serde_json::to_string(&chunk).unwrap();
        assert!(text.contains("do the thing") && text.contains("Write") && text.contains("All done here"), "{text}");
        assert!(!text.contains("Progress check"), "the side question leaked into the thread");
        let listed = list_sessions(Some(&work.to_string_lossy()), 20).unwrap();
        assert!(listed.iter().any(|s| s["sessionId"] == thread.as_str()), "{listed:?}");

        // Resumed, the thread carries on; forked, it goes on under a new id.
        let mut c = start(Some(&thread), false, "acceptEdits");
        c.prompt("PLAIN again");
        let init = c.until("init", 30, |v| v["type"] == "system" && v["subtype"] == "init");
        assert_eq!(init["session_id"], thread.as_str());
        assert_eq!(c.result()["subtype"], "success");
        drop(c);
        let mut c = start(Some(&thread), true, "acceptEdits");
        c.prompt("PLAIN forked");
        let init = c.until("init", 30, |v| v["type"] == "system" && v["subtype"] == "init");
        assert_ne!(init["session_id"], thread.as_str());
        assert_eq!(c.result()["subtype"], "success");
    }

    #[test]
    #[ignore]
    fn live_interrupt_steer_and_compact() {
        let mut c = start(None, false, "acceptEdits");
        c.prompt("SLOW one");
        c.until("init", 30, |v| v["type"] == "system" && v["subtype"] == "init");
        c.control("stop", json!({ "subtype": "interrupt" }));
        assert_eq!(c.reply("stop")["subtype"], "success");
        let result = c.result();
        assert_eq!(result["subtype"], "error_during_execution", "{:#?}", c.seen);

        // A prompt sent while a turn runs is read by that turn.
        c.prompt("SLOW two");
        c.until("init", 30, |v| v["type"] == "system" && v["subtype"] == "init");
        std::thread::sleep(Duration::from_millis(500));
        c.prompt("PLAIN steered");
        let result = c.result();
        assert_eq!(result["subtype"], "success", "{:#?}", c.seen);
        let inits = c.seen.iter().filter(|v| v["type"] == "system" && v["subtype"] == "init").count();

        c.prompt("/compact");
        c.until("compacting", 30, |v| v["type"] == "system" && v["subtype"] == "status" && v["status"] == "compacting");
        let result = c.result();
        assert!(c.seen.iter().any(|v| v["subtype"] == "compact_boundary"), "{:#?}", c.seen);
        assert_eq!(result["subtype"], "success", "{:#?}", c.seen);
        // The steered prompt started no turn of its own.
        assert_eq!(inits, 2, "{:#?}", c.seen);
    }
}

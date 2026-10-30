//! One `claude` child process per chat cell, spoken to over the stream-json protocol
//! (the same stdio protocol the Agent SDKs and the VS Code extension use).
//!
//! The backend is deliberately dumb: it never interprets messages. Each stdout line is
//! forwarded to the frontend verbatim (bar lines the frontend provably never uses, and batched
//! into arrays), and each frontend payload is written to stdin verbatim. Anything Anthropic adds
//! to the protocol flows through untouched.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::de::IgnoredAny;
use serde_json::{json, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};

const STDERR_TAIL: usize = 40;

/// Lines read within this long of a batch's first line go to the frontend as one message.
const BATCH_WINDOW: Duration = Duration::from_millis(16);
const BATCH_MAX_LINES: usize = 256;
const BATCH_MAX_BYTES: usize = 1024 * 1024;

/// Remote Control titles a session that has no name yet `<prefix>-<random>`, with the machine
/// name as the default prefix. HQ sets the folder name instead so such a session is still findable.
const REMOTE_PREFIX_VAR: &str = "CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX";
/// Claude Code withholds TaskCreate/TaskUpdate in `-p` mode unless this is set, which left HQ's
/// progress panel with no checklist to show.
const TODO_TOOLS_VAR: &str = "CLAUDE_CODE_ENABLE_TODO_TOOLS";
/// Claude Code refuses an Agent call once this many subagents run in the session (default 20).
/// It takes any whole number from 1.
const SUBAGENT_CAP_VAR: &str = "CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS";
/// Workflows have a gate of their own, which Claude Code only reads from 1 to 256.
const WORKFLOW_CAP_VAR: &str = "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS";
const WORKFLOW_CAP_MAX: u32 = 256;
/// What "no limit" sets the subagent cap to, as Claude Code has no value meaning unlimited.
const NO_SUBAGENT_CAP: u32 = 1_000_000;
/// Asks for a task list only where one helps, so a one-off question still gets a plain answer.
/// The Claude Console key a session marked for it bills to. In `-p` mode Claude Code always uses it
/// when it is set, but these outrank it in Claude Code's authentication precedence, so such a
/// session must not inherit them.
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";
const OUTRANK_API_KEY: [&str; 4] = ["ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"];
const TASK_PROMPT: &str = "You are running inside HQ, a dashboard where the user watches several sessions at once and judges progress from your task list. For work with three or more steps, or that will take more than a few minutes, create the tasks with TaskCreate before starting and keep them current with TaskUpdate: mark each one in_progress as you start it and completed as soon as it is done. Skip the task list for questions and quick single-step jobs.";

pub struct Session {
    /// Feeds the thread that owns claude's stdin. Dropping the sender closes the pipe.
    input: Mutex<Option<mpsc::Sender<String>>>,
    /// None for a cloud session (cloud.rs), which runs no process here.
    child: Option<Arc<Mutex<Child>>>,
    /// Ends what a cloud session has open; run once, when the session is killed.
    on_kill: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(windows)]
    _job: Option<job::Job>,
    /// The child leads a process group of its own (see `hide_console`); this is its ID.
    #[cfg(unix)]
    pgid: u32,
}

#[derive(Default)]
pub struct Sessions {
    next_id: AtomicU64,
    map: Mutex<HashMap<u64, Arc<Session>>>,
}

pub struct SpawnOptions {
    pub cwd: String,
    pub resume: Option<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// With `resume`: continue in a new session ID, leaving the original untouched.
    pub fork: bool,
    /// Appends HQ's task-list prompt. Off leaves Claude's system prompt untouched.
    pub auto_tasks: bool,
    /// The most subagents the session runs at once: 0 for no limit, None for Claude's own.
    pub max_agents: Option<u32>,
    /// Runs on the API key saved in HQ's settings instead of Claude's own login.
    pub api_key: bool,
    /// Gives the session HQ's own MCP server (mcp.rs), with a token of its own.
    pub mcp: bool,
}

/// The `claude` executable chosen in HQ's own settings, which beats `HQ_CLAUDE`.
static CLAUDE_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Sets (or, when blank, clears) the executable that every `claude` HQ starts uses. Applies to
/// the next process started; running sessions are left alone.
pub fn set_claude_path(path: Option<String>) {
    // A path pasted from Explorer's "Copy as path" comes wrapped in quotes.
    let path = path.map(|p| p.trim().trim_matches('"').trim().to_owned()).filter(|p| !p.is_empty());
    *CLAUDE_PATH.lock().unwrap() = path;
}

/// The `claude` executable: HQ's setting, else `HQ_CLAUDE`, else `claude` on PATH, else where the
/// native installer puts it. The last one lets HQ use a Claude Code it has just installed (see
/// setup.rs) without a restart: HQ's own PATH predates the install.
pub(crate) fn claude_program() -> String {
    CLAUDE_PATH
        .lock()
        .unwrap()
        .clone()
        .or_else(|| std::env::var("HQ_CLAUDE").ok().filter(|p| !p.is_empty()))
        .or_else(|| {
            let on_path = crate::setup::on_path(crate::setup::CLAUDE_EXE).is_some();
            let native = crate::setup::native_install().filter(|p| p.is_file());
            if on_path { None } else { native.map(|p| p.to_string_lossy().into_owned()) }
        })
        .unwrap_or_else(|| "claude".into())
}

/// Prepares every child HQ starts. Windows: no console window. Unix: its own process group, so
/// `kill_group` can end the shells and tools it starts (the job object's job, below). macOS: the
/// login shell's PATH, which an app launched from Finder lacks (see macos.rs).
pub(crate) fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(target_os = "macos")]
    cmd.env("PATH", crate::macos::path());
    #[cfg(not(any(unix, windows)))]
    let _ = cmd;
}

/// Ends every process in the group that `hide_console` gave the child at `pid`. Best called while the
/// child is still unreaped, when the ID cannot have been handed to another process.
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    if pid > 1 {
        // SAFETY: killpg only sends a signal.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_group(_pid: u32) {}

/// Asks a short-lived `claude` for the account's plan usage (the same numbers `/usage` shows)
/// and returns the reply's JSON. Used when no session is running to ask instead.
///
/// The probe skips MCP servers, claude.ai connectors and hooks, and saves no transcript, so it
/// starts quickly and leaves nothing behind.
pub fn probe_usage() -> Result<String, String> {
    let dir = std::env::temp_dir().join("hq-usage");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(claude_program());
    cmd.args([
        "-p",
        "--input-format", "stream-json",
        "--output-format", "stream-json",
        "--verbose",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--settings", r#"{"disableAllHooks":true,"disableClaudeAiConnectors":true}"#,
    ]);
    cmd.current_dir(&dir)
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("Could not start `{}` ({e})", claude_program()))?;
    #[cfg(windows)]
    let _job = job::Job::new().filter(|j| j.assign(&child));

    let mut stdin = child.stdin.take().ok_or("claude stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("claude stdout unavailable")?;
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["type"] == "control_response" && v["response"]["request_id"] == "hq-usage" {
                let _ = tx.send(v["response"].clone());
                break;
            }
        }
    });

    let request = |id: &str, subtype: &str| json!({ "type": "control_request", "request_id": id, "request": { "subtype": subtype } }).to_string();
    let result = writeln!(stdin, "{}", request("hq-init", "initialize"))
        .and_then(|_| writeln!(stdin, "{}", request("hq-usage", "get_usage")))
        .and_then(|_| stdin.flush())
        .map_err(|e| format!("write to claude failed: {e}"))
        .and_then(|_| rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "Claude did not report usage in time".to_string()));
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();

    let response = result?;
    if response["subtype"] == "error" {
        return Err(response["error"].as_str().unwrap_or("usage request failed").to_owned());
    }
    Ok(response["response"].to_string())
}

/// Runs `claude update` and returns what it printed. HQ only starts headless (`-p`) sessions,
/// so it calls this before sessions start rather than waiting on Claude's own auto-update.
///
/// The updater is left out of any job object: if HQ closes or the wait times out, it carries
/// on and finishes, instead of being killed halfway through replacing the binary.
pub fn update_claude() -> Result<String, String> {
    let mut cmd = Command::new(claude_program());
    cmd.arg("update")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    let child = cmd.spawn().map_err(|e| format!("Could not start `{}` ({e})", claude_program()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = rx
        .recv_timeout(Duration::from_secs(120))
        .map_err(|_| "`claude update` is still running".to_string())?
        .map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text.trim().to_owned())
    } else {
        Err(text.trim().to_owned())
    }
}

impl Sessions {
    /// Blocks while the process starts, so callers run it off the UI thread.
    pub fn spawn(&self, app: &AppHandle, opts: SpawnOptions, events: Channel<InvokeResponseBody>) -> Result<u64, String> {
        if !Path::new(&opts.cwd).is_dir() {
            return Err(format!("Folder does not exist: {}", opts.cwd));
        }

        let mut cmd = Command::new(claude_program());
        cmd.args([
            "-p",
            "--input-format", "stream-json",
            "--output-format", "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool", "stdio",
        ]);
        if let Some(id) = opts.resume.as_deref().filter(|s| !s.is_empty()) {
            cmd.args(["--resume", id]);
            if opts.fork {
                cmd.arg("--fork-session");
            }
        }
        // "default" is left to Claude's own settings rather than passed as a flag value.
        if let Some(mode) = opts.permission_mode.as_deref().filter(|s| !s.is_empty() && *s != "default") {
            cmd.args(["--permission-mode", mode]);
        }
        // HQ_MODEL pins a model for every HQ session without touching Claude's own settings.
        let model = opts.model.filter(|s| !s.is_empty()).or_else(|| std::env::var("HQ_MODEL").ok());
        if let Some(model) = model.as_deref().filter(|s| !s.is_empty()) {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = opts.effort.as_deref().filter(|s| !s.is_empty()) {
            cmd.args(["--effort", effort]);
        }
        // Set whatever the Automatic task lists setting says, since the composer's per-prompt toggle
        // needs the tools too; Claude only uses them when asked. A value the user set themselves wins,
        // and with the task tools off the prompt asking for them is left out.
        let tools_on = match std::env::var_os(TODO_TOOLS_VAR) {
            // Claude Code's own reading: only these values turn the tools on.
            Some(v) => matches!(v.to_string_lossy().trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            None => {
                cmd.env(TODO_TOOLS_VAR, "1");
                true
            }
        };
        if opts.auto_tasks && tools_on {
            cmd.args(["--append-system-prompt", TASK_PROMPT]);
        }
        // HQ's setting beats the inherited environment, but an `env` block in Claude's own settings
        // is applied after start-up and still wins over both.
        for (var, value) in agent_cap_env(opts.max_agents) {
            cmd.env(var, value);
        }
        if opts.api_key {
            let key = crate::store::api_key(app)
                .map_err(|e| format!("Could not read the API key from the keychain: {e}"))?
                .ok_or("No API key is saved in HQ's settings.")?;
            use_api_key(&mut cmd, &key);
        }
        // HQ's own MCP server (mcp.rs). A config file that cannot be written only costs the tools.
        let mut grant = if opts.mcp { crate::mcp::Grant::issue() } else { None };
        let mut config_file = None;
        match grant.as_ref().map(|g| g.claude_args()) {
            Some(Ok((args, file))) => {
                cmd.args(args);
                config_file = Some(file);
            }
            Some(Err(_)) => grant = None,
            None => {}
        }
        // A prefix the user set themselves wins.
        if std::env::var_os(REMOTE_PREFIX_VAR).is_none_or(|v| v.is_empty()) {
            cmd.env(REMOTE_PREFIX_VAR, remote_name_prefix(&opts.cwd));
        }
        cmd.current_dir(&opts.cwd)
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut cmd);

        let remove_config = || {
            if let Some(file) = &config_file {
                let _ = std::fs::remove_file(file);
            }
        };
        let mut child = cmd.spawn().map_err(|e| {
            remove_config();
            let hint = if cfg!(target_os = "macos") {
                "Is Claude Code installed? Install it with `curl -fsSL https://claude.ai/install.sh | bash`, or pick its location in HQ's settings."
            } else {
                "Is Claude Code installed and on PATH?"
            };
            format!("Could not start `{}` ({e}). {hint}", claude_program())
        })?;

        let stdin = child.stdin.take();
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            kill_group(child.id());
            let _ = child.kill();
            remove_config();
            return Err("claude pipes unavailable".into());
        };
        let id = self.register(app, child, stdin.map(spawn_stdin_writer), stdout, stderr, events);
        if let Some(grant) = grant {
            grant.bind(id);
        }
        if let Some(file) = config_file {
            crate::mcp::keep_config_file(id, file);
        }
        Ok(id)
    }

    /// Tracks a started child as a session: lines queued with `send` go to `input`, and `stdout`'s
    /// lines reach the frontend as described on `spawn`. A backend that translates another
    /// agent's protocol hands in its translator's ends here instead of the child's own pipes.
    pub(crate) fn register(
        &self,
        app: &AppHandle,
        child: Child,
        input: Option<mpsc::Sender<String>>,
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        events: Channel<InvokeResponseBody>,
    ) -> u64 {
        // Put the child in a kill-on-close job so closing a cell (or the app) also ends
        // any shells and tools Claude started underneath it.
        #[cfg(windows)]
        let job = job::Job::new().filter(|j| j.assign(&child));
        #[cfg(unix)]
        let pgid = child.id();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let child = Arc::new(Mutex::new(child));
        let session = Arc::new(Session {
            input: Mutex::new(input),
            child: Some(child.clone()),
            on_kill: Mutex::new(None),
            #[cfg(windows)]
            _job: job,
            #[cfg(unix)]
            pgid,
        });
        self.map.lock().unwrap().insert(id, session);

        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        spawn_stderr_reader(stderr, tail.clone());
        spawn_stdout_reader(app.clone(), id, stdout, events, move || {
            let code = wait_for_exit(&child);
            let stderr: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
            (code, stderr)
        });
        id
    }

    /// Tracks a session that runs on Anthropic's servers rather than in a process here: `input`
    /// and `stdout` are the translator's ends, as for `register`, and `on_kill` runs when the
    /// session is closed. It ends when `stdout` does.
    pub(crate) fn register_remote(
        &self,
        app: &AppHandle,
        input: mpsc::Sender<String>,
        stdout: impl Read + Send + 'static,
        events: Channel<InvokeResponseBody>,
        on_kill: impl FnOnce() + Send + 'static,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let session = Arc::new(Session {
            input: Mutex::new(Some(input)),
            child: None,
            on_kill: Mutex::new(Some(Box::new(on_kill))),
            #[cfg(windows)]
            _job: None,
            #[cfg(unix)]
            pgid: 0,
        });
        self.map.lock().unwrap().insert(id, session);
        spawn_stdout_reader(app.clone(), id, stdout, events, || (Some(0), Vec::new()));
        id
    }

    /// Queues a line for the session's writer thread and returns at once. Lines reach claude in the
    /// order they were queued, so callers must queue from one place in order (a sync command).
    pub fn send(&self, id: u64, line: String) -> Result<(), String> {
        if line.contains('\n') {
            return Err("payload must be a single JSON line".into());
        }
        let session = self.get(id)?;
        let input = session.input.lock().unwrap();
        // The writer thread ends when claude stops reading, which means the session is going away.
        input.as_ref().and_then(|tx| tx.send(line).ok()).ok_or_else(|| "session is not running".to_string())
    }

    pub fn kill(&self, id: u64) {
        let Some(session) = self.map.lock().unwrap().remove(&id) else { return };
        // Ends the writer thread, which closes claude's stdin.
        session.input.lock().unwrap().take();
        if let Some(stop) = session.on_kill.lock().unwrap().take() {
            stop();
        }
        let Some(child) = &session.child else { return };
        // The reader polls and never holds the child lock for long, so this does not wait.
        let mut child = child.lock().unwrap();
        // Ends the shells and tools claude started too. Before the kill, under the lock the reaper
        // takes: while claude is unreaped its group ID cannot have been reused. (Already reaped, the
        // group is ended by `wait_for_exit`, and this repeat takes the same pid-wrap race it accepts.)
        #[cfg(unix)]
        kill_group(session.pgid);
        let _ = child.kill();
        drop(child);
        // Dropping the last Arc closes the job handle, which ends the whole process tree.
    }

    pub fn kill_all(&self) {
        let ids: Vec<u64> = self.map.lock().unwrap().keys().copied().collect();
        for id in ids {
            self.kill(id);
        }
    }

    fn get(&self, id: u64) -> Result<Arc<Session>, String> {
        self.map.lock().unwrap().get(&id).cloned().ok_or_else(|| "session is not running".into())
    }
}

fn spawn_stderr_reader(stderr: impl Read + Send + 'static, tail: Arc<Mutex<VecDeque<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let mut t = tail.lock().unwrap();
            if t.len() == STDERR_TAIL {
                t.pop_front();
            }
            t.push_back(line);
        }
    });
}

/// The variables that hold a session to `max` subagents at once. 0 lifts both caps as far as
/// Claude Code allows; None sets nothing, leaving Claude's own limits.
fn agent_cap_env(max: Option<u32>) -> Vec<(&'static str, String)> {
    let Some(max) = max else { return Vec::new() };
    let subagents = if max == 0 { NO_SUBAGENT_CAP } else { max };
    let workflow = if max == 0 { WORKFLOW_CAP_MAX } else { max.min(WORKFLOW_CAP_MAX) };
    vec![(SUBAGENT_CAP_VAR, subagents.to_string()), (WORKFLOW_CAP_VAR, workflow.to_string())]
}

/// Points the session at `key`, past any credential that would outrank it. An `env` block in
/// Claude's own settings still wins, so the frontend checks the `apiKeySource` Claude reports.
fn use_api_key(cmd: &mut Command, key: &str) {
    cmd.env(API_KEY_VAR, key);
    for var in OUTRANK_API_KEY {
        cmd.env_remove(var);
    }
}

fn remote_name_prefix(cwd: &str) -> String {
    match Path::new(cwd).file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()) {
        Some(folder) => format!("HQ-{folder}"),
        None => "HQ".into(),
    }
}

/// Writes queued lines to claude's stdin on a thread of its own, so a multi-MB image payload
/// never stalls the caller and lines still arrive in the order they were queued.
fn spawn_stdin_writer(mut stdin: ChildStdin) -> mpsc::Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in rx {
            let written = stdin
                .write_all(line.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush());
            if written.is_err() {
                break; // claude stopped reading; the stdout reader reports the exit
            }
        }
    });
    tx
}

/// What the stdout reader hands the batcher. The exit is queued behind every line, so it is
/// always the last thing the frontend sees.
enum Outgoing {
    Line(String),
    Exit(String),
}

/// True for a line worth forwarding: a JSON object the frontend can use.
///
/// Dropped lines are only those the frontend is certain to ignore, because a wrong drop loses
/// reply text while a wrong keep only costs a little time.
fn is_forwardable(line: &str) -> bool {
    line.starts_with('{') && !is_unused_stream_event(line) && serde_json::from_str::<IgnoredAny>(line).is_ok()
}

/// Streaming noise: events from a subagent (its final messages arrive as whole messages, and the
/// frontend returns early for these events), and deltas that carry no reply text. A large Write
/// streams thousands of `input_json_delta`s.
///
/// The patterns hold quotes that are escaped inside any string value, so they match JSON
/// structure only: text that merely quotes a pattern cannot make a line drop.
fn is_unused_stream_event(line: &str) -> bool {
    const UNUSED_DELTAS: [&str; 4] = [
        r#""delta":{"type":"input_json_delta""#,
        r#""delta":{"type":"thinking_delta""#,
        r#""delta":{"type":"signature_delta""#,
        r#""delta":{"type":"citations_delta""#,
    ];
    line.contains(r#""type":"stream_event""#)
        && (line.contains(r#""parent_tool_use_id":""#) || UNUSED_DELTAS.iter().any(|d| line.contains(d)))
}

fn spawn_stdout_reader(
    app: AppHandle,
    id: u64,
    stdout: impl Read + Send + 'static,
    events: Channel<InvokeResponseBody>,
    // Run once stdout ends: waits for the process (if any) and returns its exit code and last stderr lines.
    exit: impl FnOnce() -> (Option<i32>, Vec<String>) + Send + 'static,
) {
    let (tx, rx) = mpsc::channel::<Outgoing>();
    std::thread::spawn(move || run_batcher(rx, events));
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::with_capacity(64 * 1024);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim();
                    if is_forwardable(line) && tx.send(Outgoing::Line(line.to_owned())).is_err() {
                        break; // the batcher stopped: the frontend went away
                    }
                }
            }
        }
        let (code, stderr) = exit();
        let _ = tx.send(Outgoing::Exit(json!({ "type": "hq_exit", "code": code, "stderr": stderr }).to_string()));
        app.state::<Sessions>().map.lock().unwrap().remove(&id);
        crate::mcp::release(id);
    });
}

/// Polls instead of blocking in `wait()`, which would hold the child lock for the life of the
/// process and make `kill` wait for claude to end by itself.
///
/// On Unix it then ends what claude left running in its group, as closing the job does on Windows.
/// A group ID is not handed out again while any member lives; an empty group's could be in the
/// moment since the reap, a pid-wrap race too unlikely to guard against.
fn wait_for_exit(child: &Mutex<Child>) -> Option<i32> {
    loop {
        let mut child = child.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => {
                #[cfg(unix)]
                kill_group(child.id());
                return status.code();
            }
            Ok(None) => drop(child), // not held across the sleep
            Err(_) => return None,
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Sends the reader's lines to the frontend as JSON arrays, each holding what arrived within
/// `BATCH_WINDOW` of its first line. Tauri pays a webview call per message, and a parsed array
/// spares the frontend a second JSON parse per line.
///
/// The exit goes out in the last array, as its last element, and nothing follows it.
fn run_batcher(rx: mpsc::Receiver<Outgoing>, events: Channel<InvokeResponseBody>) {
    let mut batch = String::new();
    while let Ok(first) = rx.recv() {
        batch.push('[');
        let mut count = 0;
        let mut exited = false;
        let deadline = Instant::now() + BATCH_WINDOW;
        let mut item = Some(first);
        while let Some(out) = item.take() {
            let text = match out {
                Outgoing::Line(line) => line,
                Outgoing::Exit(exit) => {
                    exited = true;
                    exit
                }
            };
            if count > 0 {
                batch.push(',');
            }
            batch.push_str(&text);
            count += 1;
            if exited || count >= BATCH_MAX_LINES || batch.len() >= BATCH_MAX_BYTES {
                break;
            }
            // A timeout ends the batch, and so does a closed queue (the next recv() then ends the loop).
            item = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok();
        }
        batch.push(']');
        if events.send(InvokeResponseBody::Json(std::mem::take(&mut batch))).is_err() || exited {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_subagent_stream_events() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},"session_id":"s","parent_tool_use_id":"toolu_1","uuid":"u"}"#;
        assert!(is_unused_stream_event(line));
    }

    #[test]
    fn drops_deltas_without_reply_text() {
        for delta in ["input_json_delta\",\"partial_json\":\"{\\\"a\\\"", "thinking_delta\",\"thinking\":\"hm", "signature_delta\",\"signature\":\"abc", "citations_delta\",\"citation\":{}"] {
            let line = format!(r#"{{"type":"stream_event","event":{{"type":"content_block_delta","index":1,"delta":{{"type":"{delta}"}}}},"session_id":"s","parent_tool_use_id":null,"uuid":"u"}}"#);
            assert!(is_unused_stream_event(&line), "{line}");
        }
    }

    #[test]
    fn keeps_everything_the_frontend_can_use() {
        let kept = [
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}},"session_id":"s","parent_tool_use_id":null,"uuid":"u"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"future_delta"}},"parent_tool_use_id":null}"#,
            // A subagent's tool call arrives as a whole message that the frontend renders.
            r#"{"type":"assistant","message":{"content":[]},"parent_tool_use_id":"toolu_1"}"#,
            // Reply text that quotes the patterns carries them escaped.
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"\"parent_tool_use_id\":\"x\" and \"delta\":{\"type\":\"input_json_delta\""}},"parent_tool_use_id":null}"#,
        ];
        for line in kept {
            assert!(!is_unused_stream_event(line), "{line}");
            assert!(is_forwardable(line), "{line}");
        }
    }

    #[test]
    fn forwards_only_valid_json_objects() {
        assert!(!is_forwardable(""));
        assert!(!is_forwardable("[1]"));
        assert!(!is_forwardable("plain text from a wrapper"));
        assert!(!is_forwardable(r#"{"type":"assistant""#));
        assert!(!is_forwardable(r#"{"a":1} trailing"#));
        assert!(is_forwardable(r#"{"type":"system"}"#));
    }

    fn collecting_channel() -> (Channel<InvokeResponseBody>, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = sent.clone();
        let channel = Channel::new(move |body| {
            let InvokeResponseBody::Json(json) = body else { panic!("expected a JSON body") };
            sink.lock().unwrap().push(json);
            Ok(())
        });
        (channel, sent)
    }

    #[test]
    fn batches_lines_into_arrays_and_sends_the_exit_last() {
        let (channel, sent) = collecting_channel();
        let (tx, rx) = mpsc::channel();
        let batcher = std::thread::spawn(move || run_batcher(rx, channel));
        for i in 0..600 {
            tx.send(Outgoing::Line(format!(r#"{{"n":{i}}}"#))).unwrap();
        }
        tx.send(Outgoing::Exit(r#"{"type":"hq_exit","code":0,"stderr":[]}"#.into())).unwrap();
        batcher.join().unwrap();

        let batches: Vec<Vec<Value>> = sent.lock().unwrap().iter().map(|b| serde_json::from_str(b).unwrap()).collect();
        assert!((3..300).contains(&batches.len()), "{} batches", batches.len());
        assert!(batches.iter().all(|b| b.len() <= BATCH_MAX_LINES));
        let all: Vec<&Value> = batches.iter().flatten().collect();
        assert_eq!(all.len(), 601);
        for (i, v) in all[..600].iter().enumerate() {
            assert_eq!(v["n"], i);
        }
        assert_eq!(all[600]["type"], "hq_exit");
        // Nothing can follow the exit: the batcher is gone.
        assert!(tx.send(Outgoing::Line("{}".into())).is_err());
    }

    #[test]
    fn a_slow_trickle_still_arrives_promptly() {
        let (channel, sent) = collecting_channel();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || run_batcher(rx, channel));
        tx.send(Outgoing::Line(r#"{"n":1}"#.into())).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(*sent.lock().unwrap(), [r#"[{"n":1}]"#]);
        tx.send(Outgoing::Exit(r#"{"type":"hq_exit"}"#.into())).unwrap();
    }

    #[test]
    fn the_batcher_stops_when_the_frontend_is_gone() {
        let channel = Channel::<InvokeResponseBody>::new(|_| Err(tauri::Error::WebviewNotFound));
        let (tx, rx) = mpsc::channel();
        let batcher = std::thread::spawn(move || run_batcher(rx, channel));
        tx.send(Outgoing::Line("{}".into())).unwrap();
        batcher.join().unwrap();
        assert!(tx.send(Outgoing::Line("{}".into())).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn kill_does_not_wait_for_the_exit_poller() {
        let child = Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        let child = Arc::new(Mutex::new(child));
        let waiter = {
            let child = child.clone();
            std::thread::spawn(move || wait_for_exit(&child))
        };
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        child.lock().unwrap().kill().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(waiter.join().unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn the_stdin_writer_keeps_order_and_closes_the_pipe_when_dropped() {
        let mut child = Command::new("findstr").arg("^").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let tx = spawn_stdin_writer(child.stdin.take().unwrap());
        for i in 0..200 {
            tx.send(format!("line {i}")).unwrap();
        }
        drop(tx);
        let out: Vec<String> = BufReader::new(child.stdout.take().unwrap()).lines().map(Result::unwrap).collect();
        child.wait().unwrap();
        assert_eq!(out.len(), 200);
        assert_eq!((out[0].as_str(), out[199].as_str()), ("line 0", "line 199"));
    }

    #[cfg(unix)]
    #[test]
    fn names_remote_sessions_after_the_folder_on_unix() {
        assert_eq!(remote_name_prefix("/Users/me/HQ"), "HQ-HQ");
        assert_eq!(remote_name_prefix("/Users/me/my app/"), "HQ-my app");
        assert_eq!(remote_name_prefix("/"), "HQ");
        assert_eq!(remote_name_prefix(""), "HQ");
    }

    #[test]
    fn the_agent_limit_sets_both_caps_and_zero_lifts_them() {
        let env = |max| agent_cap_env(max).into_iter().map(|(var, v)| format!("{var}={v}")).collect::<Vec<_>>();
        assert_eq!(env(Some(3)), ["CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS=3", "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS=3"]);
        assert_eq!(env(Some(0)), ["CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS=1000000", "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS=256"]);
        assert_eq!(env(Some(300)), ["CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS=300", "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS=256"]);
        assert!(env(None).is_empty());
    }

    #[test]
    fn an_api_key_session_sets_the_key_and_drops_what_outranks_it() {
        let mut cmd = Command::new("claude");
        use_api_key(&mut cmd, "sk-ant-api03-test");
        let envs: HashMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        assert_eq!(envs[API_KEY_VAR].as_deref(), Some("sk-ant-api03-test"));
        for var in OUTRANK_API_KEY {
            assert_eq!(envs[var], None, "{var} must be removed");
        }
    }

    #[cfg(windows)]
    #[test]
    fn names_remote_sessions_after_the_folder() {
        assert_eq!(remote_name_prefix(r"D:\Repos\HQ"), "HQ-HQ");
        assert_eq!(remote_name_prefix(r"D:\Repos\my app\"), "HQ-my app");
        assert_eq!(remote_name_prefix(r"D:\"), "HQ");
        assert_eq!(remote_name_prefix(""), "HQ");
    }
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);
    // The handle is only used for assignment and CloseHandle, both thread-safe.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> Option<Job> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Job(h))
            }
        }

        pub fn assign(&self, child: &Child) -> bool {
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

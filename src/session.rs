//! One `claude` child process per chat cell, spoken to over the stream-json protocol
//! (the same stdio protocol the Agent SDKs and the VS Code extension use).
//!
//! The backend is deliberately dumb: it never interprets messages. Each stdout line is
//! forwarded to the frontend verbatim, and each frontend payload is written to stdin
//! verbatim. Anything Anthropic adds to the protocol flows through untouched.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

const STDERR_TAIL: usize = 40;

pub struct Session {
    stdin: Mutex<Option<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    #[cfg(windows)]
    _job: Option<job::Job>,
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
}

fn claude_program() -> String {
    std::env::var("HQ_CLAUDE").unwrap_or_else(|_| "claude".into())
}

fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

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
    let _ = child.kill();
    let _ = child.wait();

    let response = result?;
    if response["subtype"] == "error" {
        return Err(response["error"].as_str().unwrap_or("usage request failed").to_owned());
    }
    Ok(response["response"].to_string())
}

impl Sessions {
    pub fn spawn(&self, app: &AppHandle, opts: SpawnOptions, events: Channel<String>) -> Result<u64, String> {
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
        cmd.current_dir(&opts.cwd)
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            format!("Could not start `{}` ({e}). Is Claude Code installed and on PATH?", claude_program())
        })?;

        // Put the child in a kill-on-close job so closing a cell (or the app) also ends
        // any shells and tools Claude started underneath it.
        #[cfg(windows)]
        let job = job::Job::new().filter(|j| j.assign(&child));

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("claude stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("claude stderr unavailable")?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let child = Arc::new(Mutex::new(child));
        let session = Arc::new(Session {
            stdin: Mutex::new(stdin),
            child: child.clone(),
            #[cfg(windows)]
            _job: job,
        });
        self.map.lock().unwrap().insert(id, session);

        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        spawn_stderr_reader(stderr, tail.clone());
        spawn_stdout_reader(app.clone(), id, stdout, child, tail, events);
        Ok(id)
    }

    pub fn send(&self, id: u64, line: &str) -> Result<(), String> {
        if line.contains('\n') {
            return Err("payload must be a single JSON line".into());
        }
        let session = self.get(id)?;
        let mut guard = session.stdin.lock().unwrap();
        let stdin = guard.as_mut().ok_or("session input is closed")?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("write to claude failed: {e}"))
    }

    pub fn kill(&self, id: u64) {
        let Some(session) = self.map.lock().unwrap().remove(&id) else { return };
        session.stdin.lock().unwrap().take();
        let _ = session.child.lock().unwrap().kill();
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

fn spawn_stdout_reader(
    app: AppHandle,
    id: u64,
    stdout: impl Read + Send + 'static,
    child: Arc<Mutex<Child>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    events: Channel<String>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::with_capacity(64 * 1024);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end();
                    if !line.is_empty() && events.send(line.to_string()).is_err() {
                        break; // frontend went away
                    }
                }
            }
        }
        let code = child.lock().unwrap().wait().ok().and_then(|s| s.code());
        let stderr: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
        let _ = events.send(json!({ "type": "hq_exit", "code": code, "stderr": stderr }).to_string());
        app.state::<Sessions>().map.lock().unwrap().remove(&id);
    });
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

//! First-run setup: installs Claude Code with Anthropic's own installer when it is missing, and
//! signs it in with `claude auth login`, so a new user gets from installing HQ to a working
//! session without a terminal. HQ never handles the credentials: Claude's login flow runs in
//! the browser and Claude stores what it gets.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::{claude_program, hide_console};

#[cfg(windows)]
pub const CLAUDE_EXE: &str = "claude.exe";
#[cfg(not(windows))]
pub const CLAUDE_EXE: &str = "claude";

/// Generous: the installer downloads a binary of about 200 MB, and a slow line takes minutes.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
/// What an error keeps of the installer's output.
const OUTPUT_TAIL: usize = 1500;

/// The sign-in window HQ opened, ended once Claude reports a login (Windows only: a Mac's
/// Terminal window belongs to Terminal).
static LOGIN: Mutex<Option<Child>> = Mutex::new(None);

/// The PATH children are started with.
fn child_path() -> Option<std::ffi::OsString> {
    #[cfg(target_os = "macos")]
    return Some(crate::macos::path());
    #[cfg(not(target_os = "macos"))]
    std::env::var_os("PATH")
}

/// `name` in one of the PATH folders.
pub fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&child_path()?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// Where Anthropic's native installer puts `claude`, whether or not it is there.
pub fn native_install() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local").join("bin").join(CLAUDE_EXE))
}

/// The file `program` names: a path as it is (with `.exe` added on Windows when it has no
/// extension), a bare name looked up on PATH.
fn resolve(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
        let exe = path.with_extension("exe");
        return (cfg!(windows) && path.extension().is_none() && exe.is_file()).then_some(exe);
    }
    if cfg!(windows) && path.extension().is_none() {
        on_path(&format!("{program}.exe"))
    } else {
        on_path(program)
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeStatus {
    /// Whether the `claude` HQ would start exists.
    installed: bool,
    program: String,
    /// From `claude auth status`; `None` when it could not tell (an old Claude Code, a timeout).
    logged_in: Option<bool>,
}

/// Whether Claude Code is installed and signed in.
pub fn status() -> ClaudeStatus {
    let program = claude_program();
    let installed = resolve(&program).is_some();
    let logged_in = if installed { auth_status(&program) } else { None };
    ClaudeStatus { installed, program, logged_in }
}

/// `claude auth status` exits 0 when signed in and 1 with a JSON report when not. Anything else
/// (a Claude Code too old to have the command, a hang) says nothing either way.
fn auth_status(program: &str) -> Option<bool> {
    let mut cmd = Command::new(program);
    cmd.args(["auth", "status"]).env_remove("CLAUDECODE");
    let (code, out) = run(cmd, STATUS_TIMEOUT).ok()?;
    match code {
        Some(0) => Some(true),
        Some(1) if serde_json::from_str::<serde_json::Value>(out.trim()).is_ok_and(|v| v.is_object()) => Some(false),
        _ => None,
    }
}

/// Runs Anthropic's installer for this platform, hidden, and returns once `claude` is in place.
pub fn install() -> Result<String, String> {
    #[cfg(windows)]
    let cmd = {
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            // Windows PowerShell 5.1 may not offer TLS 1.2 by default.
            "[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; irm https://claude.ai/install.ps1 | iex",
        ]);
        cmd
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut cmd = Command::new("/bin/bash");
        cmd.args(["-c", "set -o pipefail; curl -fsSL https://claude.ai/install.sh | bash"]);
        cmd
    };
    let (code, out) = run(cmd, INSTALL_TIMEOUT)?;
    let installed = resolve(&claude_program()).is_some();
    match code {
        Some(0) if installed => Ok(out.trim().to_owned()),
        _ => {
            let out = out.trim();
            let tail = &out[out.char_indices().rev().nth(OUTPUT_TAIL).map_or(0, |(i, _)| i)..];
            Err(if tail.is_empty() { "The Claude Code installer failed.".to_owned() } else { format!("The Claude Code installer failed:\n{tail}") })
        }
    }
}

/// Opens a window running `claude auth login`, which opens the browser to sign in.
pub fn login() -> Result<(), String> {
    let program = claude_program();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        finish_login();
        let child = Command::new(&program)
            .args(["auth", "login"])
            .env_remove("CLAUDECODE")
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map_err(|e| format!("Could not start `{program}` ({e})"))?;
        *LOGIN.lock().unwrap() = Some(child);
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        crate::macos::run_in_terminal(&home, &format!("{} auth login", crate::macos::sh_quote(&program)), Some("zsh"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    Err(format!("Run `{program} auth login` in a terminal to sign in."))
}

/// Closes the sign-in window HQ opened, if it is still open.
pub fn finish_login() {
    if let Some(mut child) = LOGIN.lock().unwrap().take() {
        let _ = child.kill();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Runs `cmd` hidden with no input and returns its exit code and output (stdout, then stderr).
/// Past `timeout` it is ended.
fn run(mut cmd: Command, timeout: Duration) -> Result<(Option<i32>, String), String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("Could not start {:?} ({e})", cmd.get_program()))?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = reader(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = reader(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                crate::session::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{:?} did not finish in time", cmd.get_program()));
            }
        }
    };
    let text = format!("{}{}", out.join().unwrap_or_default(), err.join().unwrap_or_default());
    Ok((code, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_is_not_installed() {
        assert_eq!(resolve("/no/such/dir/claude"), None);
        assert_eq!(resolve("hq-no-such-program-anywhere"), None);
    }

    #[test]
    fn a_program_on_path_resolves() {
        let found = if cfg!(windows) { resolve("cmd") } else { resolve("sh") };
        assert!(found.is_some());
    }

    #[test]
    fn the_native_install_is_under_local_bin() {
        let path = native_install().expect("home is set");
        assert!(path.ends_with(Path::new(".local").join("bin").join(CLAUDE_EXE)));
    }
}

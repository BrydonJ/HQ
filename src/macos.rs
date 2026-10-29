//! macOS-only pieces: the PATH an app launched from Finder lacks, and the terminal and VS Code
//! launchers (Windows uses wt.exe and Code.exe for these, see main.rs and store.rs).

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

// ---------- PATH ----------

/// An app started from Finder or the Dock gets launchd's PATH (`/usr/bin:/bin:/usr/sbin:/sbin`),
/// so `claude`, `hermes`, `code` and `node` are not found. The user's login shell knows the real one.
/// Generous, because a profile that loads nvm or conda is slow and a miss is cached until HQ
/// restarts; it runs on its own thread, so it delays nothing unless a child is started sooner.
const SHELL_TIMEOUT: Duration = Duration::from_secs(8);
const MARK: &str = "__HQ_PATH__";

static PATH: OnceLock<OsString> = OnceLock::new();

/// Starts working out the PATH on a thread of its own, so a slow shell profile delays no window.
/// The first `path()` call waits for it if it is still running.
pub fn warm_path() {
    std::thread::spawn(|| {
        path();
    });
}

/// The PATH children are started with: the login shell's, then HQ's own, then the usual install
/// folders. Computed once. It is handed to each child (`Command::env`) rather than set on HQ,
/// because changing the environment of a running multi-threaded process is not safe.
pub fn path() -> OsString {
    PATH.get_or_init(compute_path).clone()
}

fn compute_path() -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(shell_path) = login_shell_path() {
        dirs.extend(std::env::split_paths(&shell_path));
    }
    if let Some(own) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&own));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".cargo/bin"] {
            dirs.push(home.join(rel));
        }
    }
    for fixed in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        dirs.push(PathBuf::from(fixed));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && d.is_absolute() && seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// Asks `$SHELL -ilc` for its PATH (interactive too, since nvm and the like are set up in
/// `.zshrc`). The output is wrapped in markers because the profile may print banners of its own.
/// Gives up after `SHELL_TIMEOUT` and ends the shell.
fn login_shell_path() -> Option<OsString> {
    let shell = std::env::var("SHELL").ok().filter(|s| Path::new(s).is_absolute()).unwrap_or_else(|| "/bin/zsh".into());
    // fish keeps PATH as a list, which would print as several marked strings.
    let path = if shell.ends_with("/fish") { "(string join : $PATH)" } else { "\"$PATH\"" };
    let mut cmd = Command::new(shell);
    cmd.args(["-ilc", &format!("printf '{MARK}%s{MARK}' {path}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        // A session of its own, with no controlling terminal: when HQ is started from one
        // (dev-mac.sh), an interactive shell would otherwise reach for its job control and be
        // stopped. setsid also makes the shell a group leader, which `kill_group` relies on.
        // SAFETY: setsid is async-signal-safe and touches no memory of this process.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let out = match rx.recv_timeout(SHELL_TIMEOUT) {
        Ok(out) => out,
        Err(_) => {
            crate::session::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let _ = child.wait();
    let text = String::from_utf8_lossy(&out);
    let start = text.find(MARK)? + MARK.len();
    let end = start + text[start..].find(MARK)?;
    let found = &text[start..end];
    (!found.is_empty()).then(|| OsString::from(found))
}

// ---------- launchers ----------

/// Runs `/usr/bin/open -a app target` and reports its failure. `open` returns as soon as it has handed over.
fn open_with(app: &str, target: &Path) -> Result<(), String> {
    let out = Command::new("/usr/bin/open").args(["-a", app]).arg(target).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if why.is_empty() { "open failed".into() } else { why })
    }
}

fn absolute(dir: &str) -> Result<(), String> {
    if Path::new(dir).is_absolute() {
        Ok(())
    } else {
        Err("folder path must be absolute".into())
    }
}

/// A new Terminal window in `dir` (the home folder when none is given).
pub fn open_terminal(dir: Option<&str>) -> Result<(), String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let dir = dir.unwrap_or(&home);
    absolute(dir)?;
    open_with("Terminal", Path::new(dir))
}

/// Opens `path` (a folder or a `.code-workspace` file) in VS Code: its `code` command when it can
/// be found, else through `open -a`.
pub fn open_in_vscode(path: &str) -> Result<(), String> {
    absolute(path)?;
    if let Some(code) = find_vscode() {
        let mut child = Command::new(code).arg(path).stdin(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            let _ = child.wait(); // reaped, so it does not linger as a zombie
        });
        return Ok(());
    }
    open_with("Visual Studio Code", Path::new(path)).map_err(|_| "VS Code was not found".to_string())
}

/// The VS Code `code` command: inside the app bundle when it is installed in the usual places,
/// else on PATH.
pub fn find_vscode() -> Option<PathBuf> {
    const REL: &str = "Visual Studio Code.app/Contents/Resources/app/bin/code";
    let mut candidates = vec![PathBuf::from("/Applications").join(REL)];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications").join(REL));
    }
    candidates.extend(std::env::split_paths(&path()).map(|dir| dir.join("code")));
    candidates.into_iter().find(|p| p.is_file())
}

/// `pwsh` when it is on PATH.
fn find_pwsh() -> Option<PathBuf> {
    std::env::split_paths(&path()).map(|dir| dir.join("pwsh")).find(|p| p.is_file())
}

/// Single-quotes `s` for a POSIX shell.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Opens Terminal in `path` running `command` (a shell block from a reply, run on the user's
/// click), then leaves a shell open there. The command goes in a `.command` script rather than on
/// a command line, so no quoting can change it. `shell` picks the interpreter (`bash`, `sh`,
/// `zsh`, or `powershell` for pwsh when installed); anything else is zsh, the Mac default.
pub fn run_in_terminal(path_: &str, command: &str, shell: Option<&str>) -> Result<(), String> {
    absolute(path_)?;
    let script = write_run_script(path_, command, shell)?;
    open_with("Terminal", &script)
}

fn write_run_script(cwd: &str, command: &str, shell: Option<&str>) -> Result<PathBuf, String> {
    let io = |e: std::io::Error| format!("could not write the script ({e})");
    let dir = std::env::temp_dir().join("hq-run");
    // Private to the user, so nobody else can swap a script between writing it and running it.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(io)?;
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            if old.is_some_and(|age| age.as_secs() > 24 * 60 * 60) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let command = command.replace("\r\n", "\n");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);

    // What the script does last: for a POSIX shell the command itself, for PowerShell a hand-over
    // to a .ps1 that echoes it and then runs it.
    let (interpreter, body) = match shell {
        Some("powershell") => {
            let pwsh = find_pwsh().ok_or("PowerShell (pwsh) is not installed")?;
            let ps1 = dir.join(format!("run-{stamp}.ps1"));
            let echo: String = command.chars().flat_map(|c| match c {
                '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => vec![c, c],
                _ => vec![c],
            }).collect();
            std::fs::write(&ps1, format!("Write-Host '> {echo}' -ForegroundColor DarkGray\n{command}\n")).map_err(io)?;
            let launch = format!("{} -NoLogo -NoExit -File {}\n", sh_quote(&pwsh.to_string_lossy()), sh_quote(&ps1.to_string_lossy()));
            ("/bin/zsh", launch)
        }
        other => {
            let interpreter = match other {
                Some("bash") => "/bin/bash",
                Some("sh") => "/bin/sh",
                _ => "/bin/zsh",
            };
            let echo = format!("printf '\\033[90m> %s\\033[0m\\n' {}\n", sh_quote(&command));
            (interpreter, format!("{echo}{command}\nprintf '\\n'\nexec \"${{SHELL:-/bin/zsh}}\" -l\n"))
        }
    };
    let script = format!(
        "#!{interpreter}\nexport PATH={}\ncd -- {} || exit 1\n{body}",
        sh_quote(&path().to_string_lossy()),
        sh_quote(cwd),
    );
    let file = dir.join(format!("run-{stamp}.command"));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // New file only (never one that is already there), runnable by the user alone.
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&file).map_err(io)?;
        out.write_all(script.as_bytes()).map_err(io)?;
    }
    Ok(file)
}

/// Whether this copy runs from an installed `.app` (`/Applications` or `~/Applications`), as
/// opposed to a DMG, a download folder, a translocated copy or a build under `target/`.
pub fn is_installed() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    // <root>/HQ.app/Contents/MacOS/hq
    exe.ancestors().nth(3).is_some_and(|app| {
        app.extension().is_some_and(|e| e == "app") && app.parent().is_some_and(|dir| roots.iter().any(|r| r == dir))
    })
}

// ---------- prompts ----------

/// The sound HQ plays when a session asks a question or wants approval.
const CUE: &str = "/System/Library/Sounds/Glass.aiff";

/// Plays the prompt cue through `afplay`, which returns once the sound has finished.
pub fn play_cue() {
    if let Ok(mut child) = Command::new("/usr/bin/afplay").arg(CUE).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait(); // reaped, so it does not linger as a zombie
        });
    }
}

/// Opens HQ's own page in System Settings › Notifications, where its style can be set to Alerts.
pub fn open_notification_settings(identifier: &str) -> Result<(), String> {
    let url = format!("x-apple.systempreferences:com.apple.preference.notifications?id={identifier}");
    let out = Command::new("/usr/bin/open").arg(url).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err("System Settings could not be opened".into())
    }
}

/// What the user did with a notification from `notify_actions`, sent to the UI as `notification-action`.
#[derive(serde::Serialize, Clone)]
struct NotificationAction {
    id: String,
    action: &'static str,
}

/// A notification with Approve and Deny buttons (NSUserNotification, the API the notification
/// plugin uses too). The buttons stay on screen only when HQ's style is Alerts in System Settings;
/// a banner shows them on hover. The answer waits on a thread of its own, so each open
/// notification holds one parked thread until it is answered, clicked or cleared.
pub fn notify_actions(app: tauri::AppHandle, id: String, title: String, body: String) {
    use mac_notification_sys::{MainButton, Notification, NotificationResponse};
    use tauri::Emitter;
    // Only the first call counts, whichever of this and the plugin comes first; same choice as the plugin.
    let _ = mac_notification_sys::set_application(if tauri::is_dev() { "com.apple.Terminal" } else { &app.config().identifier });
    std::thread::spawn(move || {
        let response = Notification::new()
            .title(&title)
            .message(&body)
            .main_button(MainButton::SingleAction("Approve"))
            .close_button("Deny")
            .send();
        let action = match response {
            Ok(NotificationResponse::ActionButton(_)) => "approve",
            Ok(NotificationResponse::CloseButton(_)) => "deny",
            Ok(NotificationResponse::Click) => "click",
            _ => return, // cleared without an answer, or not shown
        };
        let _ = app.emit("notification-action", NotificationAction { id, action });
    });
}

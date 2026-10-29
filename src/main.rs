#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "codex")]
mod codex;
#[cfg(debug_assertions)]
mod dev;
#[cfg(feature = "hermes")]
mod hermes;
#[cfg(not(target_os = "macos"))]
mod icon;
#[cfg(target_os = "macos")]
mod macos;
mod mcp;
mod secret;
mod session;
mod setup;
mod store;
mod update;
mod usage;

use std::path::Path;

use session::{Sessions, SpawnOptions};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State, UserAttentionType, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

/// Every message on `on_event` is a JSON array of the session's stdout lines (as objects), and
/// the last element of the last array is the `hq_exit` object. `backend` picks the agent:
/// `claude` (the default) or, when built in, `codex` or `hermes`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn spawn_session(
    app: AppHandle,
    cwd: String,
    resume: Option<String>,
    permission_mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    fork: Option<bool>,
    auto_tasks: Option<bool>,
    max_agents: Option<u32>,
    api_key: Option<bool>,
    backend: Option<String>,
    mcp: Option<bool>,
    on_event: Channel<InvokeResponseBody>,
) -> Result<u64, String> {
    let fork = fork.unwrap_or(false);
    let auto_tasks = auto_tasks.unwrap_or(true);
    let api_key = api_key.unwrap_or(false);
    let mcp = mcp.unwrap_or(false);
    let opts = SpawnOptions { cwd, resume, permission_mode, model, effort, fork, auto_tasks, max_agents, api_key, mcp };
    tauri::async_runtime::spawn_blocking(move || match backend.as_deref() {
        None | Some("claude") => app.state::<Sessions>().spawn(&app, opts, on_event),
        #[cfg(feature = "codex")]
        Some("codex") => codex::spawn(&app, opts, on_event),
        #[cfg(feature = "hermes")]
        Some("hermes") => hermes::spawn(&app, opts, on_event),
        Some(other) => Err(format!("This build of HQ has no {other} support")),
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Deliberately not `async`: sync commands run in the order the frontend sent them, and the
/// frontend relies on that (`initialize` goes first). Queuing a line takes no time.
#[tauri::command]
fn send_line(sessions: State<'_, Sessions>, id: u64, line: String) -> Result<(), String> {
    sessions.send(id, line)
}

#[tauri::command]
fn kill_session(sessions: State<'_, Sessions>, id: u64) {
    sessions.kill(id);
}

/// The frontend's answer to an HQ MCP tool call (see mcp.rs).
#[tauri::command]
fn mcp_reply(id: u64, text: String, is_error: Option<bool>) {
    mcp::reply(id, text, is_error.unwrap_or(false));
}

/// Registers HQ's tools for every Claude Code, Codex and Hermes on this machine, or removes them.
#[tauri::command]
async fn install_mcp_everywhere(profile: State<'_, Profile>, install: bool) -> Result<Vec<mcp::InstallResult>, String> {
    let profile = profile.0.clone();
    tauri::async_runtime::spawn_blocking(move || mcp::install_everywhere(profile.as_deref(), install)).await.map_err(|e| e.to_string())
}

/// Makes a git worktree on `branch` beside the repository holding `folder`, for a session an
/// agent starts through HQ's MCP server. Returns the worktree's path.
#[tauri::command]
async fn create_worktree(folder: String, branch: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || mcp::create_worktree(&folder, &branch)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pick_folder(window: WebviewWindow) -> Option<String> {
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_parent(&window)
            .set_title("Choose a folder for the new session")
            .blocking_pick_folder()
            .and_then(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

/// Asks where to save `text` (as UTF-8, a Markdown file by default) and writes it there, starting
/// in `directory` when given. Returns the chosen path, or `None` when the user cancels.
#[tauri::command]
async fn save_text_file(window: WebviewWindow, default_name: String, text: String, directory: Option<String>) -> Result<Option<String>, String> {
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app.dialog().file().set_parent(&window).set_title("Save as").set_file_name(&default_name).add_filter("Markdown", &["md"]);
        if let Some(dir) = directory.filter(|d| std::path::Path::new(d).is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        let chosen = dialog.blocking_save_file();
        let Some(chosen) = chosen else { return Ok(None) };
        let mut path = chosen.into_path().map_err(|e| e.to_string())?;
        if path.extension().is_none() {
            path.set_extension("md");
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sets the `claude` executable that sessions start from now on; blank goes back to the default.
#[tauri::command]
fn set_claude_path(path: Option<String>) {
    session::set_claude_path(path);
}

/// What the saved API key looks like (its last characters), or None when there is none. The key
/// itself never goes to the webview, and this never reads it from the keychain.
#[tauri::command]
async fn api_key_hint(app: AppHandle) -> Option<String> {
    store::saved_api_key_hint(&app)
}

/// Saves the API key that sessions switched to API credits run on (blank removes it) and returns its hint.
#[tauri::command]
async fn set_api_key(app: AppHandle, key: Option<String>) -> Result<Option<String>, String> {
    let key = key.filter(|k| !k.trim().is_empty()).map(|k| store::clean_api_key(&k)).transpose()?;
    let hint = key.as_deref().map(store::api_key_hint);
    store::set_api_key(&app, key)?;
    Ok(hint)
}

/// Picks the VS Code workspace file that the cell's VS Code button opens for this folder.
#[tauri::command]
async fn pick_workspace(window: WebviewWindow, dir: String) -> Option<String> {
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Choose a VS Code workspace for this folder")
            .add_filter("VS Code workspace", &["code-workspace"]);
        if Path::new(&dir).is_dir() {
            dialog = dialog.set_directory(&dir);
        }
        dialog.blocking_pick_file().and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
async fn find_workspaces(path: String, dirs: Vec<String>) -> Vec<store::WorkspaceFile> {
    tauri::async_runtime::spawn_blocking(move || store::find_workspaces(&path, &dirs)).await.unwrap_or_default()
}

#[tauri::command]
fn folder_exists(path: String) -> bool {
    Path::new(&path).is_dir()
}

#[tauri::command]
fn load_state(app: AppHandle) -> Result<Option<String>, String> {
    store::load_state(&app)
}

/// Not `async` for the same reason as `send_line`: boards must reach the disk in the order they
/// were saved. The write itself happens on a thread of its own, so this returns at once.
#[tauri::command]
fn save_state(app: AppHandle, json: String) -> Result<(), String> {
    store::save_state(&app, json)
}

#[tauri::command]
async fn load_transcript(session_id: String, before: Option<u64>) -> Result<store::TranscriptChunk, String> {
    tauri::async_runtime::spawn_blocking(move || store::load_transcript(&session_id, before))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(feature = "codex")]
#[tauri::command]
async fn load_codex_transcript(session_id: String, before: Option<u64>) -> Result<store::TranscriptChunk, String> {
    tauri::async_runtime::spawn_blocking(move || codex::load_transcript(&session_id, before))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(feature = "codex")]
#[tauri::command]
async fn list_codex_sessions(limit: Option<usize>, cwd: Option<String>) -> Result<Vec<serde_json::Value>, String> {
    let limit = limit.unwrap_or(80).min(500);
    tauri::async_runtime::spawn_blocking(move || codex::list_sessions(cwd.as_deref(), limit))
        .await
        .map_err(|e| e.to_string())?
}

/// Codex's model menu, for a Codex cell that has not started yet.
#[cfg(feature = "codex")]
#[tauri::command]
async fn codex_models() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(codex::list_models).await.map_err(|e| e.to_string())?
}

#[cfg(feature = "codex")]
#[tauri::command]
async fn update_codex() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(codex::update_codex).await.map_err(|e| e.to_string())?
}

/// Sets the `codex` executable that Codex cells start from now on; blank goes back to the default.
#[cfg(feature = "codex")]
#[tauri::command]
async fn set_codex_path(path: Option<String>) {
    // Off the main thread: stopping the helper waits on its lock, which a question can hold for a minute.
    let _ = tauri::async_runtime::spawn_blocking(move || codex::set_codex_path(path)).await;
}

#[cfg(feature = "hermes")]
#[tauri::command]
async fn load_hermes_transcript(app: AppHandle, session_id: String, cwd: Option<String>, before: Option<u64>) -> Result<store::TranscriptChunk, String> {
    tauri::async_runtime::spawn_blocking(move || hermes::load_transcript(&app, &session_id, cwd.as_deref(), before))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(feature = "hermes")]
#[tauri::command]
async fn list_hermes_sessions(limit: Option<usize>, cwd: Option<String>) -> Result<Vec<serde_json::Value>, String> {
    let limit = limit.unwrap_or(80).min(500);
    tauri::async_runtime::spawn_blocking(move || hermes::list_sessions(cwd.as_deref(), limit))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn list_sessions(app: AppHandle, limit: Option<usize>, cwd: Option<String>, agents: Option<bool>) -> Result<Vec<store::SessionInfo>, String> {
    let limit = limit.unwrap_or(80).min(500);
    let agents = agents.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || store::list_sessions(&app.state::<store::Index>(), limit, cwd.as_deref(), agents))
        .await
        .map_err(|e| e.to_string())?
}

/// Streams each matching message to `on_hit` and returns how many it sent. A newer search
/// cancels an older one, which then returns early with its count so far.
#[tauri::command]
async fn search_sessions(
    app: AppHandle,
    query: String,
    cwd: Option<String>,
    limit: Option<usize>,
    on_hit: Channel<store::SessionHit>,
) -> Result<usize, String> {
    let limit = limit.unwrap_or(100).clamp(1, 500);
    // Numbered here rather than on the blocking thread, so the order is that of the calls.
    let generation = app.state::<store::Index>().begin_search();
    tauri::async_runtime::spawn_blocking(move || {
        store::search_sessions(&app.state::<store::Index>(), generation, &query, cwd.as_deref(), limit, &on_hit)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Token usage and estimated cost per session since `since_ms` (all of it when absent), read from
/// Claude's transcripts on disk. Nothing is asked of Claude, so looking costs no tokens.
#[tauri::command]
async fn usage_report(app: AppHandle, since_ms: Option<i64>) -> Result<Vec<usage::SessionUsage>, String> {
    tauri::async_runtime::spawn_blocking(move || usage::report(&app.state::<store::Index>(), &app.state::<usage::UsageIndex>(), since_ms))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn probe_usage() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(session::probe_usage).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn update_claude() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(session::update_claude).await.map_err(|e| e.to_string())?
}

/// Whether Claude Code is installed and signed in, for the first-run setup card.
#[tauri::command]
async fn claude_status() -> Result<setup::ClaudeStatus, String> {
    tauri::async_runtime::spawn_blocking(setup::status).await.map_err(|e| e.to_string())
}

/// Installs Claude Code with Anthropic's installer. Takes a minute or more.
#[tauri::command]
async fn install_claude() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(setup::install).await.map_err(|e| e.to_string())?
}

/// Opens a window running `claude auth login`, which signs in through the browser.
#[tauri::command]
async fn login_claude() -> Result<(), String> {
    blocking(setup::login).await
}

/// Closes the sign-in window once Claude reports a login.
#[tauri::command]
fn finish_login() {
    setup::finish_login();
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http(s) links can be opened".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_folder(app: AppHandle, path: String) -> Result<(), String> {
    if !Path::new(&path).is_dir() {
        return Err("folder does not exist".into());
    }
    app.opener().open_path(path, None::<&str>).map_err(|e| e.to_string())
}

/// Runs a launcher on the blocking pool, off the main thread (where a sync command runs) and the
/// async workers: on a Mac launchers wait for the login shell's PATH, which can take seconds
/// right after launch.
async fn blocking(f: impl FnOnce() -> Result<(), String> + Send + 'static) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open_in_vscode(path: String) -> Result<(), String> {
    blocking(move || vscode_now(path)).await
}

#[tauri::command]
async fn open_terminal(path: Option<String>) -> Result<(), String> {
    blocking(move || terminal_now(path)).await
}

#[tauri::command]
async fn run_in_terminal(path: String, command: String, shell: Option<String>, lang: Option<String>) -> Result<(), String> {
    blocking(move || run_now(path, command, shell, lang)).await
}

/// Opens a folder, or a `.code-workspace` file, in VS Code. Nothing else is accepted.
fn vscode_now(path: String) -> Result<(), String> {
    let target = Path::new(&path);
    if store::is_workspace_file(target) {
        if !target.is_file() {
            return Err("workspace file does not exist".into());
        }
    } else if !target.is_dir() {
        return Err("folder does not exist".into());
    }
    #[cfg(target_os = "macos")]
    return macos::open_in_vscode(&path);
    #[cfg(not(target_os = "macos"))]
    {
        let code = store::find_vscode().ok_or("VS Code (Code.exe) was not found")?;
        std::process::Command::new(code).arg(&path).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Opens the default terminal (Windows Terminal, else a console window; Terminal on a Mac), in
/// `path` when given.
fn terminal_now(path: Option<String>) -> Result<(), String> {
    let dir = path.filter(|p| !p.is_empty());
    if let Some(dir) = &dir {
        if !Path::new(dir).is_dir() {
            return Err("folder does not exist".into());
        }
    }
    #[cfg(target_os = "macos")]
    return macos::open_terminal(dir.as_deref());
    #[cfg(not(target_os = "macos"))]
    {
        open_terminal_windows(dir)
    }
}

#[cfg(not(target_os = "macos"))]
fn open_terminal_windows(dir: Option<String>) -> Result<(), String> {
    let mut wt = std::process::Command::new("wt.exe");
    if let Some(dir) = &dir {
        // wt treats `;` as a command separator, even inside a path.
        wt.args(["-d", &dir.replace(';', "\\;")]);
    }
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut cmd = std::process::Command::new("cmd.exe");
    if let Some(dir) = &dir {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        cmd.creation_flags(CREATE_NEW_CONSOLE);
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Opens a terminal in `path` that runs `command` (a shell block from a reply, run on the user's
/// click) and stays open. The command goes in a script file rather than on the command line, so
/// neither wt's `;` splitting nor shell quoting can change it. `shell` is `cmd` for batch, else
/// PowerShell (pwsh when installed, since it understands `&&` like the shells the blocks are
/// usually written for).
fn run_now(path: String, command: String, shell: Option<String>, lang: Option<String>) -> Result<(), String> {
    if !Path::new(&path).is_dir() {
        return Err("folder does not exist".into());
    }
    if command.trim().is_empty() {
        return Err("nothing to run".into());
    }
    // On a Mac `lang` picks bash, zsh, sh or pwsh (the UI maps the block's language); `shell` is for Windows.
    #[cfg(target_os = "macos")]
    {
        let _ = shell;
        return macos::run_in_terminal(&path, &command, lang.as_deref());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = lang;
        run_in_terminal_windows(path, command, shell)
    }
}

#[cfg(not(target_os = "macos"))]
fn run_in_terminal_windows(path: String, command: String, shell: Option<String>) -> Result<(), String> {
    let cmd = shell.as_deref() == Some("cmd");
    let script = write_run_script(&command, cmd).map_err(|e| format!("could not write the script ({e})"))?;
    let script = script.to_string_lossy().into_owned();
    let program = if cmd { "cmd.exe".to_string() } else { powershell_program() };
    let args: Vec<String> = if cmd {
        vec!["/k".into(), script]
    } else {
        ["-NoLogo", "-NoExit", "-ExecutionPolicy", "Bypass", "-File"].iter().map(|s| s.to_string()).chain([script]).collect()
    };
    let mut wt = std::process::Command::new("wt.exe");
    // wt treats `;` as a command separator, even inside an argument.
    wt.args(["-d", &path.replace(';', "\\;"), &program]);
    wt.args(args.iter().map(|a| a.replace(';', "\\;")));
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut direct = std::process::Command::new(&program);
    direct.current_dir(&path).args(&args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        direct.creation_flags(CREATE_NEW_CONSOLE);
    }
    direct.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// `pwsh.exe` when it is on PATH, else Windows PowerShell.
#[cfg(not(target_os = "macos"))]
fn powershell_program() -> String {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join("pwsh.exe").is_file()))
        .unwrap_or(false);
    if found { "pwsh.exe" } else { "powershell.exe" }.into()
}

/// Writes the command to `%TEMP%\hq-run\`, as a `.cmd` or a `.ps1` that first echoes it. Scripts
/// older than a day are removed. A `.cmd` runs with batch rules (cmd re-reads it as it goes, and a
/// literal `%` must be `%%`); that is accepted for what are short, one-off commands.
#[cfg(not(target_os = "macos"))]
fn write_run_script(command: &str, cmd: bool) -> std::io::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("hq-run");
    std::fs::create_dir_all(&dir)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            if old.is_some_and(|age| age.as_secs() > 24 * 60 * 60) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let file = dir.join(format!("run-{stamp}.{}", if cmd { "cmd" } else { "ps1" }));
    let body = if cmd {
        // cmd reads scripts in the console's code page; switching to UTF-8 first keeps non-ASCII intact.
        format!("@chcp 65001 >nul\r\n{}\r\n", command.replace("\r\n", "\n").replace('\n', "\r\n"))
    } else {
        // A BOM, or Windows PowerShell reads the file as ANSI. In a single-quoted string only quotes are
        // special, and PowerShell takes the curly single quotes as quotes too, so each is doubled.
        let echo: String = command.chars().flat_map(|c| match c {
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => vec![c, c],
            _ => vec![c],
        }).collect();
        format!("\u{feff}Write-Host '> {echo}' -ForegroundColor DarkGray\r\n{command}\r\n")
    };
    std::fs::write(&file, body)?;
    Ok(file)
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<update::UpdateInfo>, String> {
    update::check(&app).await
}

#[tauri::command]
fn install_update(app: AppHandle) -> Result<(), String> {
    update::install(&app, true)
}

#[tauri::command]
fn request_attention(window: WebviewWindow) {
    if !window.is_focused().unwrap_or(false) {
        let _ = window.request_user_attention(Some(UserAttentionType::Informational));
    }
}

/// A freshly loaded page owns no cells, so any `claude` still running belonged to a page that
/// was reloaded (F5 in a dev build, or WebView2 recovering from a crash) and nobody can reach it.
#[tauri::command]
fn kill_all_sessions(sessions: State<'_, Sessions>) {
    sessions.kill_all();
}

/// A session that `hq.exe --new-session <folder> ...` asked for, waiting for the UI to open it.
#[derive(serde::Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct LaunchRequest {
    cwd: String,
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    prompt: Option<String>,
}

#[derive(Default)]
struct Launches(std::sync::Mutex<Vec<LaunchRequest>>);

/// Reads `--new-session <folder> [--model m] [--effort e] [--permission-mode p] [--prompt text]`
/// from a command line. A plain start asks for nothing.
fn parse_launch(args: &[String]) -> Option<LaunchRequest> {
    let value = |flag: &str| {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).filter(|v| !v.is_empty() && !v.starts_with("--")).cloned()
    };
    Some(LaunchRequest {
        cwd: value("--new-session")?,
        model: value("--model"),
        effort: value("--effort"),
        permission_mode: value("--permission-mode"),
        prompt: value("--prompt"),
    })
}

/// Hands the UI the sessions asked for on the command line since it last looked.
#[tauri::command]
fn take_launches(launches: State<'_, Launches>) -> Vec<LaunchRequest> {
    launches.0.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BuildInfo {
    profile: Option<String>,
    version: String,
    /// The UI is read from `ui/` on each load, so a reload shows edits without a rebuild.
    live_ui: bool,
    /// Whether this copy installs updates (the installed app, or a test build that opts in).
    updatable: bool,
    /// The agents this copy can start in a cell: built in and, for Hermes, installed.
    backends: Vec<&'static str>,
    /// Whether a custom app icon is set; absent where HQ cannot change its icon (macOS).
    app_icon: Option<bool>,
}

struct Profile(Option<String>);

/// Async because finding Hermes can wait on the macOS login-shell PATH (see `macos::path`).
#[tauri::command]
async fn build_info(app: AppHandle, profile: State<'_, Profile>) -> Result<BuildInfo, String> {
    let mut backends = vec!["claude"];
    #[cfg(feature = "codex")]
    backends.push("codex");
    // Hermes is offered only where it is installed, so no one without it sees it in the UI.
    #[cfg(feature = "hermes")]
    if tauri::async_runtime::spawn_blocking(hermes::installed).await.unwrap_or(false) {
        backends.push("hermes");
    }
    Ok(BuildInfo {
        profile: profile.0.clone(),
        version: app.package_info().version.to_string(),
        live_ui: cfg!(debug_assertions),
        updatable: update::updates_enabled(),
        backends,
        #[cfg(not(target_os = "macos"))]
        app_icon: Some(icon::is_set(&app)),
        #[cfg(target_os = "macos")]
        app_icon: None,
    })
}

/// Which copy of HQ this is. The installed app is the default profile. Any other copy (a build
/// under `target\`) is "dev", or whatever `HQ_PROFILE` names. A profile gets its own identifier,
/// and with it its own board.json, WebView2 data folder and single-instance lock, so it runs
/// next to the installed app without touching it. `HQ_PROFILE=default` opts back in.
fn profile() -> Option<String> {
    match std::env::var("HQ_PROFILE") {
        Ok(p) if p.eq_ignore_ascii_case("default") => None,
        Ok(p) if !p.is_empty() && p.len() <= 32 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => {
            Some(p.to_ascii_lowercase())
        }
        _ if update::is_installed() => None,
        _ => Some("dev".into()),
    }
}

/// Tauri's default Mac menu, less what would swallow HQ's own keys: its Close Window is Cmd+W
/// (which would end the app and every session, where HQ means "close this cell") and its
/// Minimize is Cmd+M (HQ's "maximize this cell"). A menu shortcut fires before the page sees the
/// key, so those two are left out or given no shortcut. Edit stays, since Cmd+C/V/X/A/Z need it.
#[cfg(target_os = "macos")]
fn mac_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
    let name = app.package_info().name.clone();
    Menu::with_items(
        app,
        &[
            &Submenu::with_items(
                app,
                name,
                true,
                &[
                    &PredefinedMenuItem::about(app, None, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::show_all(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            &Submenu::with_items(app, "View", true, &[&PredefinedMenuItem::fullscreen(app, None)?])?,
            &Submenu::with_items(
                app,
                "Window",
                true,
                &[&MenuItem::with_id(app, "minimize", "Minimize", true, None::<&str>)?, &PredefinedMenuItem::maximize(app, Some("Zoom"))?],
            )?,
        ],
    )
}

fn main() {
    // A session's agent runs `hq --mcp-stdio` to reach HQ's MCP server; it is not a second HQ.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--mcp-stdio") {
        let profile = args.iter().position(|a| a == "--profile").and_then(|i| args.get(i + 1)).cloned();
        std::process::exit(mcp::run_stdio_bridge(profile));
    }
    let profile = profile();
    let mut context = tauri::generate_context!();
    if let Some(name) = &profile {
        let config = context.config_mut();
        config.identifier = format!("{}.{name}", config.identifier);
        for window in &mut config.app.windows {
            window.title = format!("{} ({name})", window.title);
        }
    }
    #[cfg(debug_assertions)]
    dev::prepare(&mut context);
    // Works out the login shell's PATH while the window opens; the first child waits for it.
    #[cfg(target_os = "macos")]
    macos::warm_path();

    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.menu(mac_menu).on_menu_event(|app, event| {
        if event.id().0 == "minimize" {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.minimize();
            }
        }
    });
    builder
        // Must come first: a second launch of the same profile hands over to the running copy.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(request) = parse_launch(&args) {
                if let Some(launches) = app.try_state::<Launches>() {
                    if let Ok(mut q) = launches.0.lock() {
                        q.push(request);
                    }
                    let _ = app.emit("launch", ());
                }
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Sessions::default())
        .manage(store::Index::default())
        .manage(usage::UsageIndex::default())
        .manage(update::PendingUpdate::default())
        .manage(Profile(profile))
        // A first start can ask for a session too; the UI takes it once the board is back.
        .manage(Launches(std::sync::Mutex::new(parse_launch(&std::env::args().collect::<Vec<_>>()).into_iter().collect())))
        .invoke_handler(tauri::generate_handler![
            spawn_session,
            send_line,
            kill_session,
            mcp_reply,
            create_worktree,
            install_mcp_everywhere,
            pick_folder,
            save_text_file,
            set_claude_path,
            api_key_hint,
            set_api_key,
            pick_workspace,
            find_workspaces,
            folder_exists,
            load_state,
            save_state,
            load_transcript,
            #[cfg(feature = "codex")]
            load_codex_transcript,
            #[cfg(feature = "codex")]
            list_codex_sessions,
            #[cfg(feature = "codex")]
            codex_models,
            #[cfg(feature = "codex")]
            update_codex,
            #[cfg(feature = "codex")]
            set_codex_path,
            #[cfg(feature = "hermes")]
            load_hermes_transcript,
            #[cfg(feature = "hermes")]
            list_hermes_sessions,
            list_sessions,
            search_sessions,
            usage_report,
            probe_usage,
            update_claude,
            claude_status,
            install_claude,
            login_claude,
            finish_login,
            open_url,
            reveal_folder,
            open_in_vscode,
            open_terminal,
            run_in_terminal,
            request_attention,
            check_update,
            install_update,
            kill_all_sessions,
            build_info,
            take_launches,
            #[cfg(not(target_os = "macos"))]
            icon::pick_app_icon,
            #[cfg(not(target_os = "macos"))]
            icon::reset_app_icon,
        ])
        .setup(|app| {
            #[cfg(not(target_os = "macos"))]
            icon::apply(app.handle());
            mcp::start(app.handle(), app.state::<Profile>().0.as_deref());
            Ok(())
        })
        .build(context)
        .expect("failed to start HQ")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                app.state::<Sessions>().kill_all();
                mcp::cleanup();
                setup::finish_login();
                #[cfg(feature = "codex")]
                codex::stop_helper();
                // The frontend's last save may still be waiting for the writer thread.
                store::flush_state();
                // An update that never found an idle moment installs as HQ closes.
                if update::is_ready(app) {
                    let _ = update::install(app, false);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_start_asks_for_nothing() {
        assert_eq!(parse_launch(&args(&["hq.exe"])), None);
        assert_eq!(parse_launch(&args(&["hq.exe", "--new-session"])), None);
        assert_eq!(parse_launch(&args(&["hq.exe", "--new-session", ""])), None);
        assert_eq!(parse_launch(&args(&["hq.exe", "--new-session", "--model", "sonnet"])), None);
    }

    #[test]
    fn new_session_reads_its_options() {
        let got = parse_launch(&args(&[
            "hq.exe", "--new-session", "D:/Repos/x", "--model", "sonnet", "--prompt", "Read a; b. Start now.",
        ]));
        assert_eq!(
            got,
            Some(LaunchRequest {
                cwd: "D:/Repos/x".into(),
                model: Some("sonnet".into()),
                effort: None,
                permission_mode: None,
                prompt: Some("Read a; b. Start now.".into()),
            })
        );
    }
}

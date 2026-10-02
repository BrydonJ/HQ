#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(debug_assertions)]
mod dev;
mod session;
mod store;
mod update;

use std::path::Path;

use session::{Sessions, SpawnOptions};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager, State, UserAttentionType, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

/// Every message on `on_event` is a JSON array of the session's stdout lines (as objects), and
/// the last element of the last array is the `hq_exit` object.
#[tauri::command]
async fn spawn_session(
    app: AppHandle,
    cwd: String,
    resume: Option<String>,
    permission_mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    fork: Option<bool>,
    on_event: Channel<InvokeResponseBody>,
) -> Result<u64, String> {
    let fork = fork.unwrap_or(false);
    let opts = SpawnOptions { cwd, resume, permission_mode, model, effort, fork };
    tauri::async_runtime::spawn_blocking(move || app.state::<Sessions>().spawn(&app, opts, on_event))
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

/// Asks where to save `text` (as UTF-8, a Markdown file by default) and writes it there.
/// Returns the chosen path, or `None` when the user cancels.
#[tauri::command]
async fn save_text_file(window: WebviewWindow, default_name: String, text: String) -> Result<Option<String>, String> {
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let chosen = app
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Save as")
            .set_file_name(&default_name)
            .add_filter("Markdown", &["md"])
            .blocking_save_file();
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

#[tauri::command]
async fn list_sessions(app: AppHandle, limit: Option<usize>, cwd: Option<String>) -> Result<Vec<store::SessionInfo>, String> {
    let limit = limit.unwrap_or(80).min(500);
    tauri::async_runtime::spawn_blocking(move || store::list_sessions(&app.state::<store::Index>(), limit, cwd.as_deref()))
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

#[tauri::command]
async fn probe_usage() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(session::probe_usage).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn update_claude() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(session::update_claude).await.map_err(|e| e.to_string())?
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

/// Opens a folder, or a `.code-workspace` file, in VS Code. Nothing else is accepted.
#[tauri::command]
fn open_in_vscode(path: String) -> Result<(), String> {
    let target = Path::new(&path);
    if store::is_workspace_file(target) {
        if !target.is_file() {
            return Err("workspace file does not exist".into());
        }
    } else if !target.is_dir() {
        return Err("folder does not exist".into());
    }
    let code = store::find_vscode().ok_or("VS Code (Code.exe) was not found")?;
    std::process::Command::new(code).arg(&path).spawn().map(|_| ()).map_err(|e| e.to_string())
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

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BuildInfo {
    profile: Option<String>,
    version: String,
    /// The UI is read from `ui/` on each load, so a reload shows edits without a rebuild.
    live_ui: bool,
    /// Whether this copy installs updates (the installed app, or a test build that opts in).
    updatable: bool,
}

struct Profile(Option<String>);

#[tauri::command]
fn build_info(app: AppHandle, profile: State<'_, Profile>) -> BuildInfo {
    BuildInfo {
        profile: profile.0.clone(),
        version: app.package_info().version.to_string(),
        live_ui: cfg!(debug_assertions),
        updatable: update::updates_enabled(),
    }
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

fn main() {
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

    tauri::Builder::default()
        // Must come first: a second launch of the same profile hands over to the running copy.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
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
        .manage(update::PendingUpdate::default())
        .manage(Profile(profile))
        .invoke_handler(tauri::generate_handler![
            spawn_session,
            send_line,
            kill_session,
            pick_folder,
            save_text_file,
            set_claude_path,
            pick_workspace,
            find_workspaces,
            folder_exists,
            load_state,
            save_state,
            load_transcript,
            list_sessions,
            search_sessions,
            probe_usage,
            update_claude,
            open_url,
            reveal_folder,
            open_in_vscode,
            request_attention,
            check_update,
            install_update,
            kill_all_sessions,
            build_info,
        ])
        .build(context)
        .expect("failed to start HQ")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                app.state::<Sessions>().kill_all();
                // The frontend's last save may still be waiting for the writer thread.
                store::flush_state();
                // An update that never found an idle moment installs as HQ closes.
                if update::is_ready(app) {
                    let _ = update::install(app, false);
                }
            }
        });
}

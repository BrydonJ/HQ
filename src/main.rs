#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;
mod store;

use std::path::Path;

use session::{Sessions, SpawnOptions};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State, UserAttentionType, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
fn spawn_session(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    cwd: String,
    resume: Option<String>,
    permission_mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    fork: Option<bool>,
    on_event: Channel<String>,
) -> Result<u64, String> {
    let fork = fork.unwrap_or(false);
    sessions.spawn(&app, SpawnOptions { cwd, resume, permission_mode, model, effort, fork }, on_event)
}

#[tauri::command]
fn send_line(sessions: State<'_, Sessions>, id: u64, line: String) -> Result<(), String> {
    sessions.send(id, &line)
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

#[tauri::command]
fn folder_exists(path: String) -> bool {
    Path::new(&path).is_dir()
}

#[tauri::command]
fn load_state(app: AppHandle) -> Result<Option<String>, String> {
    store::load_state(&app)
}

#[tauri::command]
fn save_state(app: AppHandle, json: String) -> Result<(), String> {
    store::save_state(&app, &json)
}

#[tauri::command]
async fn load_transcript(session_id: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || store::load_transcript(&session_id))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn list_sessions(limit: Option<usize>) -> Result<Vec<store::SessionInfo>, String> {
    let limit = limit.unwrap_or(80).min(500);
    tauri::async_runtime::spawn_blocking(move || store::list_sessions(limit))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn probe_usage() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(session::probe_usage).await.map_err(|e| e.to_string())?
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

#[tauri::command]
fn open_in_vscode(path: String) -> Result<(), String> {
    if !Path::new(&path).is_dir() {
        return Err("folder does not exist".into());
    }
    let code = store::find_vscode().ok_or("VS Code (Code.exe) was not found")?;
    std::process::Command::new(code).arg(&path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn request_attention(window: WebviewWindow) {
    if !window.is_focused().unwrap_or(false) {
        let _ = window.request_user_attention(Some(UserAttentionType::Informational));
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Sessions::default())
        .invoke_handler(tauri::generate_handler![
            spawn_session,
            send_line,
            kill_session,
            pick_folder,
            folder_exists,
            load_state,
            save_state,
            load_transcript,
            list_sessions,
            probe_usage,
            open_url,
            reveal_folder,
            open_in_vscode,
            request_attention,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start HQ")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                app.state::<Sessions>().kill_all();
            }
        });
}

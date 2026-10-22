//! A custom app icon: a PNG or ICO the user picks, shown on the window and taskbar instead of HQ's
//! own. A copy is kept next to board.json, so the original can move or go. macOS has no window
//! icon (its Dock icon comes from the bundle), so this is Windows and Linux only.

use std::fs;
use std::path::PathBuf;

use tauri::image::Image;
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

/// Larger files are refused: an icon needs nothing near this, and the whole file is decoded.
const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// It is decoded again at every start, so its pixels are capped too.
const MAX_SIDE: u32 = 1024;

fn icon_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(crate::store::state_path(app)?.with_file_name("app-icon"))
}

/// Whether a custom icon is set.
pub fn is_set(app: &AppHandle) -> bool {
    icon_path(app).is_ok_and(|p| p.is_file())
}

/// Puts the custom icon on the main window, or HQ's own when none is set or it cannot be read.
pub fn apply(app: &AppHandle) {
    let custom = icon_path(app).ok().and_then(|p| fs::read(p).ok()).and_then(|b| Image::from_bytes(&b).ok().map(Image::to_owned));
    let Some(icon) = custom.or_else(|| app.default_window_icon().cloned()) else { return };
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_icon(icon);
    }
}

/// Asks for a PNG or ICO and makes it the app icon. `false` when the dialog is cancelled.
#[tauri::command]
pub async fn pick_app_icon(window: WebviewWindow) -> Result<bool, String> {
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let picked = app
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Choose an app icon")
            .add_filter("Icon", &["png", "ico"])
            .blocking_pick_file()
            .and_then(|p| p.into_path().ok());
        let Some(path) = picked else { return Ok(false) };
        if fs::metadata(&path).map_err(|e| e.to_string())?.len() > MAX_BYTES {
            return Err("That file is too large for an icon (8 MB at most).".into());
        }
        let bytes = fs::read(&path).map_err(|e| e.to_string())?;
        let image = Image::from_bytes(&bytes).map_err(|e| format!("Not a PNG or ICO image HQ can read: {e}"))?;
        if image.width() > MAX_SIDE || image.height() > MAX_SIDE {
            return Err(format!("That image is too large for an icon ({MAX_SIDE}x{MAX_SIDE} pixels at most)."));
        }
        let dest = icon_path(&app)?;
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
        apply(&app);
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Goes back to HQ's own icon.
#[tauri::command]
pub fn reset_app_icon(app: AppHandle) -> Result<(), String> {
    match fs::remove_file(icon_path(&app)?) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    apply(&app);
    Ok(())
}

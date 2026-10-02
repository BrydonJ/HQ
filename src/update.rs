//! Self-update from signed releases (tauri-plugin-updater).
//!
//! The frontend decides when to install: installing ends every running `claude`, so it waits
//! until no session is busy. The download happens up front, so the install itself is quick.
//! Only an installed copy updates itself; a dev build or a copied exe is left alone.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::session::Sessions;

/// A verified download waiting to be installed.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<(Update, Vec<u8>)>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
}

impl UpdateInfo {
    fn of(update: &Update) -> Self {
        UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        }
    }
}

/// The NSIS installer puts `uninstall.exe` next to the app; a build under `target\` has none.
pub fn is_installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("uninstall.exe").is_file()))
        .unwrap_or(false)
}

/// `HQ_NO_UPDATE` turns updates off; `HQ_UPDATE_ANY_COPY` allows them for a test build.
pub fn updates_enabled() -> bool {
    if std::env::var_os("HQ_NO_UPDATE").is_some() {
        return false;
    }
    is_installed() || std::env::var_os("HQ_UPDATE_ANY_COPY").is_some()
}

/// Checks for a newer release and downloads it. Returns the version that is ready to install.
pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    if !updates_enabled() {
        return Ok(None);
    }
    let Some(update) = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let pending = app.state::<PendingUpdate>();
    if let Some((ready, _)) = pending.0.lock().unwrap().as_ref() {
        if ready.version == update.version {
            return Ok(Some(UpdateInfo::of(ready)));
        }
    }
    // The signature is checked against the public key in tauri.conf.json during the download.
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
    let info = UpdateInfo::of(&update);
    *pending.0.lock().unwrap() = Some((update, bytes));
    Ok(Some(info))
}

pub fn is_ready(app: &AppHandle) -> bool {
    app.state::<PendingUpdate>().0.lock().unwrap().is_some()
}

/// Runs the downloaded installer. On Windows this exits HQ; with `restart` the installer
/// starts the new version when it finishes.
pub fn install(app: &AppHandle, restart: bool) -> Result<(), String> {
    let Some((update, bytes)) = app.state::<PendingUpdate>().0.lock().unwrap().take() else {
        return Err("No update has been downloaded.".into());
    };
    app.state::<Sessions>().kill_all();
    // Installing exits HQ without the usual exit event, so queued saves are flushed here.
    crate::store::flush_state();
    update.restart_after_install(restart).install(bytes).map_err(|e| e.to_string())?;
    // Only reached on platforms where installing does not exit the app.
    if restart {
        app.restart();
    }
    Ok(())
}

//! Board persistence and read-only access to Claude Code's own session transcripts.

use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use tauri::{AppHandle, Manager};

/// Transcript lines older than this are dropped when restoring a cell, so a very long
/// session does not stall startup. The full history still lives in Claude's own file.
const MAX_TRANSCRIPT_LINES: usize = 1500;

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("board.json"))
}

pub fn load_state(app: &AppHandle) -> Result<Option<String>, String> {
    let path = state_path(app)?;
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn save_state(app: &AppHandle, json: &str) -> Result<(), String> {
    serde_json::from_str::<Value>(json).map_err(|e| format!("refusing to save invalid JSON: {e}"))?;
    let path = state_path(app)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claude"))
}

/// Returns the user/assistant lines of a session's transcript so a restored cell can show
/// its history. Finds the file by name under every project folder instead of re-deriving
/// Claude's folder-name encoding, which is an internal detail that may change.
pub fn load_transcript(session_id: &str) -> Result<Vec<String>, String> {
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err("invalid session id".into());
    }
    let projects = claude_config_dir().ok_or("cannot locate Claude config folder")?.join("projects");
    let file_name = format!("{session_id}.jsonl");
    let path = fs::read_dir(&projects)
        .map_err(|e| format!("{}: {e}", projects.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join(&file_name))
        .find(|p| p.is_file());
    let Some(path) = path else { return Ok(Vec::new()) };

    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| {
            let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
            let kind = v.get("type").and_then(Value::as_str);
            matches!(kind, Some("user" | "assistant"))
                && v.get("isSidechain").and_then(Value::as_bool) != Some(true)
        })
        .map(str::to_owned)
        .collect();
    if lines.len() > MAX_TRANSCRIPT_LINES {
        lines.drain(..lines.len() - MAX_TRANSCRIPT_LINES);
    }
    Ok(lines)
}

/// Finds VS Code's executable directly so a folder path is passed as a plain argument,
/// never through `cmd /c code ...` where `&` or `^` in a path would be interpreted.
pub fn find_vscode() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Programs/Microsoft VS Code/Code.exe"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            candidates.push(PathBuf::from(pf).join("Microsoft VS Code/Code.exe"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

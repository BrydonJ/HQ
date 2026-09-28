//! Board persistence and read-only access to Claude Code's own session transcripts.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

/// Transcript lines older than this are dropped when restoring a cell, so a very long
/// session does not stall startup. The full history still lives in Claude's own file.
const MAX_TRANSCRIPT_LINES: usize = 1500;

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    // HQ_STATE_DIR lets a second copy (for example a test build) keep its own board.
    let dir = match std::env::var_os("HQ_STATE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => app.path().app_config_dir().map_err(|e| e.to_string())?,
    };
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

/// A session from Claude's own transcript folder, for the "Resume a session" list.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    session_id: String,
    cwd: String,
    cwd_exists: bool,
    title: Option<String>,
    last_prompt: Option<String>,
    /// Where the session was last used: `claude-vscode`, `cli`, `sdk-cli` (HQ), ...
    entrypoint: Option<String>,
    modified_ms: u64,
}

/// How much of each end of a transcript is read. Titles and the last prompt are appended as a
/// session goes, so the newest copies sit near the end; the start is only read as a fallback.
const SCAN_BYTES: u64 = 256 * 1024;

/// Lists the most recently used sessions across every project folder, newest first.
///
/// The transcript format is internal to Claude Code, so every field is optional and a
/// transcript that cannot be read is skipped rather than failing the whole list.
pub fn list_sessions(limit: usize) -> Result<Vec<SessionInfo>, String> {
    let projects = claude_config_dir().ok_or("cannot locate Claude config folder")?.join("projects");
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    for dir in fs::read_dir(&projects).map_err(|e| format!("{}: {e}", projects.display()))?.flatten() {
        let Ok(entries) = fs::read_dir(dir.path()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_session = path.extension().is_some_and(|e| e == "jsonl")
                && path.file_stem().and_then(|s| s.to_str()).is_some_and(is_session_id);
            if !is_session {
                continue;
            }
            if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                files.push((path, modified));
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(files.into_iter().filter_map(|(path, modified)| read_session_info(&path, modified)).take(limit).collect())
}

fn is_session_id(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

#[derive(Default)]
struct Scan {
    cwd: Option<String>,
    entrypoint: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    first_prompt: Option<String>,
}

fn read_session_info(path: &Path, modified: SystemTime) -> Option<SessionInfo> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut scan = Scan::default();

    let start = len.saturating_sub(SCAN_BYTES);
    let tail = read_range(&mut file, start, len)?;
    let mut lines: Vec<&str> = tail.split('\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // partial line
    }
    for line in lines.iter().rev() {
        scan_line(line, &mut scan, true);
        let titled = scan.custom_title.is_some() || scan.ai_title.is_some();
        if scan.cwd.is_some() && titled && scan.last_prompt.is_some() {
            break; // everything shown is found; older lines only hold older copies
        }
    }
    if scan.cwd.is_none() || (scan.custom_title.is_none() && scan.ai_title.is_none() && scan.last_prompt.is_none()) {
        let head = read_range(&mut file, 0, SCAN_BYTES.min(len))?;
        let mut lines: Vec<&str> = head.split('\n').collect();
        if len > SCAN_BYTES {
            lines.pop(); // partial line
        }
        for line in lines {
            scan_line(line, &mut scan, false);
        }
    }

    let cwd = scan.cwd?; // a session without a folder cannot be resumed
    let modified_ms = modified.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    Some(SessionInfo {
        session_id: path.file_stem()?.to_str()?.to_owned(),
        cwd_exists: Path::new(&cwd).is_dir(),
        cwd,
        title: scan.custom_title.or(scan.ai_title),
        last_prompt: scan.last_prompt.or(scan.first_prompt),
        entrypoint: scan.entrypoint,
        modified_ms,
    })
}

fn read_range(file: &mut fs::File, start: u64, end: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((end - start) as usize);
    file.take(end - start).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Picks the metadata out of one transcript line. `newest_first` is true while walking the
/// tail backwards, so the first value found there is the latest one.
fn scan_line(line: &str, scan: &mut Scan, newest_first: bool) {
    let set = |slot: &mut Option<String>, value: Option<&str>| {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            if slot.is_none() || !newest_first {
                *slot = Some(v.to_owned());
            }
        }
    };
    // Cheap substring checks first; most lines are large messages that need no parsing.
    let wanted = line.contains("\"custom-title\"")
        || line.contains("\"ai-title\"")
        || line.contains("\"last-prompt\"")
        || (scan.cwd.is_none() && line.contains("\"cwd\""))
        || (scan.first_prompt.is_none() && !newest_first && line.contains("\"type\":\"user\""));
    if !wanted {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else { return };
    let text = |key: &str| v.get(key).and_then(Value::as_str);
    match text("type") {
        Some("custom-title") => set(&mut scan.custom_title, text("customTitle")),
        Some("ai-title") => set(&mut scan.ai_title, text("aiTitle")),
        Some("last-prompt") => set(&mut scan.last_prompt, text("lastPrompt")),
        _ => {}
    }
    if scan.cwd.is_none() {
        if let Some(cwd) = text("cwd") {
            scan.cwd = Some(cwd.to_owned());
            scan.entrypoint = text("entrypoint").map(str::to_owned);
        }
    }
    if !newest_first && scan.first_prompt.is_none() && text("type") == Some("user") && v.get("isMeta").is_none() {
        let prompt = v.pointer("/message/content").and_then(Value::as_str).filter(|t| !t.trim_start().starts_with('<'));
        set(&mut scan.first_prompt, prompt.map(|t| t.lines().next().unwrap_or(t)));
    }
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

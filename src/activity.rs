//! A small per-day log of when each session started, worked, went idle and closed, for
//! timesheets (`/log-hours` reads it). One JSON line per state change, one file per local day:
//! `<config dir>/activity/YYYY-MM-DD.jsonl`. Files older than `KEEP_DAYS` are deleted at startup.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tauri::AppHandle;

const KEEP_DAYS: u64 = 90;
/// A line is a handful of short fields; anything longer is not one of ours.
const MAX_LINE: usize = 4096;

fn dir(app: &AppHandle) -> Result<PathBuf, String> {
    let state = crate::store::state_path(app)?;
    let dir = state.parent().ok_or("no config dir")?.join("activity");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// `YYYY-MM-DD`: the frontend names the day, since it knows the local date.
fn valid_day(day: &str) -> bool {
    let b = day.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn valid_line(line: &str) -> bool {
    line.len() <= MAX_LINE && !line.contains(['\n', '\r']) && serde_json::from_str::<serde_json::Value>(line).map_or(false, |v| v.is_object())
}

/// Appends one line to the day's file. Synchronous so lines land in the order they were sent.
pub fn append(app: &AppHandle, day: &str, line: &str) -> Result<(), String> {
    if !valid_day(day) || !valid_line(line) {
        return Err("bad activity line".into());
    }
    let path = dir(app)?.join(format!("{day}.jsonl"));
    let mut file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| e.to_string())?;
    file.write_all(format!("{line}\n").as_bytes()).map_err(|e| e.to_string())
}

/// Deletes day files last written more than `KEEP_DAYS` ago.
pub fn prune(app: &AppHandle) {
    let Ok(dir) = dir(app) else { return };
    let Ok(entries) = fs::read_dir(&dir) else { return };
    let cutoff = SystemTime::now() - Duration::from_secs(KEEP_DAYS * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".jsonl")) else { continue };
        if !valid_day(stem) {
            continue;
        }
        if entry.metadata().and_then(|m| m.modified()).map_or(false, |t| t < cutoff) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_lines() {
        assert!(valid_day("2026-11-12"));
        assert!(!valid_day("2026-11-1"));
        assert!(!valid_day("../../x.jsonl"));
        assert!(!valid_day("2026a11-12"));
        assert!(valid_line(r#"{"state":"running"}"#));
        assert!(!valid_line("[1]"));
        assert!(!valid_line("{\"a\":1}\n{\"b\":2}"));
        assert!(!valid_line("not json"));
    }
}

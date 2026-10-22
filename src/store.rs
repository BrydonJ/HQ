//! Board persistence and read-only access to Claude Code's own session transcripts.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use memchr::memmem;
use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

/// A restored cell loads at most this many transcript lines (or `MAX_TRANSCRIPT_BYTES` of them,
/// whichever comes first), newest first, so a very long session does not stall startup. The
/// full history still lives in Claude's own file, and "Load earlier" reads on from there.
const MAX_TRANSCRIPT_LINES: usize = 1500;
const MAX_TRANSCRIPT_BYTES: usize = 4 * 1024 * 1024;
const READ_CHUNK: u64 = 256 * 1024;
/// How far back past the loaded lines to look for the session's title lines.
const TITLE_SCAN_BYTES: u64 = 32 * 1024 * 1024;
/// A base64 run shorter than this is left alone: nothing big enough to be worth blanking.
const MIN_BASE64_RUN: usize = 256;

pub(crate) fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
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

enum SaveJob {
    Write(String),
    /// Answered once every write queued before it is on disk.
    Flush(mpsc::Sender<()>),
}

static SAVE_QUEUE: OnceLock<mpsc::Sender<SaveJob>> = OnceLock::new();
/// The last failed write, reported by the next `save_state` call since the writer is not awaited.
static SAVE_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Queues the board for writing and returns at once. The frontend sends a full snapshot each
/// time, so a writer thread that skips to the newest queued one loses nothing, and one thread
/// writing in queue order means a slow, older save can never land after a newer one (parallel
/// tasks would race on both the temp file and the order). The frontend always sends
/// `JSON.stringify` output, so the text is not parsed again here.
pub fn save_state(app: &AppHandle, json: String) -> Result<(), String> {
    let queue = SAVE_QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<SaveJob>();
        let app = app.clone();
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let (mut json, mut flush) = match job {
                    SaveJob::Write(json) => (json, None),
                    SaveJob::Flush(done) => {
                        let _ = done.send(());
                        continue;
                    }
                };
                // Only the newest snapshot matters. A flush request that arrives meanwhile is
                // answered after that write.
                while let Ok(next) = rx.try_recv() {
                    match next {
                        SaveJob::Write(newer) => json = newer,
                        SaveJob::Flush(done) => flush = Some(done),
                    }
                }
                let written = write_state(&app, &json);
                if let Err(e) = &written {
                    eprintln!("could not save the board: {e}");
                }
                *SAVE_ERROR.lock().unwrap() = written.err();
                if let Some(done) = flush {
                    let _ = done.send(());
                }
            }
        });
        tx
    });
    queue.send(SaveJob::Write(json)).map_err(|_| "board writer stopped".to_string())?;
    match SAVE_ERROR.lock().unwrap().take() {
        Some(e) => Err(format!("the previous save failed: {e}")),
        None => Ok(()),
    }
}

/// Waits (briefly) for queued saves to reach the disk. Called as HQ exits, when the frontend's
/// last save may still be in the queue.
pub fn flush_state() {
    let Some(queue) = SAVE_QUEUE.get() else { return };
    let (tx, rx) = mpsc::channel();
    if queue.send(SaveJob::Flush(tx)).is_ok() {
        let _ = rx.recv_timeout(Duration::from_secs(3));
    }
}

fn write_state(app: &AppHandle, json: &str) -> Result<(), String> {
    let path = state_path(app)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// The Claude Console API key from HQ's settings, beside board.json but never in it: the board
/// is a snapshot the webview sends on every save. Outer None: not read from disk yet.
static API_KEY: Mutex<Option<Option<String>>> = Mutex::new(None);

fn api_key_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(state_path(app)?.with_file_name("api-key"))
}

/// The saved API key, read from disk the first time it is asked for.
pub fn api_key(app: &AppHandle) -> Option<String> {
    let mut key = API_KEY.lock().unwrap();
    key.get_or_insert_with(|| {
        let text = fs::read_to_string(api_key_path(app).ok()?).ok()?;
        clean_api_key(&text).ok()
    })
    .clone()
}

/// Saves the key, or with None deletes it. Unix: readable by the user only. Windows: the folder
/// is in the user's profile, which only that user can read (as with Claude's own credentials).
pub fn set_api_key(app: &AppHandle, key: Option<String>) -> Result<(), String> {
    let path = api_key_path(app)?;
    match &key {
        Some(k) => {
            let tmp = path.with_extension("tmp");
            let _ = fs::remove_file(&tmp); // a leftover keeps its old mode; a new file gets 0600
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let written = options.open(&tmp).and_then(|mut f| io::Write::write_all(&mut f, k.as_bytes()));
            if let Err(e) = written.and_then(|_| fs::rename(&tmp, &path)) {
                let _ = fs::remove_file(&tmp);
                return Err(format!("could not save the API key: {e}"));
            }
        }
        None => match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(format!("could not remove the API key: {e}")),
            _ => {}
        },
    }
    *API_KEY.lock().unwrap() = Some(key);
    Ok(())
}

/// Checks a pasted key. Only a Console API key draws on API credits: a subscription token from
/// `claude setup-token` bills the plan, and as an API key it fails.
pub fn clean_api_key(raw: &str) -> Result<String, String> {
    let key = raw.trim().trim_matches('"').trim();
    if key.starts_with("sk-ant-oat") {
        return Err("That is a Claude subscription token (from `claude setup-token`), not an API key. Create one at platform.claude.com.".into());
    }
    if !key.starts_with("sk-ant-api") {
        return Err("That is not a Claude API key: Console keys start with sk-ant-api.".into());
    }
    if key.len() > 300 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("The key has characters an API key never has. Copy it again.".into());
    }
    Ok(key.to_owned())
}

/// Enough of the key to tell which one is saved, never enough to use it.
pub fn api_key_hint(key: &str) -> String {
    format!("sk-ant-…{}", &key[key.len().saturating_sub(4)..])
}

fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claude"))
}

/// One slice of a session's transcript, for replaying it in a cell.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptChunk {
    /// The transcript's own lines, oldest first, for the frontend to parse.
    lines: Vec<String>,
    /// Where the first message line sits in the file. Pass it as `before` to load the lines
    /// that come earlier; 0 means there are none.
    start: u64,
}

impl TranscriptChunk {
    /// For a backend that builds the lines itself; `start` is whatever its `before` reads back.
    #[allow(dead_code)]
    pub(crate) fn new(lines: Vec<String>, start: u64) -> Self {
        TranscriptChunk { lines, start }
    }
}

/// Returns the user/assistant and title lines of a session's transcript so a restored cell can
/// show its history and name. Finds the file by name under every project folder instead of
/// re-deriving Claude's folder-name encoding, which is an internal detail that may change.
///
/// Reads from the end of the file, so the cost follows what is returned, not the file's size.
/// With `before` it loads the lines ahead of that offset instead, without the titles.
pub fn load_transcript(session_id: &str, before: Option<u64>) -> Result<TranscriptChunk, String> {
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
    let Some(path) = path else { return Ok(TranscriptChunk { lines: Vec::new(), start: 0 }) };
    read_transcript(&path, before).map_err(|e| e.to_string())
}

#[derive(PartialEq, Clone, Copy)]
enum LineKind {
    Message,
    CustomTitle,
    AiTitle,
}

/// The two fields that decide whether a transcript line is wanted.
#[derive(Deserialize)]
struct LineHead {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
}

const CUSTOM_TITLE: &[u8] = br#""type":"custom-title""#;
const AI_TITLE: &[u8] = br#""type":"ai-title""#;

/// Classifies one transcript line, or returns `None` for the many it does not want: the
/// main thread's user and assistant messages, and the lines that name the session.
fn line_kind(raw: &[u8]) -> Option<LineKind> {
    // Cheap substring checks first; most of the bytes are in message lines that must be parsed
    // anyway, but the rest (snapshots, progress, queue entries) need no parsing at all.
    const WANTED: [&[u8]; 4] = [br#""type":"user""#, br#""type":"assistant""#, br#""custom-title""#, br#""ai-title""#];
    if !WANTED.iter().any(|w| memmem::find(raw, w).is_some()) {
        return None;
    }
    let head: LineHead = serde_json::from_slice(raw).ok()?;
    match head.kind.as_deref()? {
        "user" | "assistant" if head.is_sidechain != Some(true) => Some(LineKind::Message),
        // The session's name: a rename made anywhere, or the title Claude gave it.
        "custom-title" => Some(LineKind::CustomTitle),
        "ai-title" => Some(LineKind::AiTitle),
        _ => None,
    }
}

pub(crate) fn read_transcript(path: &Path, before: Option<u64>) -> io::Result<TranscriptChunk> {
    let file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let end = before.map_or(len, |b| b.min(len));
    let mut walk = BackwardLines::new(file, end);

    let mut kept: Vec<String> = Vec::new(); // newest first
    let (mut bytes, mut start, mut capped) = (0, 0, false);
    let (mut has_custom, mut has_ai) = (false, false);
    while let Some((offset, raw)) = walk.next_line()? {
        let Some(kind) = line_kind(raw) else { continue };
        // Loading earlier lines must not hand back a title that an already loaded line supersedes.
        if before.is_some() && kind != LineKind::Message {
            continue;
        }
        // Image bytes are blanked before counting, since the budget is what crosses IPC.
        let line = blank_base64(String::from_utf8_lossy(raw).trim_end()).into_owned();
        bytes += line.len();
        has_custom |= kind == LineKind::CustomTitle;
        has_ai |= kind == LineKind::AiTitle;
        kept.push(line);
        start = offset;
        if kept.len() >= MAX_TRANSCRIPT_LINES || bytes >= MAX_TRANSCRIPT_BYTES {
            capped = true;
            break;
        }
    }
    // Reaching the top of the region without a cap leaves nothing earlier to load, even when the
    // oldest message sits further down.
    if !capped {
        start = 0;
    }

    // The session's names are appended where they happen, so they can sit far above the lines
    // loaded. A name already among them supersedes any earlier one, so only the missing kind is
    // looked for, and only on the first load.
    let mut titles: Vec<String> = Vec::new(); // newest first
    if before.is_none() && capped {
        while !(has_custom && has_ai) {
            let Some((offset, raw)) = walk.next_line()? else { break };
            if start - offset > TITLE_SCAN_BYTES {
                break;
            }
            let looks_like = (!has_custom && memmem::find(raw, CUSTOM_TITLE).is_some())
                || (!has_ai && memmem::find(raw, AI_TITLE).is_some());
            let kind = if looks_like { line_kind(raw) } else { None };
            match kind {
                Some(LineKind::CustomTitle) if !has_custom => has_custom = true,
                Some(LineKind::AiTitle) if !has_ai => has_ai = true,
                _ => continue,
            }
            titles.push(String::from_utf8_lossy(raw).trim_end().to_owned());
        }
    }

    titles.reverse();
    kept.reverse();
    titles.extend(kept);
    Ok(TranscriptChunk { lines: titles, start })
}

/// Hands out the lines of a file's first `end` bytes from the last to the first, reading
/// `READ_CHUNK` bytes at a time. A line that straddles a chunk is stitched together before it
/// is handed out.
struct BackwardLines {
    file: fs::File,
    /// File offset of `buf[0]`.
    buf_start: u64,
    buf: Vec<u8>,
    /// Only `buf[..end]` is still to be handed out.
    end: usize,
    done: bool,
}

impl BackwardLines {
    fn new(file: fs::File, end: u64) -> Self {
        BackwardLines { file, buf_start: end, buf: Vec::new(), end: 0, done: end == 0 }
    }

    /// The next line up as (its offset in the file, its bytes without the line break). The
    /// empty line after a final line break comes out too, so callers skip empty lines.
    fn next_line(&mut self) -> io::Result<Option<(u64, &[u8])>> {
        loop {
            if self.done {
                return Ok(None);
            }
            if let Some(newline) = memchr::memrchr(b'\n', &self.buf[..self.end]) {
                let (from, to) = (newline + 1, self.end);
                self.end = newline;
                return Ok(Some((self.buf_start + from as u64, &self.buf[from..to])));
            }
            if self.buf_start == 0 {
                self.done = true;
                return Ok(Some((0, &self.buf[..self.end])));
            }
            let start = self.buf_start.saturating_sub(READ_CHUNK);
            let mut chunk = vec![0; (self.buf_start - start) as usize];
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut chunk)?;
            chunk.extend_from_slice(&self.buf[..self.end]);
            self.end = chunk.len();
            self.buf = chunk;
            self.buf_start = start;
        }
    }
}

/// Empties the value of every `"data":"<base64>"` and `"base64":"<base64>"` in a line (image
/// blocks, and the file bytes a Read of a PDF or picture leaves in `toolUseResult`) so they do
/// not cross IPC. The line stays valid JSON; an image block with an empty `data` is shown as
/// omitted.
fn blank_base64(line: &str) -> Cow<'_, str> {
    let first = blank_runs(line, br#""data":""#);
    let second = blank_runs(first.as_deref().unwrap_or(line), br#""base64":""#);
    match (first, second) {
        (_, Some(blanked)) | (Some(blanked), None) => Cow::Owned(blanked),
        (None, None) => Cow::Borrowed(line),
    }
}

/// `line` with the long base64 value after each `key` emptied, or `None` if there is none.
fn blank_runs(line: &str, key: &[u8]) -> Option<String> {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let (mut copied, mut from) = (0, 0);
    while let Some(i) = memmem::find(&bytes[from..], key) {
        let value = from + i + key.len();
        let run = bytes[value..].iter().take_while(|&&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')).count();
        from = value + run;
        if run > MIN_BASE64_RUN && bytes.get(from) == Some(&b'"') {
            // Both cut points sit next to ASCII bytes, so they are char boundaries.
            out.push_str(&line[copied..value]);
            copied = from;
        }
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&line[copied..]);
    Some(out)
}

/// A session from Claude's own transcript folder, for the "Resume a session" list.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    session_id: String,
    /// The folder the session started in. Its transcript is filed under this folder, so the
    /// session is resumed from here.
    cwd: String,
    /// The folder the session was last used in, when Claude moved away from where it started.
    last_cwd: Option<String>,
    /// Set on every call, not cached: folders appear and disappear.
    cwd_exists: bool,
    title: Option<String>,
    last_prompt: Option<String>,
    /// Where the session was last used: `claude-vscode`, `cli`, `sdk-cli` (HQ), ...
    entrypoint: Option<String>,
    /// When the session started: the first line's timestamp, as written (ISO 8601).
    started: Option<String>,
    /// Started headless by another agent or script (`claude -p "<prompt>"`), not by a person.
    agent: bool,
    modified_ms: u64,
}

/// How much of each end of a transcript is read. Titles and the last prompt are appended as a
/// session goes, so the newest copies sit near the end; the start is only read as a fallback.
const SCAN_BYTES: u64 = 256 * 1024;
/// How much of the start of a transcript is always read, for where and when the session began.
const HEAD_BYTES: u64 = 64 * 1024;

/// What HQ has learned from transcripts, kept in managed state: each file's session details
/// (reading them means scanning both ends of the file, and there are hundreds of files), and
/// the number of the search that is allowed to run.
#[derive(Default)]
pub struct Index {
    /// An entry holds only while the file's length and modified time are unchanged.
    sessions: Mutex<HashMap<PathBuf, CachedInfo>>,
    search: AtomicU64,
}

struct CachedInfo {
    len: u64,
    modified: SystemTime,
    /// `None` for a transcript that cannot be resumed, which is not worth reading again either.
    info: Option<SessionInfo>,
}

struct TranscriptFile {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

impl Index {
    /// The session in a transcript file, read only if the file changed since it was last read.
    fn info(&self, file: &TranscriptFile) -> Option<SessionInfo> {
        let cached = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(&file.path).filter(|c| c.len == file.len && c.modified == file.modified).map(|c| c.info.clone())
        };
        if let Some(info) = cached {
            return info;
        }
        // The lock is not held while reading, so a list and a search can run at once.
        let info = read_session_info(&file.path, file.modified);
        let entry = CachedInfo { len: file.len, modified: file.modified, info: info.clone() };
        self.sessions.lock().unwrap().insert(file.path.clone(), entry);
        info
    }

    /// Forgets the files that are gone, judged against the whole folder walk.
    fn retain(&self, files: &[TranscriptFile]) {
        let live: HashSet<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        self.sessions.lock().unwrap().retain(|path, _| live.contains(path.as_path()));
    }

    /// Starts a search and cancels any that is still running.
    pub fn begin_search(&self) -> u64 {
        self.search.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Every session transcript under the project folders, newest first.
fn transcript_files() -> Result<Vec<TranscriptFile>, String> {
    let projects = claude_config_dir().ok_or("cannot locate Claude config folder")?.join("projects");
    let mut files = Vec::new();
    for dir in fs::read_dir(&projects).map_err(|e| format!("{}: {e}", projects.display()))?.flatten() {
        let Ok(entries) = fs::read_dir(dir.path()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_session = path.extension().is_some_and(|e| e == "jsonl")
                && path.file_stem().and_then(|s| s.to_str()).is_some_and(is_session_id);
            if !is_session {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    files.push(TranscriptFile { path, len: meta.len(), modified });
                }
            }
        }
    }
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(files)
}

/// Lists the most recently used sessions across every project folder, newest first, or only
/// those whose folder is `cwd`. `agents` picks the sessions other agents started headless
/// instead of everyone else's, so a busy agent run does not push a person's sessions off the list.
///
/// The transcript format is internal to Claude Code, so every field is optional and a
/// transcript that cannot be read is skipped rather than failing the whole list. A file that
/// has not changed costs one stat.
pub fn list_sessions(index: &Index, limit: usize, cwd: Option<&str>, agents: bool) -> Result<Vec<SessionInfo>, String> {
    let files = transcript_files()?;
    index.retain(&files);
    // Lazy, so reading stops once `limit` sessions are found.
    let wanted = cwd.map(folder_key);
    Ok(files
        .iter()
        .filter_map(|file| index.info(file))
        .filter(|info| info.agent == agents && wanted.as_ref().is_none_or(|w| info.in_folder(w)))
        .take(limit)
        .map(|info| SessionInfo { cwd_exists: Path::new(&info.cwd).is_dir(), ..info })
        .collect())
}

impl SessionInfo {
    /// The session started in the folder, or was last used there. `key` is a `folder_key`.
    fn in_folder(&self, key: &str) -> bool {
        folder_key(&self.cwd) == key || self.last_cwd.as_deref().is_some_and(|c| folder_key(c) == key)
    }
}

/// Compares folders the way the UI does: either slash, no trailing slash, any case.
fn folder_key(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn is_session_id(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

#[derive(Default)]
struct Scan {
    cwd: Option<String>,
    first_cwd: Option<String>,
    started: Option<String>,
    entrypoint: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    first_prompt: Option<String>,
    /// A `bridge-session` line came first: Remote Control was on, so a person was at it (HQ, the CLI).
    bridged: bool,
    /// What the first `queue-operation` enqueued: a string for `claude -p "<prompt>"`. Prompts HQ
    /// sends as blocks (its task-list nudge rides along) are enqueued without one.
    enqueued: Option<Option<String>>,
}

fn read_session_info(path: &Path, modified: SystemTime) -> Option<SessionInfo> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut scan = Scan::default();

    // The first lines say where and when the session began. A first line longer than the head
    // cannot be parsed, and the latest folder stands in for the first.
    let head = read_range(&mut file, 0, HEAD_BYTES.min(len))?;
    for line in head.split('\n') {
        scan_origin(line, &mut scan);
        if scan.first_cwd.is_some() && scan.started.is_some() && scan.enqueued.is_some() {
            break;
        }
    }

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

    let agent = is_agent(&scan);
    let last_cwd = scan.cwd?; // a session without a folder cannot be resumed
    let cwd = scan.first_cwd.unwrap_or_else(|| last_cwd.clone());
    let last_cwd = (folder_key(&last_cwd) != folder_key(&cwd)).then_some(last_cwd);
    let modified_ms = modified.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    Some(SessionInfo {
        session_id: path.file_stem()?.to_str()?.to_owned(),
        cwd_exists: false, // not cached; filled in by whoever hands the session out
        cwd,
        last_cwd,
        title: scan.custom_title.or(scan.ai_title),
        last_prompt: scan.last_prompt.or(scan.first_prompt),
        agent,
        entrypoint: scan.entrypoint,
        started: scan.started,
        modified_ms,
    })
}

/// Whether another agent started the session headless: `claude -p "<prompt>"` through the SDK
/// entrypoint, with no Remote Control and its first prompt given as text. Claude Code does not
/// record who started a session, so this is inferred; a slash command counts as a person's.
fn is_agent(scan: &Scan) -> bool {
    scan.entrypoint.as_deref() == Some("sdk-cli")
        && !scan.bridged
        && scan.enqueued.as_ref().and_then(Option::as_deref).is_some_and(|p| !p.trim_start().starts_with('/'))
}

/// Picks the first folder and the first timestamp out of a line from the start of a transcript,
/// and how the session was started.
fn scan_origin(line: &str, scan: &mut Scan) {
    if scan.enqueued.is_none() && line.contains("\"type\":\"bridge-session\"") {
        scan.bridged = true;
    }
    if scan.enqueued.is_none() && line.contains("\"queue-operation\"") && line.contains("\"enqueue\"") {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            scan.enqueued = Some(v.get("content").and_then(Value::as_str).map(str::to_owned));
        }
    }
    let wanted = (scan.first_cwd.is_none() && line.contains("\"cwd\""))
        || (scan.started.is_none() && line.contains("\"timestamp\""));
    if !wanted {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else { return };
    let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned);
    if scan.first_cwd.is_none() {
        scan.first_cwd = text("cwd");
    }
    if scan.started.is_none() {
        scan.started = text("timestamp");
    }
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
        // A prompt with an image or HQ's task-list nudge is sent as blocks; its first text block counts.
        let content = v.pointer("/message/content");
        let prompt = content
            .and_then(Value::as_str)
            .or_else(|| content?.as_array()?.iter().filter(|b| b["type"] == "text").find_map(|b| b["text"].as_str().filter(|t| !t.starts_with(TASK_HINT_PREFIX))))
            .filter(|t| !t.trim_start().starts_with('<'));
        set(&mut scan.first_prompt, prompt.map(|t| t.lines().next().unwrap_or(t)));
    }
}

/// How the composer's task-list nudge (`TASK_HINT` in app.js) starts. It is HQ's text, not the user's.
const TASK_HINT_PREFIX: &str = "(HQ: track this work";

/// A message that matches a full-text search, and the session it is in.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHit {
    session_id: String,
    cwd: String,
    cwd_exists: bool,
    title: Option<String>,
    /// `user` or `assistant`.
    role: &'static str,
    /// About 140 characters of the message around the match.
    snippet: String,
    timestamp: Option<String>,
}

const MAX_HITS_PER_SESSION: usize = 5;

/// Searches the text of every session's messages for `query`, ignoring case, and streams each
/// match to `on_hit` as it is found: newest sessions first, a session's matches in file order,
/// one per message, `MAX_HITS_PER_SESSION` per session and `limit` in all. Returns how many it sent.
///
/// Tool calls, tool results, thinking and subagent messages are not searched. The search stops
/// quietly, returning what it sent, once a newer one has begun (`generation` is the number
/// `begin_search` gave this one) or the frontend stops listening.
pub fn search_sessions(
    index: &Index,
    generation: u64,
    query: &str,
    cwd: Option<&str>,
    limit: usize,
    on_hit: &Channel<SessionHit>,
) -> Result<usize, String> {
    if query.chars().count() < 2 {
        return Ok(0);
    }
    let files = transcript_files()?;
    index.retain(&files);
    let matcher = Matcher::new(query);
    let wanted = cwd.map(folder_key);
    let cancelled = || index.search.load(Ordering::SeqCst) != generation;

    let mut total = 0;
    for file in &files {
        if total >= limit || cancelled() {
            break;
        }
        // The session's details are only needed to answer, so a session without a match is never
        // looked up, except to apply the folder filter.
        let mut session: Option<Option<SessionInfo>> = wanted.is_some().then(|| index.info(file));
        if let Some(w) = &wanted {
            let in_folder = session.as_ref().and_then(Option::as_ref).is_some_and(|info| info.in_folder(w));
            if !in_folder {
                continue;
            }
        }
        let mut in_session = 0;
        let mut listening = true;
        // An unreadable transcript is skipped, like in the session list.
        let _ = scan_messages(&file.path, &matcher, &cancelled, &mut |role, timestamp, snippet| {
            let Some(info) = session.get_or_insert_with(|| index.info(file)) else { return false };
            let hit = SessionHit {
                session_id: info.session_id.clone(),
                cwd: info.cwd.clone(),
                cwd_exists: Path::new(&info.cwd).is_dir(),
                title: info.title.clone(),
                role,
                snippet,
                timestamp,
            };
            if cancelled() || on_hit.send(hit).is_err() {
                listening = false;
                return false;
            }
            total += 1;
            in_session += 1;
            total < limit && in_session < MAX_HITS_PER_SESSION
        });
        if !listening {
            break;
        }
    }
    Ok(total)
}

/// The fields of a user or assistant line that a search needs.
#[derive(Deserialize)]
struct MessageLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    /// Text Claude Code injected on the user's behalf (skills, reminders), not typed by them.
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    timestamp: Option<String>,
    message: Option<MessageBody>,
}

#[derive(Deserialize)]
struct MessageBody {
    content: Option<Texts>,
}

/// The text parts of a message's content: a typed prompt is a plain string, anything else is
/// an array of blocks of which only the `text` ones count. Read through a visitor so the
/// blocks that are not text (tool inputs and results can be huge) are skipped, not copied.
struct Texts(Vec<String>);

impl<'de> Deserialize<'de> for Texts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Block {
            #[serde(rename = "type")]
            kind: Option<String>,
            text: Option<String>,
        }
        struct TextsVisitor;
        impl<'de> Visitor<'de> for TextsVisitor {
            type Value = Texts;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or an array of content blocks")
            }

            fn visit_str<E>(self, text: &str) -> Result<Texts, E> {
                Ok(Texts(vec![text.to_owned()]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Texts, A::Error> {
                let mut texts = Vec::new();
                while let Some(block) = seq.next_element::<Block>()? {
                    if block.kind.as_deref() == Some("text") {
                        texts.extend(block.text.filter(|t| !t.starts_with(TASK_HINT_PREFIX)));
                    }
                }
                Ok(Texts(texts))
            }
        }
        deserializer.deserialize_any(TextsVisitor)
    }
}

/// Reads a transcript front to back and calls `on_hit(role, timestamp, snippet)` for each message
/// whose text matches, until it returns false or the transcript ends.
fn scan_messages(
    path: &Path,
    matcher: &Matcher,
    cancelled: &dyn Fn() -> bool,
    on_hit: &mut dyn FnMut(&'static str, Option<String>, String) -> bool,
) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(READ_CHUNK as usize, fs::File::open(path)?);
    let mut buf = Vec::new();
    loop {
        if cancelled() {
            return Ok(());
        }
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        // Raw checks first: most lines are not worth parsing, and a line that cannot hold the
        // query is skipped without a parse even when it is a message.
        let is_message = memmem::find(&buf, br#""type":"user""#).is_some() || memmem::find(&buf, br#""type":"assistant""#).is_some();
        if !matcher.may_match(&buf) || !is_message {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<MessageLine>(&buf) else { continue };
        let role = match line.kind.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            _ => continue,
        };
        if line.is_sidechain == Some(true) || line.is_meta == Some(true) {
            continue;
        }
        let Some(texts) = line.message.and_then(|m| m.content) else { continue };
        if let Some(snippet) = texts.0.iter().find_map(|text| matcher.find(text)) {
            if !on_hit(role, line.timestamp, snippet) {
                return Ok(());
            }
        }
    }
}

struct Matcher {
    /// The query in lowercase, matched against the lowercase of each text.
    needle: String,
    /// The query's lowercase bytes, when its raw form can be looked for in a JSON line before
    /// the line is parsed. JSON escapes only quotes, backslashes and control characters, so a
    /// query without them appears in the line as itself, apart from letter case. Non-ASCII
    /// queries skip this shortcut: case folding there is not a byte comparison.
    raw: Option<Vec<u8>>,
}

impl Matcher {
    fn new(query: &str) -> Self {
        let plain = query.is_ascii() && !query.bytes().any(|b| matches!(b, b'"' | b'\\' | 0..=0x1f | 0x7f));
        Matcher { needle: query.to_lowercase(), raw: plain.then(|| query.to_ascii_lowercase().into_bytes()) }
    }

    /// False means the raw JSON line cannot hold a match.
    fn may_match(&self, line: &[u8]) -> bool {
        self.raw.as_deref().is_none_or(|raw| contains_ascii_ci(line, raw))
    }

    /// A snippet around the first match in `text`.
    fn find(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        let at = lower.find(&self.needle)?;
        // Lowercasing a few rare characters changes a text's length; the snippet is then cut
        // from the lowercase text, because `at` is an offset into that.
        let source = if lower.len() == text.len() && text.is_char_boundary(at) { text } else { &lower };
        Some(snippet(source, at))
    }
}

/// Whether `hay` holds `needle` (which must already be ASCII lowercase), ignoring ASCII case.
fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else { return true };
    let mut from = 0;
    while let Some(i) = memchr::memchr2(first, first.to_ascii_uppercase(), &hay[from..]) {
        let at = from + i;
        if hay.len() - at < needle.len() {
            return false;
        }
        if hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// About 140 characters of `text` around the byte offset `at`, on one line.
fn snippet(text: &str, at: usize) -> String {
    const LEAD: usize = 40;
    const WIDTH: usize = 140;
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let start = text[..at].char_indices().rev().take(LEAD).last().map_or(at, |(i, _)| i);
    let end = text[start..].char_indices().nth(WIDTH).map_or(text.len(), |(i, _)| start + i);
    let body = text[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{body}{}", if start > 0 { "\u{2026}" } else { "" }, if end < text.len() { "\u{2026}" } else { "" })
}

/// Finds VS Code's executable directly so a folder path is passed as a plain argument,
/// never through `cmd /c code ...` where `&` or `^` in a path would be interpreted.
///
/// On a Mac it is the `code` command (inside the app bundle, else on PATH), run by macos.rs.
#[cfg(windows)]
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

#[cfg(not(any(windows, target_os = "macos")))]
pub fn find_vscode() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join("code")).find(|p| p.is_file())
}

pub fn is_workspace_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("code-workspace"))
}

/// A `.code-workspace` file offered for a folder in the VS Code button's menu.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFile {
    path: String,
    /// The workspace lists the folder among its own folders.
    includes_folder: bool,
}

/// Workspace files in the folder, its parent and `dirs` (where workspaces were picked before),
/// those that list the folder first.
pub fn find_workspaces(folder: &str, dirs: &[String]) -> Vec<WorkspaceFile> {
    let folder_path = Path::new(folder);
    let mut search: Vec<&Path> = vec![folder_path];
    search.extend(folder_path.parent());
    search.extend(dirs.iter().map(Path::new));
    let wanted = folder_key(folder);
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for dir in search {
        if !seen.insert(folder_key(&dir.to_string_lossy())) {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for path in entries.flatten().map(|entry| entry.path()) {
            if !is_workspace_file(&path) || !path.is_file() {
                continue;
            }
            let includes_folder = workspace_folders(&path).iter().any(|f| folder_key(f) == wanted);
            found.push(WorkspaceFile { path: path.to_string_lossy().into_owned(), includes_folder });
        }
    }
    found.sort_by_key(|w| (!w.includes_folder, w.path.to_lowercase()));
    found.truncate(30);
    found
}

/// The folders a workspace file lists, as full paths. The file is JSON with comments, so the
/// `"path"` values are picked out by hand instead of parsed; a miss only loses the ranking.
fn workspace_folders(file: &Path) -> Vec<String> {
    let Some(base) = file.parent() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(file) else { return Vec::new() };
    if text.len() > 256 * 1024 {
        return Vec::new();
    }
    let mut folders = Vec::new();
    let mut rest = text.as_str();
    while let Some(i) = rest.find("\"path\"") {
        rest = &rest[i + 6..];
        let Some(value) = rest.trim_start().strip_prefix(':').map(str::trim_start).and_then(|r| r.strip_prefix('"')) else { continue };
        let Some(end) = value.find('"') else { break };
        let raw = value[..end].replace("\\\\", "\\").replace("\\/", "/");
        folders.push(resolve(base, &raw));
    }
    folders
}

/// Joins a workspace's relative folder path onto its own folder and folds `.` and `..` away.
fn resolve(base: &Path, relative: &str) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in base.join(relative).components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_console_api_key_is_accepted() {
        let key = "sk-ant-api03-AbC_def-123";
        assert_eq!(clean_api_key(&format!("  \"{key}\"\n")).unwrap(), key);
        assert!(clean_api_key("sk-ant-oat01-abc").unwrap_err().contains("subscription token"));
        assert!(clean_api_key("sk-proj-abc").is_err());
        assert!(clean_api_key("sk-ant-api03-a b").is_err());
        assert!(clean_api_key(&format!("sk-ant-api03-{}", "a".repeat(300))).is_err());
        assert_eq!(api_key_hint(key), "sk-ant-…-123");
    }

    /// A transcript on disk that is removed again when the test ends.
    struct TempTranscript(PathBuf);

    impl TempTranscript {
        fn new(name: &str, lines: &[String], trailing_newline: bool) -> Self {
            let path = std::env::temp_dir().join(format!("hq-test-{}-{name}.jsonl", std::process::id()));
            let mut text = lines.join("\n");
            if trailing_newline {
                text.push('\n');
            }
            fs::write(&path, text).unwrap();
            TempTranscript(path)
        }
    }

    impl Drop for TempTranscript {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn user_line(i: usize, pad: usize) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"msg {i} {}"}},"timestamp":"2026-01-01T00:00:00.000Z"}}"#,
            "x".repeat(pad)
        )
    }

    const CUSTOM: &str = r#"{"type":"custom-title","customTitle":"Named","sessionId":"s"}"#;
    const AI: &str = r#"{"type":"ai-title","aiTitle":"Titled","sessionId":"s"}"#;

    #[test]
    fn loads_the_newest_lines_and_pages_back_through_the_rest() {
        let mut lines = vec![CUSTOM.to_string(), AI.to_string()];
        for i in 0..3500 {
            lines.push(user_line(i, 200));
            if i % 50 == 0 {
                lines.push(r#"{"type":"file-history-snapshot","snapshot":{}}"#.into());
                lines.push(user_line(i, 10).replace("\"isSidechain\":false", "\"isSidechain\":true"));
            }
        }
        let file = TempTranscript::new("paging", &lines, true);

        let first = read_transcript(&file.0, None).unwrap();
        assert_eq!(first.lines.len(), 2 + 1500);
        assert!(first.lines[0].contains("custom-title") && first.lines[1].contains("ai-title"));
        assert!(first.lines[2].contains("msg 2000 "));
        assert!(first.lines.last().unwrap().contains("msg 3499 "));
        let bytes = fs::read(&file.0).unwrap();
        assert!(bytes[first.start as usize..].starts_with(first.lines[2].as_bytes()));

        let second = read_transcript(&file.0, Some(first.start)).unwrap();
        assert_eq!(second.lines.len(), 1500);
        assert!(second.lines[0].contains("msg 500 ") && second.lines.last().unwrap().contains("msg 1999 "));

        let third = read_transcript(&file.0, Some(second.start)).unwrap();
        assert_eq!(third.lines.len(), 500);
        assert!(third.lines[0].contains("msg 0 ") && third.lines.last().unwrap().contains("msg 499 "));
        assert_eq!(third.start, 0);
    }

    #[test]
    fn a_title_in_the_window_replaces_the_earlier_one() {
        let mut lines = vec![AI.to_string(), CUSTOM.to_string()];
        lines.extend((0..2000).map(|i| user_line(i, 50)));
        lines.push(CUSTOM.replace("Named", "Renamed"));
        let file = TempTranscript::new("titles", &lines, true);

        let chunk = read_transcript(&file.0, None).unwrap();
        assert_eq!(chunk.lines.len(), 1 + 1500);
        assert!(chunk.lines[0].contains("ai-title"));
        assert_eq!(chunk.lines.iter().filter(|l| l.contains("custom-title")).count(), 1);
        assert!(chunk.lines.last().unwrap().contains("Renamed"));
    }

    #[test]
    fn a_byte_budget_ends_the_window_even_for_lines_longer_than_a_read() {
        let lines: Vec<String> = (0..30).map(|i| user_line(i, 300_000)).collect();
        let file = TempTranscript::new("budget", &lines, false); // no final line break

        let chunk = read_transcript(&file.0, None).unwrap();
        assert_eq!(chunk.lines.len(), 14);
        assert!(chunk.lines.last().unwrap().contains("msg 29 "));
        assert!(chunk.start > 0);
        // Every line came back whole.
        for line in &chunk.lines {
            assert!(serde_json::from_str::<serde::de::IgnoredAny>(line).is_ok());
        }
    }

    #[test]
    fn handles_a_region_that_is_empty_or_past_the_end() {
        let file = TempTranscript::new("edges", &[user_line(1, 5), user_line(2, 5)], true);
        let all = read_transcript(&file.0, Some(u64::MAX)).unwrap();
        assert_eq!(all.lines.len(), 2);
        assert_eq!(all.start, 0);
        let none = read_transcript(&file.0, Some(0)).unwrap();
        assert!(none.lines.is_empty());
        assert_eq!(none.start, 0);
        let empty = TempTranscript::new("empty", &[], false);
        assert!(read_transcript(&empty.0, None).unwrap().lines.is_empty());
    }

    #[test]
    fn image_bytes_are_blanked_in_loaded_lines() {
        let image = format!(
            r#"{{"isSidechain":false,"type":"user","message":{{"role":"user","content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{}"}}}},{{"type":"text","text":"look"}}]}},"toolUseResult":{{"file":{{"base64":"{}","type":"pdf"}}}}}}"#,
            "iVBOR+/w0K".repeat(60_000),
            "JVBERi0=".repeat(1000)
        );
        let file = TempTranscript::new("image", &[image], true);

        let chunk = read_transcript(&file.0, None).unwrap();
        assert_eq!(chunk.lines.len(), 1);
        assert!(chunk.lines[0].len() < 400, "{}", chunk.lines[0].len());
        let v: Value = serde_json::from_str(&chunk.lines[0]).unwrap();
        assert_eq!(v["message"]["content"][0]["source"]["data"], "");
        assert_eq!(v["message"]["content"][0]["source"]["media_type"], "image/png");
        assert_eq!(v["message"]["content"][1]["text"], "look");
        assert_eq!(v["toolUseResult"]["file"]["base64"], "");
    }

    #[test]
    fn short_or_unrelated_data_values_stay() {
        let short = r#"{"data":"abc","x":{"data":"dGVzdA=="}}"#;
        assert!(matches!(blank_base64(short), Cow::Borrowed(_)));
        let long_text = format!(r#"{{"data":"{} not base64"}}"#, "a".repeat(400));
        assert!(matches!(blank_base64(&long_text), Cow::Borrowed(_)));
        // Quotes in a string value are escaped, so quoted JSON is not mistaken for a field.
        let quoted = format!(r#"{{"text":"\"data\":\"{}\""}}"#, "A".repeat(400));
        assert!(matches!(blank_base64(&quoted), Cow::Borrowed(_)));
    }

    fn search_lines() -> Vec<String> {
        let line = |kind: &str, flags: &str, content: &str| {
            format!(r#"{{"type":"{kind}",{flags}"timestamp":"2026-02-03T04:05:06.000Z","message":{{"content":{content}}}}}"#)
        };
        vec![
            line("user", "", r#""Please fix the FROBNICATOR now""#),
            line("assistant", "", r#"[{"type":"thinking","thinking":"frobnicator thoughts"},{"type":"text","text":"I fixed the frobnicator today"}]"#),
            line("assistant", "", r#"[{"type":"tool_use","name":"Bash","input":{"command":"grep frobnicator"}}]"#),
            line("user", "", r#"[{"type":"tool_result","tool_use_id":"t","content":"frobnicator found"}]"#),
            line("user", r#""isMeta":true,"#, r#""frobnicator skill text""#),
            line("assistant", r#""isSidechain":true,"#, r#"[{"type":"text","text":"a subagent says frobnicator"}]"#),
            line("user", "", r#"[{"type":"text","text":"say \"hello\" to the frobnicator"}]"#),
            r#"{"type":"file-history-snapshot","note":"frobnicator"}"#.to_string(),
        ]
    }

    fn hits(file: &TempTranscript, query: &str) -> Vec<(&'static str, Option<String>, String)> {
        let mut found = Vec::new();
        scan_messages(&file.0, &Matcher::new(query), &|| false, &mut |role, timestamp, snippet| {
            found.push((role, timestamp, snippet));
            true
        })
        .unwrap();
        found
    }

    #[test]
    fn search_reads_message_text_only() {
        let file = TempTranscript::new("search", &search_lines(), true);
        let found = hits(&file, "frobnicator");
        let roles: Vec<_> = found.iter().map(|h| h.0).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(found[0].1.as_deref(), Some("2026-02-03T04:05:06.000Z"));
        assert_eq!(found[0].2, "Please fix the FROBNICATOR now");
        assert_eq!(found[1].2, "I fixed the frobnicator today");
        assert_eq!(found[2].2, r#"say "hello" to the frobnicator"#);
    }

    #[test]
    fn headless_sessions_started_by_agents_are_told_apart() {
        let user = r#"{"type":"user","cwd":"C:\\proj","entrypoint":"sdk-cli","message":{"content":"Review the diff"}}"#;
        let info = |name: &str, head: &[&str]| {
            let lines: Vec<String> = head.iter().chain([&user]).map(|l| l.to_string()).collect();
            let file = TempTranscript::new(name, &lines, true);
            read_session_info(&file.0, SystemTime::now()).unwrap()
        };
        let enqueue = |content: &str| format!(r#"{{"type":"queue-operation","operation":"enqueue","content":{content}}}"#);
        let bridge = r#"{"type":"bridge-session","bridgeSessionId":"cse_1"}"#;
        assert!(info("agent-p", &[&enqueue(r#""Review the diff""#)]).agent);
        assert!(!info("agent-hq", &[&enqueue("null")]).agent, "HQ sends its prompts as blocks");
        assert!(!info("agent-bridged", &[bridge, &enqueue(r#""Fix it""#)]).agent, "Remote Control means a person");
        assert!(!info("agent-slash", &[&enqueue(r#""/log-hours""#)]).agent);
    }

    #[test]
    fn task_hint_is_neither_a_title_nor_a_search_hit() {
        let prompt = r#"{"type":"user","message":{"content":[{"type":"text","text":"Fix the frobnicator\nplease"},{"type":"text","text":"(HQ: track this work with TaskCreate)"}]}}"#;
        let mut scan = Scan::default();
        scan_line(prompt, &mut scan, false);
        assert_eq!(scan.first_prompt.as_deref(), Some("Fix the frobnicator"));
        let file = TempTranscript::new("search-hint", &[prompt.to_string()], true);
        assert_eq!(hits(&file, "frobnicator").len(), 1);
        assert!(hits(&file, "TaskCreate").is_empty());
    }

    #[test]
    fn search_finds_queries_that_json_escapes() {
        let file = TempTranscript::new("search-escape", &search_lines(), true);
        assert_eq!(hits(&file, r#""hello""#).len(), 1);
        assert_eq!(hits(&file, "nothing like this").len(), 0);
    }

    #[test]
    fn search_stops_when_asked() {
        let file = TempTranscript::new("search-stop", &search_lines(), true);
        let mut count = 0;
        scan_messages(&file.0, &Matcher::new("frobnicator"), &|| false, &mut |_, _, _| {
            count += 1;
            false
        })
        .unwrap();
        assert_eq!(count, 1);
        let mut cancelled = 0;
        scan_messages(&file.0, &Matcher::new("frobnicator"), &|| true, &mut |_, _, _| {
            cancelled += 1;
            true
        })
        .unwrap();
        assert_eq!(cancelled, 0);
    }

    #[test]
    fn matcher_ignores_case_and_decides_when_a_raw_check_is_safe() {
        assert!(Matcher::new("foo bar").raw.is_some());
        assert!(Matcher::new(r#"say "hi""#).raw.is_none());
        assert!(Matcher::new("back\\slash").raw.is_none());
        assert!(Matcher::new("caf\u{e9}").raw.is_none());
        assert_eq!(Matcher::new("CAF\u{c9}").find("A Caf\u{e9} au lait").as_deref(), Some("A Caf\u{e9} au lait"));
        assert!(Matcher::new("zzz").find("A Caf\u{e9} au lait").is_none());
        // A character whose lowercase is longer than itself still yields a snippet.
        assert!(Matcher::new("\u{130}x").find("\u{130}\u{130}\u{130}\u{130}x").is_some());
    }

    #[test]
    fn raw_check_ignores_ascii_case() {
        assert!(contains_ascii_ci(b"Hello World", b"o w"));
        assert!(contains_ascii_ci(b"Hello World", b"hello world"));
        assert!(contains_ascii_ci(b"xxHELLO", b"hello"));
        assert!(!contains_ascii_ci(b"Hello", b"hello!"));
        assert!(!contains_ascii_ci(b"Hell", b"hello"));
        assert!(contains_ascii_ci(b"anything", b""));
    }

    #[test]
    fn snippets_are_short_single_lines_cut_on_char_boundaries() {
        let text = format!("{}needle{}", "\u{e9}\n ".repeat(100), "\u{e9}\t".repeat(100));
        let at = text.find("needle").unwrap();
        let s = snippet(&text, at);
        assert!(s.starts_with('\u{2026}') && s.ends_with('\u{2026}'));
        assert!(s.contains("needle"));
        assert!(!s.contains('\n') && !s.contains("  "));
        assert!(s.chars().count() <= 142);
        assert_eq!(snippet("short text", 6), "short text");
        // An offset inside a character is moved back instead of panicking.
        assert_eq!(snippet("\u{e9}\u{e9}", 1), "\u{e9}\u{e9}");
    }

    #[test]
    fn the_session_cache_keeps_a_file_until_it_changes_and_forgets_missing_ones() {
        let index = Index::default();
        let line = r#"{"type":"user","cwd":"C:\\proj","entrypoint":"sdk-cli","message":{"content":"first prompt"}}"#;
        let file = TempTranscript::new("cache", &[line.to_string()], true);
        let meta = fs::metadata(&file.0).unwrap();
        let entry = TranscriptFile { path: file.0.clone(), len: meta.len(), modified: meta.modified().unwrap() };

        let info = index.info(&entry).unwrap();
        assert_eq!(info.cwd, r"C:\proj");
        assert_eq!(info.last_prompt.as_deref(), Some("first prompt"));

        // Same length and time: answered from the cache even though the file is gone.
        fs::remove_file(&file.0).unwrap();
        assert_eq!(index.info(&entry).unwrap().cwd, r"C:\proj");
        // A changed length means the file is read again, which now fails.
        let changed = TranscriptFile { len: entry.len + 1, ..entry };
        assert!(index.info(&changed).is_none());

        index.retain(&[]);
        assert!(index.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn a_session_keeps_the_folder_and_time_it_started_in() {
        let lines = [
            r#"{"type":"user","cwd":"C:\\proj","timestamp":"2026-03-04T05:06:07.000Z","message":{"content":"first prompt"}}"#.to_string(),
            r#"{"type":"user","cwd":"C:\\proj\\sub","timestamp":"2026-03-05T00:00:00.000Z","message":{"content":"later"}}"#.to_string(),
        ];
        let file = TempTranscript::new("origin", &lines, true);
        let meta = fs::metadata(&file.0).unwrap();
        let entry = TranscriptFile { path: file.0.clone(), len: meta.len(), modified: meta.modified().unwrap() };
        let info = Index::default().info(&entry).unwrap();
        assert_eq!(info.cwd, r"C:\proj");
        assert_eq!(info.last_cwd.as_deref(), Some(r"C:\proj\sub"));
        assert_eq!(info.started.as_deref(), Some("2026-03-04T05:06:07.000Z"));
        assert!(info.in_folder(&folder_key(r"C:\Proj\")) && info.in_folder(&folder_key(r"c:/proj/sub")));
        assert!(!info.in_folder(&folder_key(r"C:\other")));
    }
}

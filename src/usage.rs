//! Token usage and estimated cost per session, read from the `usage` Claude Code records on
//! every reply in its own transcripts. Nothing is sent anywhere and no model is asked: this only
//! reads files that are already on disk, and only when the usage dashboard asks for them.
//!
//! The transcript format is internal to Claude Code, so every field is optional and a line that
//! cannot be read is skipped.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use memchr::memmem;
use serde::{Deserialize, Serialize};

use crate::store;

/// What the usage dashboard has read, kept in managed state. An entry holds only while the
/// file's length and modified time are unchanged, so a second look costs one stat per file.
#[derive(Default)]
pub struct UsageIndex {
    files: Mutex<HashMap<PathBuf, Cached>>,
}

struct Cached {
    len: u64,
    modified: SystemTime,
    usage: FileUsage,
}

#[derive(Clone, Default)]
struct FileUsage {
    /// The first folder a line names; the session's own folder comes from its main transcript.
    cwd: Option<String>,
    buckets: Vec<Bucket>,
}

/// Usage at one request timestamp on one model, split between the main thread and subagents.
/// Keep timestamps intact: even UTC hours can straddle midnight in the user's time zone.
#[derive(Serialize, Clone, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    /// Request time, in ms since the Unix epoch (UTC).
    at: i64,
    model: String,
    /// Spent by subagents (Task/Agent tool calls) rather than the main conversation.
    sub: bool,
    requests: u32,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    /// At API list prices, in USD; `None` for a model the price table does not know.
    cost: Option<f64>,
}

/// A session's usage for the dashboard, with the name the resume list gives it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    session_id: String,
    cwd: Option<String>,
    title: Option<String>,
    modified_ms: u64,
    rows: Vec<Bucket>,
}

/// Every session's usage since `since_ms` (all of it when `None`), most recently changed first.
pub fn report(sessions: &store::Index, index: &UsageIndex, since_ms: Option<i64>) -> Result<Vec<SessionUsage>, String> {
    let projects = store::claude_config_dir().ok_or("cannot locate Claude config folder")?.join("projects");
    let files = usage_files(&projects)?;
    let live: HashSet<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    index.files.lock().unwrap().retain(|path, _| live.contains(path.as_path()));

    let since = since_ms.unwrap_or(i64::MIN);
    let mut by_session: HashMap<String, SessionUsage> = HashMap::new();
    for file in &files {
        let modified_ms = file.modified.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        // A file last written before the period holds nothing in it, and is not even read.
        if (modified_ms as i64) < since {
            continue;
        }
        let usage = index.usage(file);
        let rows = usage.rows_since(since);
        let entry = by_session.entry(file.session_id.clone()).or_insert_with(|| SessionUsage {
            session_id: file.session_id.clone(),
            cwd: None,
            title: None,
            modified_ms: 0,
            rows: Vec::new(),
        });
        if !file.sub {
            if let Some((cwd, title)) = sessions.session_name(&file.path, file.len, file.modified) {
                entry.cwd = Some(cwd);
                entry.title = title;
            }
        }
        if entry.cwd.is_none() {
            entry.cwd = usage.cwd;
        }
        entry.modified_ms = entry.modified_ms.max(modified_ms);
        entry.rows.extend(rows);
    }
    let mut out: Vec<SessionUsage> = by_session.into_values().filter(|s| !s.rows.is_empty()).collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
    Ok(out)
}

struct UsageFile {
    path: PathBuf,
    session_id: String,
    /// A subagent's transcript, filed under `<session>/subagents/`.
    sub: bool,
    len: u64,
    modified: SystemTime,
}

impl FileUsage {
    fn rows_since(&self, since: i64) -> Vec<Bucket> {
        self.buckets.iter().filter(|b| b.at >= since).cloned().collect()
    }
}

impl UsageIndex {
    fn usage(&self, file: &UsageFile) -> FileUsage {
        let cached = {
            let files = self.files.lock().unwrap();
            files.get(&file.path).filter(|c| c.len == file.len && c.modified == file.modified).map(|c| c.usage.clone())
        };
        if let Some(usage) = cached {
            return usage;
        }
        // The lock is not held while reading, so the rest of HQ never waits on it.
        let usage = read_usage(&file.path, file.sub).unwrap_or_default();
        let entry = Cached { len: file.len, modified: file.modified, usage: usage.clone() };
        self.files.lock().unwrap().insert(file.path.clone(), entry);
        usage
    }
}

/// Each session's transcript (`<project>/<session>.jsonl`) and its subagents' transcripts
/// (`<project>/<session>/subagents/*.jsonl`), which are billed to the same session.
fn usage_files(projects: &Path) -> Result<Vec<UsageFile>, String> {
    let mut files = Vec::new();
    let mut add = |path: PathBuf, session_id: &str, sub: bool, meta: fs::Metadata| {
        if let Ok(modified) = meta.modified() {
            files.push(UsageFile { path, session_id: session_id.to_owned(), sub, len: meta.len(), modified });
        }
    };
    for dir in fs::read_dir(projects).map_err(|e| format!("{}: {e}", projects.display()))?.flatten() {
        let Ok(entries) = fs::read_dir(dir.path()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()).filter(|s| store::is_session_id(s)).map(str::to_owned) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                let Ok(subs) = fs::read_dir(path.join("subagents")) else { continue };
                for sub in subs.flatten() {
                    let sub_path = sub.path();
                    if sub_path.extension().is_some_and(|e| e == "jsonl") {
                        if let Ok(meta) = sub.metadata() {
                            add(sub_path, &stem, true, meta);
                        }
                    }
                }
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                add(path, &stem, false, meta);
            }
        }
    }
    Ok(files)
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
    server_tool_use: Option<ServerToolUse>,
    speed: Option<String>,
    inference_geo: Option<String>,
}

#[derive(Deserialize)]
struct CacheCreation {
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ServerToolUse {
    web_search_requests: Option<u64>,
}

/// One API response's usage, in the units the price table needs.
#[derive(Default, Debug, PartialEq)]
struct Request {
    input: u64,
    output: u64,
    write_5m: u64,
    write_1h: u64,
    read: u64,
    web_searches: u64,
    fast: bool,
    us_only: bool,
}

fn read_usage(path: &Path, sub: bool) -> io::Result<FileUsage> {
    let mut reader = BufReader::with_capacity(256 * 1024, fs::File::open(path)?);
    let mut buckets: HashMap<(i64, String), Bucket> = HashMap::new();
    // Claude Code writes each content block of a reply as its own line, every one carrying the
    // reply's full usage, so a reply is counted once by its message id.
    let mut seen: HashSet<String> = HashSet::new();
    let mut cwd = None;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        // Most lines (tool results, snapshots, progress) are skipped without parsing them.
        if memmem::find(&raw, br#""usage""#).is_none() || memmem::find(&raw, br#""assistant""#).is_none() {
            if cwd.is_none() && memmem::find(&raw, br#""cwd""#).is_some() {
                cwd = serde_json::from_slice::<Line>(&raw).ok().and_then(|l| l.cwd);
            }
            continue;
        }
        let Ok(line) = serde_json::from_slice::<Line>(&raw) else { continue };
        if cwd.is_none() {
            cwd = line.cwd;
        }
        let (Some("assistant"), Some(message)) = (line.kind.as_deref(), line.message) else { continue };
        let (Some(model), Some(usage)) = (message.model, message.usage) else { continue };
        if model.starts_with('<') {
            continue; // `<synthetic>`: a reply Claude Code wrote itself, with no request behind it
        }
        if let Some(id) = message.id {
            if !seen.insert(id) {
                continue;
            }
        }
        let Some(at) = line.timestamp.as_deref().and_then(parse_time_ms) else { continue };
        let request = Request::from(&usage);
        let bucket = buckets.entry((at, model.clone())).or_insert_with(|| Bucket {
            at,
            model: model.clone(),
            sub,
            cost: Some(0.0),
            ..Bucket::default()
        });
        bucket.requests += 1;
        bucket.input += request.input;
        bucket.output += request.output;
        bucket.cache_write += request.write_5m + request.write_1h;
        bucket.cache_read += request.read;
        bucket.cost = match (bucket.cost, cost(&model, &request)) {
            (Some(total), Some(c)) => Some(total + c),
            _ => None,
        };
    }
    let mut buckets: Vec<Bucket> = buckets.into_values().collect();
    buckets.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.model.cmp(&b.model)));
    Ok(FileUsage { cwd, buckets })
}

impl From<&Usage> for Request {
    fn from(u: &Usage) -> Self {
        let written = u.cache_creation_input_tokens.unwrap_or(0);
        // Older CLIs report no split; whatever is not named as the 1-hour kind is priced as the 5-minute one.
        let write_1h = u.cache_creation.as_ref().and_then(|c| c.ephemeral_1h_input_tokens).unwrap_or(0).min(written);
        Request {
            input: u.input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
            write_5m: written - write_1h,
            write_1h,
            read: u.cache_read_input_tokens.unwrap_or(0),
            web_searches: u.server_tool_use.as_ref().and_then(|s| s.web_search_requests).unwrap_or(0),
            fast: u.speed.as_deref() == Some("fast"),
            us_only: u.inference_geo.as_deref() == Some("us"),
        }
    }
}

/// USD per million tokens: base input, 5-minute cache write, 1-hour cache write, cache read, output.
type Rates = [f64; 5];

/// The family and version of a model id: `claude-opus-4-1-20250805` and `claude-3-5-haiku-latest`
/// read as ("opus", "4.1") and ("haiku", "3.5"). Platform prefixes such as `us.anthropic.` and
/// suffixes such as `-v1:0` or `[1m]` are ignored.
fn model_key(model: &str) -> Option<(&str, String)> {
    let start = model.find("claude-")? + "claude-".len();
    let parts: Vec<&str> = model[start..].split(['-', '@', '[', ':']).collect();
    let is_version = |p: &&str| !p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit());
    let lead: Vec<&str> = parts.iter().copied().take_while(is_version).collect();
    let (family, version) = if lead.is_empty() {
        let family = *parts.first()?;
        (family, parts[1..].iter().copied().take_while(is_version).collect::<Vec<_>>())
    } else {
        (*parts.get(lead.len())?, lead) // the older naming puts the version first
    };
    Some((family, version.join(".")))
}

/// List prices from https://platform.claude.com/docs/en/about-claude/pricing (October 2026).
/// `prompt` is the request's whole prompt (input, cache reads and writes), which picks Haiku 5.5's tier.
fn rates(family: &str, version: &str, prompt: u64) -> Option<Rates> {
    Some(match (family, version) {
        ("fable" | "mythos", "5.1") => [10.0, 12.5, 20.0, 0.25, 50.0],
        ("fable" | "mythos", "5") => [10.0, 12.5, 20.0, 1.0, 50.0],
        ("opus", "5.5") => [4.0, 5.0, 8.0, 0.20, 20.0],
        ("opus", "5" | "4.8" | "4.7" | "4.6" | "4.5") => [5.0, 6.25, 10.0, 0.50, 25.0],
        ("opus", "4.1" | "4") => [15.0, 18.75, 30.0, 1.50, 75.0],
        ("sonnet", "5.5") => [2.0, 2.5, 4.0, 0.10, 10.0],
        ("sonnet", "5") => [2.0, 2.5, 4.0, 0.20, 10.0],
        ("sonnet", "4.6" | "4.5" | "4") => [3.0, 3.75, 6.0, 0.30, 15.0],
        ("haiku", "5.5") if prompt > 100_000 => [0.50, 0.625, 1.0, 0.05, 2.50],
        ("haiku", "5.5") => [0.10, 0.125, 0.20, 0.01, 0.50],
        ("haiku", "4.5") => [1.0, 1.25, 2.0, 0.10, 5.0],
        ("haiku", "3.5") => [0.80, 1.0, 1.60, 0.08, 4.0],
        _ => return None,
    })
}

/// What one request would cost at API list prices, or `None` for a model without a known price.
fn cost(model: &str, r: &Request) -> Option<f64> {
    let (family, version) = model_key(model)?;
    let [input, write_5m, write_1h, read, output] = rates(family, &version, r.input + r.write_5m + r.write_1h + r.read)?;
    let tokens = r.input as f64 * input
        + r.write_5m as f64 * write_5m
        + r.write_1h as f64 * write_1h
        + r.read as f64 * read
        + r.output as f64 * output;
    let mut usd = tokens / 1_000_000.0;
    // Fast mode doubles every rate on the models that have it; caching multipliers stack on top.
    if r.fast && family == "opus" && matches!(version.as_str(), "5.5" | "5" | "4.8") {
        usd *= 2.0;
    }
    // US-only inference costs 1.1x on Claude 4.6 and later.
    let before_4_6 = matches!((family, version.as_str()), ("opus", "4.5" | "4.1" | "4") | ("sonnet", "4.5" | "4") | ("haiku", "4.5" | "3.5"));
    if r.us_only && !before_4_6 {
        usd *= 1.1;
    }
    Some(usd + r.web_searches as f64 * 0.01) // web search: $10 per 1,000
}

/// `2026-10-28T14:35:41.790Z` (or with a `+hh:mm` offset) as ms since the Unix epoch.
fn parse_time_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut i = 19;
    let mut ms = 0;
    if b.get(i) == Some(&b'.') {
        let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        ms = num(i + 1, digits.min(3))? * 10_i64.pow(3 - digits.min(3) as u32);
        i += 1 + digits;
    }
    let offset_min = match b.get(i) {
        Some(b'Z') | None => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let m = num(i + 1, 2)? * 60 + num(i + 4, 2)?;
            if sign == b'+' { m } else { -m }
        }
        _ => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute - offset_min) * 60_000 + second * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(id: &str, at: &str, model: &str, usage: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","cwd":"/work/app","sessionId":"s","message":{{"id":"{id}","model":"{model}","content":[{{"type":"text","text":"hi"}}],"usage":{usage}}}}}"#
        )
    }

    #[test]
    fn reads_iso_timestamps() {
        assert_eq!(parse_time_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time_ms("2026-10-28T14:35:41.790Z"), Some(1_793_198_141_790));
        assert_eq!(parse_time_ms("2026-10-28T16:35:41.79+02:00"), Some(1_793_198_141_790));
        assert_eq!(parse_time_ms("2024-02-29T00:00:00.000Z"), Some(1_709_164_800_000));
        assert_eq!(parse_time_ms("yesterday"), None);
        assert_eq!(parse_time_ms("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn names_models_old_and_new() {
        assert_eq!(model_key("claude-opus-5-5"), Some(("opus", "5.5".into())));
        assert_eq!(model_key("claude-opus-4-20250514"), Some(("opus", "4".into())));
        assert_eq!(model_key("claude-opus-4-1-20250805"), Some(("opus", "4.1".into())));
        assert_eq!(model_key("claude-sonnet-4-5-20250929"), Some(("sonnet", "4.5".into())));
        assert_eq!(model_key("claude-3-5-haiku-20241022"), Some(("haiku", "3.5".into())));
        assert_eq!(model_key("us.anthropic.claude-haiku-4-5-20251001-v1:0"), Some(("haiku", "4.5".into())));
        assert_eq!(model_key("claude-opus-4-6[1m]"), Some(("opus", "4.6".into())));
        assert_eq!(model_key("claude-opus-4-5@20251101"), Some(("opus", "4.5".into())));
        assert_eq!(model_key("gpt-5"), None);
    }

    #[test]
    fn prices_each_kind_of_token() {
        let r = Request { input: 1_000_000, output: 1_000_000, write_5m: 1_000_000, write_1h: 1_000_000, read: 1_000_000, ..Request::default() };
        let near = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(near(cost("claude-opus-5-5", &r), 4.0 + 5.0 + 8.0 + 0.2 + 20.0));
        assert!(near(cost("claude-sonnet-4-5-20250929", &r), 3.0 + 3.75 + 6.0 + 0.3 + 15.0));
        assert!(near(cost("claude-opus-5-5", &Request { fast: true, ..Request { output: 1_000_000, ..Request::default() } }), 40.0));
        assert!(near(cost("claude-opus-5", &Request { us_only: true, output: 1_000_000, ..Request::default() }), 27.5));
        assert!(near(cost("claude-opus-4-5", &Request { us_only: true, output: 1_000_000, ..Request::default() }), 25.0));
        assert!(near(cost("claude-sonnet-5-5", &Request { web_searches: 3, ..Request::default() }), 0.03));
        assert_eq!(cost("claude-3-7-sonnet-20250219", &r), None);
    }

    #[test]
    fn haiku_5_5_prices_by_prompt_length() {
        let small = Request { input: 50_000, read: 50_000, output: 1_000_000, ..Request::default() };
        let large = Request { input: 50_000, read: 50_001, output: 1_000_000, ..Request::default() };
        assert!((cost("claude-haiku-5-5", &small).unwrap() - (0.005 + 0.0005 + 0.5)).abs() < 1e-9);
        assert!((cost("claude-haiku-5-5", &large).unwrap() - (0.025 + 0.050001 * 0.05 + 2.5)).abs() < 1e-9);
    }

    #[test]
    fn splits_cache_writes_by_lifetime() {
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":2,"output_tokens":5,"cache_creation_input_tokens":300,"cache_read_input_tokens":7,
                "cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200},"speed":"standard"}"#,
        )
        .unwrap();
        assert_eq!(Request::from(&usage), Request { input: 2, output: 5, write_5m: 100, write_1h: 200, read: 7, ..Request::default() });
        let old: Usage = serde_json::from_str(r#"{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":40,"cache_read_input_tokens":null}"#).unwrap();
        assert_eq!(Request::from(&old).write_5m, 40);
    }

    #[test]
    fn counts_each_reply_once_and_preserves_timestamps() {
        let u = r#"{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":1000}"#;
        let lines = [
            r#"{"type":"user","cwd":"/work/app","message":{"role":"user","content":"go"}}"#.to_owned(),
            assistant("a", "2026-10-28T14:05:00.000Z", "claude-opus-5-5", u),
            assistant("a", "2026-10-28T14:05:01.000Z", "claude-opus-5-5", u), // a second block of the same reply
            assistant("b", "2026-10-28T14:59:00.000Z", "claude-opus-5-5", u),
            assistant("c", "2026-10-28T15:00:00.000Z", "claude-opus-5-5", u),
            assistant("d", "2026-10-28T15:10:00.000Z", "claude-haiku-5-5", u),
            assistant("e", "2026-10-28T15:11:00.000Z", "<synthetic>", u),
            "not json at all".to_owned(),
        ];
        let path = std::env::temp_dir().join(format!("hq-usage-{}.jsonl", std::process::id()));
        fs::write(&path, lines.join("\n")).unwrap();
        let usage = read_usage(&path, false).unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(usage.cwd.as_deref(), Some("/work/app"));
        let hour = parse_time_ms("2026-10-28T14:00:00Z").unwrap();
        let summary: Vec<(i64, &str, u32, u64, u64)> =
            usage.buckets.iter().map(|b| (b.at - hour, b.model.as_str(), b.requests, b.output, b.cache_read)).collect();
        assert_eq!(
            summary,
            vec![(300_000, "claude-opus-5-5", 1, 20, 1000), (3_540_000, "claude-opus-5-5", 1, 20, 1000),
                 (3_600_000, "claude-opus-5-5", 1, 20, 1000), (4_200_000, "claude-haiku-5-5", 1, 20, 1000)]
        );
        let first = &usage.buckets[0];
        assert!((first.cost.unwrap() - (10.0 * 4.0 + 20.0 * 20.0 + 1000.0 * 0.2) / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn cached_usage_filters_exactly_at_fractional_offset_midnight() {
        let path = std::env::temp_dir().join(format!("hq-usage-range-{}.jsonl", std::process::id()));
        let u = r#"{"input_tokens":10,"output_tokens":20}"#;
        let lines = [
            assistant("before", "2026-10-27T18:29:59.999Z", "claude-opus-5-5", u),
            assistant("boundary", "2026-10-27T18:30:00.000Z", "claude-opus-5-5", u),
            assistant("after", "2026-10-27T18:40:00.000Z", "claude-opus-5-5", u),
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let meta = fs::metadata(&path).unwrap();
        let file = UsageFile { path: path.clone(), session_id: "session".into(), sub: true,
            len: meta.len(), modified: meta.modified().unwrap() };
        let index = UsageIndex::default();
        assert_eq!(index.usage(&file).rows_since(i64::MIN).len(), 3);
        // A warm cache must support changing ranges without retaining the previous day's usage.
        let since = parse_time_ms("2026-10-28T00:00:00+05:30").unwrap();
        let rows = index.usage(&file).rows_since(since);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].at, since);
        assert!(rows.iter().all(|b| b.sub));
        assert_eq!(rows.iter().map(|b| b.output).sum::<u64>(), 40);
        assert_eq!(index.usage(&file).rows_since(i64::MIN).len(), 3);
        assert!(index.usage(&file).rows_since(since + 3_600_000).is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn finds_subagent_transcripts_under_their_session() {
        let root = std::env::temp_dir().join(format!("hq-usage-tree-{}", std::process::id()));
        let id = "d19ab9a0-7d92-556b-8554-a32a70d780bf";
        let project = root.join("-work-app");
        fs::create_dir_all(project.join(id).join("subagents")).unwrap();
        fs::write(project.join(format!("{id}.jsonl")), "").unwrap();
        fs::write(project.join(id).join("subagents").join("agent-x.jsonl"), "").unwrap();
        fs::write(project.join(id).join("subagents").join("agent-x.meta.json"), "{}").unwrap();
        fs::write(project.join("notes.jsonl"), "").unwrap();
        let mut found: Vec<(String, bool)> = usage_files(&root).unwrap().into_iter().map(|f| (f.session_id, f.sub)).collect();
        let _ = fs::remove_dir_all(&root);
        found.sort();
        assert_eq!(found, vec![(id.to_owned(), false), (id.to_owned(), true)]);
    }
}

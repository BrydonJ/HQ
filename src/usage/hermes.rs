//! Hermes Agent's usage for the token dashboard, read from the SQLite file Hermes keeps its sessions
//! in (`state.db` in its home folder). The file is opened read-only: HQ never writes to it, and a
//! missing, locked or unreadable file only means there are no Hermes rows to show.
//!
//! Hermes records no per-request usage: `messages.token_count` is never filled in, and the only
//! token counts are totals per session and model (`session_model_usage`) with the first and last
//! time each was used. Each total therefore becomes one bucket at its last use, so a session that
//! runs across midnight lands wholly on the day it was last used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};

use super::{cost_over, Bucket, Request, SessionUsage};

/// Length and modified time of `state.db` and of `state.db-wal`. Hermes keeps the file in WAL mode,
/// so a write lands in the second one and the first only changes when it is folded back in.
type Stamp = [Option<(u64, SystemTime)>; 2];

/// Everything read from the file, with the state of the file at the time. A second look at an
/// unchanged file costs two stats.
#[derive(Default)]
pub(super) struct Cache(Mutex<Option<(Stamp, Vec<SessionUsage>)>>);

/// Hermes' sessions with usage since `since` (ms since the Unix epoch), or none when Hermes is not there.
pub(super) fn report(cache: &Cache, since: i64) -> Vec<SessionUsage> {
    match crate::mcp::hermes_home().map(|home| home.join("state.db")) {
        Some(db) if db.is_file() => sessions_since(cache, &db, since),
        _ => Vec::new(),
    }
}

fn sessions_since(cache: &Cache, db: &Path, since: i64) -> Vec<SessionUsage> {
    let stamp = stamp(db);
    let cached = cache.0.lock().unwrap().as_ref().filter(|(s, _)| *s == stamp).map(|(_, all)| all.clone());
    let all = cached.unwrap_or_else(|| {
        // The lock is not held while reading, so the rest of HQ never waits on it.
        match read(db) {
            Ok(all) => {
                *cache.0.lock().unwrap() = Some((stamp, all.clone()));
                all
            }
            // A locked or half-written file is tried again at the next look, not remembered as empty.
            Err(e) => {
                eprintln!("HQ: Hermes usage unreadable ({}): {e}", db.display());
                Vec::new()
            }
        }
    });
    all.into_iter()
        .filter_map(|mut s| {
            s.rows.retain(|b| b.at >= since);
            (!s.rows.is_empty()).then_some(s)
        })
        .collect()
}

fn stamp(db: &Path) -> Stamp {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    [db.to_owned(), PathBuf::from(wal)].map(|p| std::fs::metadata(p).ok().and_then(|m| Some((m.len(), m.modified().ok()?))))
}

/// What the dashboard needs to know about a session row.
struct Meta {
    source: String,
    parent: Option<String>,
    title: Option<String>,
    cwd: Option<String>,
    /// Last activity, in ms since the Unix epoch.
    active_ms: u64,
}

fn millis(secs: f64) -> i64 {
    (secs * 1000.0) as i64
}

fn tokens(n: i64) -> u64 {
    n.max(0) as u64
}

/// The session at the top of `id`'s chain of parents (itself when it has none, or when the parent
/// is not in the file). The walk is capped so a loop in a damaged file cannot hang it.
fn root_of<'a>(id: &'a str, metas: &'a HashMap<String, Meta>) -> &'a str {
    let mut at = id;
    for _ in 0..32 {
        match metas.get(at).and_then(|m| m.parent.as_deref()).filter(|p| metas.contains_key(*p)) {
            Some(parent) => at = parent,
            None => break,
        }
    }
    at
}

/// Hermes' own cost where HQ's price table has no entry for the model (the table covers Claude only).
/// `None` when Hermes has no figure either, so that the dashboard counts the tokens as unpriced.
fn price(model: &str, calls: u64, r: &Request, estimated: f64, actual: f64) -> Option<f64> {
    // Cache writes carry no lifetime in Hermes' counts, so they are priced as the 5-minute kind.
    cost_over(model, r, calls).or_else(|| [actual, estimated].into_iter().find(|c| *c > 0.0))
}

/// Every session in the file with its usage. Subagent sessions (and any other session that has a
/// parent) have their rows filed under the session at the top of the chain, marked `sub`.
fn read(db: &Path) -> rusqlite::Result<Vec<SessionUsage>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    // Hermes writes while HQ reads; wait a moment for a lock rather than give up on the first.
    conn.busy_timeout(Duration::from_millis(500))?;

    let mut metas: HashMap<String, Meta> = HashMap::new();
    let mut stmt = conn.prepare("SELECT id, source, parent_session_id, title, cwd, COALESCE(last_activity_at, ended_at, started_at) FROM sessions")?;
    let mut found = stmt.query([])?;
    while let Some(row) = found.next()? {
        let non_empty = |s: Option<String>| s.filter(|s| !s.is_empty());
        metas.insert(row.get(0)?, Meta {
            source: row.get(1)?,
            parent: non_empty(row.get(2)?),
            title: non_empty(row.get(3)?),
            cwd: non_empty(row.get(4)?),
            active_ms: millis(row.get::<_, Option<f64>>(5)?.unwrap_or(0.0)).max(0) as u64,
        });
    }

    let mut by_root: HashMap<String, SessionUsage> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT session_id, model, api_call_count, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
         estimated_cost_usd, actual_cost_usd, COALESCE(last_seen, first_seen) \
         FROM session_model_usage WHERE COALESCE(last_seen, first_seen) IS NOT NULL",
    )?;
    let mut found = stmt.query([])?;
    while let Some(row) = found.next()? {
        let id: String = row.get(0)?;
        let model: String = row.get(1)?;
        let calls = tokens(row.get(2)?);
        let request = Request {
            input: tokens(row.get(3)?),
            output: tokens(row.get(4)?),
            read: tokens(row.get(5)?),
            write_5m: tokens(row.get(6)?),
            ..Request::default()
        };
        let (estimated, actual): (f64, f64) = (row.get(7)?, row.get(8)?);
        let meta = metas.get(&id);
        let root = root_of(&id, &metas);
        let root_meta = metas.get(root);
        let entry = by_root.entry(root.to_owned()).or_insert_with(|| SessionUsage {
            agent: "hermes",
            session_id: root.to_owned(),
            cwd: root_meta.and_then(|m| m.cwd.clone()),
            title: root_meta.and_then(|m| m.title.clone()),
            modified_ms: 0,
            rows: Vec::new(),
        });
        if entry.cwd.is_none() {
            entry.cwd = meta.and_then(|m| m.cwd.clone());
        }
        entry.modified_ms = entry.modified_ms.max(meta.map_or(0, |m| m.active_ms));
        entry.rows.push(Bucket {
            at: millis(row.get(9)?),
            cost: price(&model, calls, &request, estimated, actual),
            model,
            sub: meta.is_some_and(|m| m.source == "subagent" || m.parent.is_some()),
            requests: u32::try_from(calls).unwrap_or(u32::MAX),
            input: request.input,
            output: request.output,
            cache_write: request.write_5m,
            cache_read: request.read,
        });
    }
    let mut out: Vec<SessionUsage> = by_root.into_values().collect();
    for s in &mut out {
        s.rows.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.model.cmp(&b.model)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state.db with the columns this module reads, in a folder of its own that goes away with it.
    struct TestDb {
        dir: PathBuf,
        path: PathBuf,
        conn: Connection,
    }

    impl TestDb {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("hq-usage-hermes-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("state.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, parent_session_id TEXT, title TEXT, cwd TEXT,
                     started_at REAL NOT NULL, ended_at REAL, last_activity_at REAL);
                 CREATE TABLE session_model_usage (session_id TEXT NOT NULL, model TEXT NOT NULL, task TEXT NOT NULL DEFAULT '',
                     api_call_count INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0,
                     output_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                     cache_write_tokens INTEGER NOT NULL DEFAULT 0, estimated_cost_usd REAL NOT NULL DEFAULT 0,
                     actual_cost_usd REAL NOT NULL DEFAULT 0, first_seen REAL, last_seen REAL,
                     PRIMARY KEY (session_id, model, task));",
            )
            .unwrap();
            TestDb { dir, path, conn }
        }

        fn session(&self, id: &str, source: &str, parent: Option<&str>, title: &str, active: f64) {
            self.conn
                .execute(
                    "INSERT INTO sessions (id, source, parent_session_id, title, cwd, started_at, last_activity_at) VALUES (?1, ?2, ?3, ?4, 'F:\\Repos\\City', ?5, ?5)",
                    rusqlite::params![id, source, parent, title, active],
                )
                .unwrap();
        }

        /// Usage of `model` in session `id`, last used at `last_seen` (seconds): calls, input, output, cache read, cache write.
        fn usage(&self, id: &str, model: &str, last_seen: f64, [calls, input, output, read, write]: [i64; 5], estimated: f64) {
            self.conn
                .execute(
                    "INSERT INTO session_model_usage (session_id, model, api_call_count, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, estimated_cost_usd, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                    rusqlite::params![id, model, calls, input, output, read, write, estimated, last_seen],
                )
                .unwrap();
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn all(db: &TestDb) -> Vec<SessionUsage> {
        sessions_since(&Cache::default(), &db.path, i64::MIN)
    }

    #[test]
    fn prices_like_claude_and_matches_hermes_own_estimate() {
        let db = TestDb::new("price");
        db.session("s1", "local", None, "Work", 1_795_079_000.0);
        // A real row from a Hermes file: Hermes' own estimate for it was 1.6231565999999997.
        db.usage("s1", "claude-opus-5-5", 1_795_079_221.3, [40, 82, 20915, 3_107_418, 116_609], 1.6231566);
        let sessions = all(&db);
        assert_eq!(sessions.len(), 1);
        let row = &sessions[0].rows[0];
        assert!((row.cost.unwrap() - 1.6231566).abs() < 1e-9, "{:?}", row.cost);
        assert_eq!((row.requests, row.input, row.output, row.cache_read, row.cache_write), (40, 82, 20915, 3_107_418, 116_609));
        assert_eq!(row.at, 1_795_079_221_300);
        assert_eq!(sessions[0].agent, "hermes");
        assert_eq!(sessions[0].cwd.as_deref(), Some("F:\\Repos\\City"));
    }

    #[test]
    fn unknown_models_use_hermes_figure_or_stay_unpriced() {
        let db = TestDb::new("unknown");
        db.session("s1", "local", None, "Work", 1_795_079_000.0);
        db.usage("s1", "claude-fable-5-2", 1_795_079_001.0, [10, 100, 200, 3000, 0], 7.5);
        db.usage("s1", "gpt-6.1-sol", 1_795_079_002.0, [5, 100, 200, 3000, 0], 0.0);
        let rows = &all(&db)[0].rows;
        assert_eq!(rows[0].cost, Some(7.5));
        assert_eq!(rows[1].cost, None);
        assert_eq!(rows[1].input + rows[1].output + rows[1].cache_read, 3300);
    }

    #[test]
    fn picks_haiku_tier_by_the_average_request() {
        let db = TestDb::new("haiku");
        db.session("s1", "local", None, "Work", 1_795_079_000.0);
        // 40 requests of about 50k prompt tokens each: the cheap tier, though the sum is over 100k.
        db.usage("s1", "claude-haiku-5-5", 1_795_079_001.0, [40, 0, 0, 2_000_000, 0], 0.0);
        let cost = all(&db)[0].rows[0].cost.unwrap();
        assert!((cost - 2.0 * 0.01).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn files_subagents_under_their_session() {
        let db = TestDb::new("subs");
        db.session("root", "local", None, "Orchestrator", 1_795_070_000.0);
        db.session("mid", "subagent", Some("root"), "Subagent: one", 1_795_075_000.0);
        db.session("leaf", "subagent", Some("mid"), "Subagent: two", 1_795_078_000.0);
        db.session("compressed", "desktop", Some("root"), "Subagent: three", 1_795_072_000.0);
        db.session("alone", "subagent", Some("gone"), "Subagent: orphan", 1_795_060_000.0);
        db.usage("root", "claude-opus-5-5", 1_795_070_001.0, [1, 10, 10, 10, 10], 0.0);
        db.usage("mid", "claude-opus-5-5", 1_795_075_001.0, [1, 10, 10, 10, 10], 0.0);
        db.usage("leaf", "claude-opus-5-5", 1_795_078_001.0, [1, 10, 10, 10, 10], 0.0);
        db.usage("compressed", "claude-opus-5-5", 1_795_072_001.0, [1, 10, 10, 10, 10], 0.0);
        db.usage("alone", "claude-opus-5-5", 1_795_060_001.0, [1, 10, 10, 10, 10], 0.0);
        let mut sessions = all(&db);
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["alone", "root"]);
        let root = &sessions[1];
        assert_eq!(root.title.as_deref(), Some("Orchestrator"));
        assert_eq!(root.modified_ms, 1_795_078_000_000);
        assert_eq!(root.rows.iter().filter(|b| b.sub).count(), 3);
        assert_eq!(root.rows.iter().filter(|b| !b.sub).count(), 1);
        assert!(sessions[0].rows[0].sub); // its parent is not in the file, so it stands alone
    }

    #[test]
    fn keeps_only_rows_since_the_start_of_the_period() {
        let db = TestDb::new("since");
        db.session("s1", "local", None, "Work", 1_795_079_000.0);
        db.session("s2", "local", None, "Old", 1_795_000_000.0);
        db.usage("s1", "claude-opus-5-5", 1_795_000_001.0, [1, 1, 1, 1, 1], 0.0);
        db.usage("s1", "claude-sonnet-5-5", 1_795_079_001.0, [1, 1, 1, 1, 1], 0.0);
        db.usage("s2", "claude-opus-5-5", 1_795_000_002.0, [1, 1, 1, 1, 1], 0.0);
        let cache = Cache::default();
        let sessions = sessions_since(&cache, &db.path, 1_795_050_000_000);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].rows.len(), 1);
        assert_eq!(sessions[0].rows[0].model, "claude-sonnet-5-5");
        // The second look comes from the cache and still filters to its own period.
        assert_eq!(sessions_since(&cache, &db.path, i64::MIN).iter().map(|s| s.rows.len()).sum::<usize>(), 3);
    }

    #[test]
    fn rereads_a_file_that_changed() {
        let db = TestDb::new("changed");
        db.session("s1", "local", None, "Work", 1_795_079_000.0);
        db.usage("s1", "claude-opus-5-5", 1_795_079_001.0, [1, 1, 1, 1, 1], 0.0);
        let cache = Cache::default();
        assert_eq!(sessions_since(&cache, &db.path, i64::MIN)[0].rows.len(), 1);
        db.usage("s1", "claude-sonnet-5-5", 1_795_079_002.0, [1, 1, 1, 1, 1], 0.0);
        assert_eq!(sessions_since(&cache, &db.path, i64::MIN)[0].rows.len(), 2);
    }

    #[test]
    fn a_missing_or_foreign_file_is_no_rows() {
        let dir = std::env::temp_dir().join(format!("hq-usage-hermes-{}-missing", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(sessions_since(&Cache::default(), &dir.join("state.db"), i64::MIN).is_empty());
        std::fs::write(dir.join("state.db"), b"this is not a database").unwrap();
        assert!(sessions_since(&Cache::default(), &dir.join("state.db"), i64::MIN).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reads the Hermes file on this machine and prints what the dashboard would get.
    /// Run with `cargo test --lib prints_this_machines_hermes_usage -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn prints_this_machines_hermes_usage() {
        let started = std::time::Instant::now();
        let sessions = report(&Cache::default(), i64::MIN);
        println!("{} sessions in {:?}", sessions.len(), started.elapsed());
        for s in sessions.iter().take(5) {
            println!("{} {:?} {:?} modified {} rows {}", s.session_id, s.title, s.cwd, s.modified_ms, s.rows.len());
            for b in s.rows.iter().take(3) {
                println!("  {b:?}");
            }
        }
    }
}

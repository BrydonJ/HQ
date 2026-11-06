//! HQ's side of the session host (host.rs): starts this build's host, finds the hosts earlier
//! builds left running, and carries sessions' lines between them and the cells.
//!
//! Every failure here falls back to running the session inside HQ as before (session.rs): a host
//! that cannot start, answer, or be reached costs the sessions' surviving an update, never the
//! sessions themselves. `HQ_NO_HOST=1` turns the host off.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hasher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::host::{hosts_dir, hq_dir, Discovery, PROTO};
use crate::session::{Outgoing, Sessions};

/// What the host's executable is called. Not `hq`: the installer stops every `hq.exe` by name.
const HOST_EXE_PREFIX: &str = "hq-session-host-";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// After a host failed to start, sessions run inside HQ for this long before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long init keeps retrying hosts that do not answer, all of them together. A host still
/// unreached then has its sessions stopped once it answers (`stop_when_reachable`).
const INIT_RETRY_BUDGET: Duration = Duration::from_secs(8);
/// How long a caller waits for init: longer than init can take (its retry budget, one more
/// connect of up to 6s, and starting this build's host), so no caller sees half the hosts.
const READY_WAIT: Duration = Duration::from_secs(60);
/// How long sessions wait for HQ to come back from an update.
pub const UPDATE_GRACE_SECS: u64 = 30 * 60;
/// The host's own wait after HQ goes without a word (host.rs `LOST_GRACE`).
pub const LOST_GRACE_SECS: u64 = 15 * 60;

/// A connection to one host.
pub struct HostConn {
    /// Lines for the host, written by a thread of its own so a multi-MB prompt never stalls the caller.
    writer: mpsc::Sender<String>,
    next_req: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    /// host session id -> where its lines go.
    sinks: Mutex<HashMap<u64, Sink>>,
    /// Lines an attach is replaying, by host session id, until the attach's reply.
    replays: Mutex<HashMap<u64, String>>,
    /// Spawns given up on: a late success is killed, since HQ started nothing for it.
    abandoned: Mutex<HashSet<u64>>,
    /// Sessions whose attach gave up: their late replay lines and reply are dropped.
    dead_attaches: Mutex<HashSet<u64>>,
    alive: AtomicBool,
    pub pid: u32,
    pub exe: String,
    /// How to reach the host again, should this connection break while it lives on.
    disc: Discovery,
}

pub struct Sink {
    /// The id the frontend knows the session by.
    pub local: u64,
    pub tx: Option<mpsc::Sender<Outgoing>>,
}

enum Pending {
    Reply(mpsc::Sender<Result<Value, String>>),
    /// The sink goes in when the reply is read, ahead of the session's first line.
    Spawn(mpsc::Sender<Result<Value, String>>, Sink),
    Attach(mpsc::Sender<Result<Value, String>>, u64, Sink),
}

#[derive(Deserialize)]
struct Incoming<'a> {
    t: &'a str,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    r: u8,
    #[serde(default, rename = "in")]
    inbound: bool,
    #[serde(default)]
    s: u8,
    #[serde(borrow)]
    line: Option<&'a RawValue>,
    #[serde(default)]
    req: Option<u64>,
}

impl HostConn {
    fn connect(app: &AppHandle, d: &Discovery) -> Result<Arc<HostConn>, String> {
        let (reader, writer, hello) = handshake(d)?;
        let (wtx, wrx) = mpsc::channel::<String>();
        let conn = Arc::new(HostConn {
            writer: wtx,
            next_req: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            sinks: Mutex::new(HashMap::new()),
            replays: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(HashSet::new()),
            dead_attaches: Mutex::new(HashSet::new()),
            alive: AtomicBool::new(true),
            pid: d.pid,
            exe: hello["exe"].as_str().unwrap_or_default().to_owned(),
            disc: d.clone(),
        });
        let mut writer = writer;
        let me = conn.clone();
        std::thread::spawn(move || {
            for line in wrx {
                if writer.write_all(line.as_bytes()).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
            // Gone or broken: the reader sees the socket close and ends the host's sessions here.
            me.alive.store(false, Ordering::Relaxed);
            let _ = writer.shutdown(std::net::Shutdown::Both);
        });
        let (app, me) = (app.clone(), conn.clone());
        std::thread::spawn(move || me.read_loop(&app, reader));
        Ok(conn)
    }
}

/// Connects to a host and runs the handshake: the reader, the writer and the host's hello.
fn handshake(d: &Discovery) -> Result<(BufReader<TcpStream>, TcpStream, Value), String> {
    if d.proto != PROTO {
        return Err(format!("host {} speaks protocol {}", d.pid, d.proto));
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], d.port));
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;
    writer.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    // The host proves it knows the token before anything else is said: whoever holds the
    // port of a host that died gets no environment, prompt or token from HQ.
    let mut read = || -> Result<Value, String> {
        let mut line = String::new();
        reader.by_ref().take(64 * 1024).read_line(&mut line).map_err(|e| e.to_string())?;
        serde_json::from_str(line.trim()).map_err(|_| "the session host answered nonsense".to_string())
    };
    let nonce = crate::mcp::new_token();
    writeln!(writer, "{}", json!({ "t": "hello", "nonce": nonce, "proto": PROTO })).map_err(|e| e.to_string())?;
    let challenge = read()?;
    let theirs = challenge["nonce"].as_str().unwrap_or_default().to_owned();
    let proved = challenge["proof"].as_str().is_some_and(|p| crate::host::same_token(p, &crate::host::proof(&d.token, "host", &nonce)));
    if challenge["t"] != "challenge" || !proved || theirs.is_empty() {
        return Err(format!("port {} is not the session host it claims to be", d.port));
    }
    writeln!(writer, "{}", json!({ "t": "auth", "proof": crate::host::proof(&d.token, "client", &theirs) })).map_err(|e| e.to_string())?;
    let hello = read()?;
    writer.set_read_timeout(None).map_err(|e| e.to_string())?;
    if hello["t"] != "hello" || hello["proto"].as_u64() != Some(PROTO) || hello["pid"].as_u64() != Some(u64::from(d.pid)) {
        return Err("the host did not say hello".into());
    }
    Ok((reader, writer, hello))
}

/// Connects to a host found as HQ starts, trying again for a few seconds: one busy starting a
/// session may be slow to answer, and a host skipped leaves its sessions running unseen.
fn connect_retrying(app: &AppHandle, d: &Discovery, until: Instant) -> Result<Arc<HostConn>, String> {
    let mut wait = Duration::from_millis(250);
    loop {
        match HostConn::connect(app, d) {
            Err(_) if wait <= Duration::from_secs(2) && Instant::now() + wait < until && pid_alive(d.pid) => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            r => return r,
        }
    }
}

/// A live host HQ could not reach as it started. Its sessions' cells came back from their
/// transcripts without them, so a resend there would start a second agent on the same
/// conversation while the first runs on: they are stopped as soon as the host answers. (One
/// that never does, speaking another protocol, ends them itself once its grace runs out.)
fn stop_when_reachable(d: Discovery) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(UPDATE_GRACE_SECS);
        while Instant::now() < deadline && pid_alive(d.pid) {
            std::thread::sleep(Duration::from_secs(2));
            if handshake(&d).is_ok() {
                stop_sessions(&d);
                return;
            }
        }
    });
}

/// The connection to a host broke while the host lives on (a socket error, not a crash). Its
/// sessions would carry on unseen while their cells say they stopped, and a resend would start
/// a second agent on the same conversation: so they are stopped, over a fresh connection, first.
/// HQ is the host's only client, so all of them were this connection's (a spawn sent just before
/// the break too).
fn stop_sessions(d: &Discovery) {
    let Ok((mut reader, mut writer, _)) = handshake(d) else { return };
    let _ = writer.set_read_timeout(Some(REPLY_TIMEOUT));
    if writeln!(writer, "{}", json!({ "t": "kill_all", "req": 1 })).is_ok() {
        let mut line = String::new();
        let _ = reader.read_line(&mut line); // its reply: they are stopped
    }
    let _ = writer.shutdown(std::net::Shutdown::Both);
}

impl HostConn {

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn write(&self, msg: &Value) -> Result<(), String> {
        if !self.alive() {
            return Err("the session host is gone".into());
        }
        self.writer.send(format!("{msg}\n")).map_err(|_| "the session host is gone".to_string())
    }

    fn request(&self, mut msg: Value, pending: impl FnOnce(mpsc::Sender<Result<Value, String>>) -> Pending) -> Result<Value, String> {
        let req = self.next_req.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(req, pending(tx));
        msg["req"] = json!(req);
        if let Err(e) = self.write(&msg) {
            self.pending.lock().unwrap().remove(&req);
            return Err(e);
        }
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(r) => r,
            Err(_) => {
                let mut pending = self.pending.lock().unwrap();
                if let Some(p) = pending.remove(&req) {
                    match p {
                        Pending::Attach(_, hid, _) => {
                            self.dead_attaches.lock().unwrap().insert(hid);
                            self.replays.lock().unwrap().remove(&hid);
                        }
                        _ => {
                            self.abandoned.lock().unwrap().insert(req);
                        }
                    }
                    return Err("the session host did not answer".into());
                }
                drop(pending);
                // The reader took it just now and is sending the result (it never blocks doing so).
                rx.recv().unwrap_or_else(|_| Err("the session host did not answer".into()))
            }
        }
    }

    fn ask(&self, msg: Value) -> Result<Value, String> {
        self.request(msg, Pending::Reply)
    }

    /// Starts a session; returns the host's id for it and when it started.
    pub fn spawn(&self, msg: Value, sink: Sink) -> Result<(u64, u64), String> {
        let r = self.request(msg, |tx| Pending::Spawn(tx, sink))?;
        Ok((r["id"].as_u64().ok_or("no id")?, r["startedMs"].as_u64().unwrap_or(0)))
    }

    /// Points the session's lines at `sink`, which first gets one `hq_replay` line holding
    /// everything the host kept of it.
    /// (A timeout marks the session's replay dead, in `request`: its late lines are dropped.)
    pub fn attach(&self, hid: u64, from_ms: u64, sink: Sink) -> Result<(), String> {
        self.request(json!({ "t": "attach", "id": hid, "fromMs": from_ms }), |tx| Pending::Attach(tx, hid, sink)).map(|_| ())
    }

    pub fn send(&self, hid: u64, line: &str) -> Result<(), String> {
        self.write(&json!({ "t": "send", "id": hid, "line": line }))
    }

    pub fn kill(&self, hid: u64) {
        let _ = self.write(&json!({ "t": "kill", "id": hid }));
    }

    pub fn list(&self) -> Result<Vec<Value>, String> {
        let r = self.ask(json!({ "t": "list" }))?;
        Ok(r["sessions"].as_array().cloned().unwrap_or_default())
    }

    pub fn kill_all(&self) {
        let _ = self.ask(json!({ "t": "kill_all" }));
    }

    pub fn detach(&self, grace_secs: u64) {
        let _ = self.ask(json!({ "t": "detach", "graceSecs": grace_secs }));
    }

    fn retire(&self) {
        let _ = self.ask(json!({ "t": "retire" }));
    }

    /// Whether a cell is attached to the session (its lines have somewhere to go).
    pub fn has_cell(&self, hid: u64) -> bool {
        self.sinks.lock().unwrap().get(&hid).is_some_and(|s| s.tx.is_some())
    }

    pub fn drop_sink(&self, hid: u64) {
        self.sinks.lock().unwrap().remove(&hid);
    }

    pub fn add_sink(&self, hid: u64, sink: Sink) {
        self.sinks.lock().unwrap().insert(hid, sink);
    }

    fn read_loop(self: Arc<Self>, app: &AppHandle, mut reader: BufReader<TcpStream>) {
        let mut buf = Vec::with_capacity(64 * 1024);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(text) = std::str::from_utf8(&buf) else { continue };
            let Ok(msg) = serde_json::from_str::<Incoming>(text.trim()) else { continue };
            match msg.t {
                "out" => {
                    let Some(line) = msg.line else { continue };
                    if msg.r == 1 {
                        if self.dead_attaches.lock().unwrap().contains(&msg.id) {
                            continue;
                        }
                        let mut replays = self.replays.lock().unwrap();
                        let acc = replays.entry(msg.id).or_default();
                        acc.push_str(if acc.is_empty() { "" } else { "," });
                        acc.push_str(&format!(r#"{{"seq":{},"in":{},"state":{},"line":{}}}"#, msg.seq, msg.inbound, msg.s == 1, line.get()));
                        continue;
                    }
                    self.deliver(app, msg.id, line.get());
                }
                "reply" => {
                    let Some(req) = msg.req else { continue };
                    let full: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
                    let result = match full["err"].as_str() {
                        Some(e) => Err(e.to_owned()),
                        None => Ok(full.clone()),
                    };
                    let Some(p) = self.pending.lock().unwrap().remove(&req) else {
                        // A spawn that answered too late: nothing here runs it, so it must not run.
                        if self.abandoned.lock().unwrap().remove(&req) {
                            if let Some(hid) = full["id"].as_u64() {
                                self.kill(hid);
                            }
                        }
                        if let Some(hid) = full["attached"].as_u64() {
                            self.dead_attaches.lock().unwrap().remove(&hid);
                        }
                        continue;
                    };
                    match p {
                        Pending::Reply(tx) => {
                            let _ = tx.send(result);
                        }
                        Pending::Spawn(tx, sink) => {
                            if let Some(hid) = result.as_ref().ok().and_then(|r| r["id"].as_u64()) {
                                // Both before the next line, which may already be the session's exit.
                                app.state::<Sessions>().insert_hosted(sink.local, self.clone(), hid);
                                self.sinks.lock().unwrap().insert(hid, sink);
                            }
                            let _ = tx.send(result);
                        }
                        Pending::Attach(tx, hid, sink) => {
                            let entries = self.replays.lock().unwrap().remove(&hid).unwrap_or_default();
                            if result.is_ok() {
                                let ended = self.replay(app, hid, sink, &full, &entries);
                                if ended {
                                    end_local(app, &self, hid);
                                }
                            }
                            let _ = tx.send(result);
                        }
                    }
                }
                _ => {}
            }
        }
        // The host is gone (it crashed, or was killed): its sessions went with it.
        self.alive.store(false, Ordering::Relaxed);
        // Or only the connection is, and the host's sessions run on: they are stopped before HQ
        // says so, and before a spawn waiting here falls back to running inside HQ. Not while HQ
        // closes for an update, when they are meant to run on.
        if pid_alive(self.pid) && !crate::update::updating() {
            stop_sessions(&self.disc);
        }
        for (_, p) in self.pending.lock().unwrap().drain() {
            let tx = match p {
                Pending::Reply(tx) | Pending::Spawn(tx, _) | Pending::Attach(tx, _, _) => tx,
            };
            let _ = tx.send(Err("the session host is gone".into()));
        }
        let hids: Vec<u64> = self.sinks.lock().unwrap().keys().copied().collect();
        let exit = json!({ "type": "hq_exit", "code": -1, "stderr": ["HQ lost the background process that ran this session."] }).to_string();
        for hid in hids {
            self.deliver(app, hid, &exit);
        }
    }

    /// Hands a live line to the session's cell. The exit is the last thing it gets.
    fn deliver(&self, app: &AppHandle, hid: u64, line: &str) {
        let exit = is_exit(line);
        {
            let mut sinks = self.sinks.lock().unwrap();
            let Some(sink) = sinks.get_mut(&hid) else { return };
            if let Some(tx) = &sink.tx {
                let out = if exit { Outgoing::Exit(line.to_owned()) } else { Outgoing::Line(line.to_owned()) };
                if tx.send(out).is_err() {
                    sink.tx = None; // the cell went (a reload): the next attach gives it a new one
                }
            }
        }
        if exit {
            end_local(app, self, hid);
        }
    }

    /// Sends the cell what the host kept, then makes `sink` the session's. True when the session
    /// had ended.
    fn replay(&self, _app: &AppHandle, hid: u64, sink: Sink, reply: &Value, entries: &str) -> bool {
        let ended = entries.contains(r#""type":"hq_exit""#) && last_is_exit(entries);
        let line = format!(
            r#"{{"type":"hq_replay","seen":{},"truncated":{},"startedMs":{},"entries":[{entries}]}}"#,
            reply["seen"].as_u64().unwrap_or(0),
            reply["truncated"].as_bool().unwrap_or(false),
            reply["startedMs"].as_u64().unwrap_or(0),
        );
        if let Some(tx) = &sink.tx {
            let _ = tx.send(if ended { Outgoing::Exit(line) } else { Outgoing::Line(line) });
        }
        self.sinks.lock().unwrap().insert(hid, sink);
        ended
    }
}

/// Whether a session line is the exit the host wrote (its keys come sorted: code, stderr, type).
fn is_exit(line: &str) -> bool {
    line.starts_with(r#"{"code":"#) && line.ends_with(r#""type":"hq_exit"}"#)
}

/// Whether the last replayed entry is the exit.
fn last_is_exit(entries: &str) -> bool {
    entries.ends_with(r#""type":"hq_exit"}}"#)
}

/// The session has ended: HQ forgets it, and its MCP token stops working.
fn end_local(app: &AppHandle, conn: &HostConn, hid: u64) {
    let Some(sink) = conn.sinks.lock().unwrap().remove(&hid) else { return };
    app.state::<Sessions>().forget(sink.local);
    crate::mcp::release(sink.local);
}

/// The hosts HQ knows of, and which one new sessions go to.
#[derive(Default)]
pub struct Link {
    state: Mutex<LinkState>,
    ready: Condvar,
}

#[derive(Default)]
struct LinkState {
    ready: bool,
    profile: Option<String>,
    conns: Vec<Arc<HostConn>>,
    primary: Option<Arc<HostConn>>,
    failed_at: Option<Instant>,
}

pub fn disabled() -> bool {
    std::env::var("HQ_NO_HOST").is_ok_and(|v| !v.is_empty() && v != "0")
}

impl Link {
    pub fn set_profile(&self, profile: Option<String>) {
        self.state.lock().unwrap().profile = profile;
    }

    /// Connects to every host running for this profile and starts this build's own. Runs once,
    /// off the UI thread, as HQ starts.
    pub fn init(&self, app: &AppHandle, profile: Option<String>) {
        let mut conns = Vec::new();
        let mut primary = None;
        let mine = host_exe().ok();
        let retry_until = Instant::now() + INIT_RETRY_BUDGET;
        if !disabled() {
            if let Some(dir) = hosts_dir(profile.as_deref()) {
                for (path, d) in discoveries(&dir) {
                    // Connected already (one a spawn started meanwhile): a second connection would replace it.
                    if self.state.lock().unwrap().conns.iter().any(|c| c.pid == d.pid && c.alive()) {
                        continue;
                    }
                    match connect_retrying(app, &d, retry_until) {
                        Ok(conn) => {
                            let own = mine.as_deref().is_some_and(|m| same_path(Path::new(&conn.exe), m));
                            if own && primary.is_none() {
                                primary = Some(conn.clone());
                            } else {
                                conn.retire(); // ends once its sessions have
                            }
                            conns.push(conn);
                        }
                        Err(_) if !pid_alive(d.pid) => {
                            let _ = fs::remove_file(path);
                        }
                        Err(e) => {
                            eprintln!("HQ could not reach session host {}: {e}; its sessions will be stopped", d.pid);
                            stop_when_reachable(d);
                        }
                    }
                }
            }
        }
        {
            // Merged: a spawn that gave up waiting may have started a host meanwhile.
            let mut st = self.state.lock().unwrap();
            st.profile = profile;
            st.conns.extend(conns);
            if st.primary.as_ref().is_none_or(|p| !p.alive()) {
                st.primary = primary;
            } else if let Some(extra) = primary {
                extra.retire();
            }
        }
        if !disabled() {
            let _ = self.ensure_primary(app);
            remove_old_exes(&self.state.lock().unwrap().conns);
        }
        self.state.lock().unwrap().ready = true;
        self.ready.notify_all();
    }

    fn wait_ready(&self) {
        let mut st = self.state.lock().unwrap();
        let deadline = Instant::now() + READY_WAIT;
        while !st.ready {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            st = self.ready.wait_timeout(st, left).unwrap().0;
        }
    }

    /// The host new sessions go to, started if need be. None: run the session inside HQ.
    pub fn primary(&self, app: &AppHandle) -> Option<Arc<HostConn>> {
        if disabled() {
            return None;
        }
        self.wait_ready();
        // Still finding the hosts: starting one now could race init for the same sessions.
        if !self.state.lock().unwrap().ready {
            return None;
        }
        self.ensure_primary(app)
    }

    fn ensure_primary(&self, app: &AppHandle) -> Option<Arc<HostConn>> {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = st.primary.as_ref().filter(|p| p.alive()) {
            return Some(p.clone());
        }
        if st.failed_at.is_some_and(|t| t.elapsed() < RETRY_AFTER) {
            return None;
        }
        match start_host(app, st.profile.as_deref()) {
            Ok(conn) => {
                st.conns.retain(|c| c.alive());
                st.conns.push(conn.clone());
                st.primary = Some(conn.clone());
                st.failed_at = None;
                Some(conn)
            }
            Err(e) => {
                eprintln!("HQ session host unavailable, running sessions inside HQ: {e}");
                st.failed_at = Some(Instant::now());
                None
            }
        }
    }

    pub fn conns(&self) -> Vec<Arc<HostConn>> {
        self.wait_ready();
        self.state.lock().unwrap().conns.iter().filter(|c| c.alive()).cloned().collect()
    }
}

/// Every discovery file in `dir`, read.
fn discoveries(dir: &Path) -> Vec<(PathBuf, Discovery)> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let d = serde_json::from_slice::<Discovery>(&fs::read(&p).ok()?).ok()?;
            Some((p, d))
        })
        .collect()
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// This build's copy of the executable for the host: named after a hash of its bytes, so every
/// build (dev builds share a version) gets its own, and one in use is never overwritten.
pub fn host_exe() -> Result<PathBuf, String> {
    static CACHED: Mutex<Option<PathBuf>> = Mutex::new(None);
    // Held throughout: two threads must not write the copy at once.
    let mut cached = CACHED.lock().unwrap();
    if let Some(p) = cached.clone().filter(|p| p.is_file()) {
        return Ok(p);
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let bytes = fs::read(&me).map_err(|e| e.to_string())?;
    // SipHash with fixed keys: stable across runs, which is all a name needs.
    #[allow(deprecated)]
    let mut h = std::hash::SipHasher::new();
    h.write(&bytes);
    let dir = hq_dir().ok_or("no home folder")?.join("bin");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(format!("{HOST_EXE_PREFIX}{:016x}{}", h.finish(), std::env::consts::EXE_SUFFIX));
    if !target.is_file() || fs::metadata(&target).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        let tmp = dir.join(format!(".{}.{}.tmp", HOST_EXE_PREFIX, std::process::id()));
        fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
        }
        if let Err(e) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            if !target.is_file() {
                return Err(e.to_string());
            }
        }
    }
    *cached = Some(target.clone());
    Ok(target)
}

/// Copies left by older builds whose hosts have ended. One still running cannot be removed on
/// Windows; on Unix its host was found above and is skipped.
fn remove_old_exes(conns: &[Arc<HostConn>]) {
    let Ok(mine) = host_exe() else { return };
    let Some(dir) = mine.parent() else { return };
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !name.starts_with(HOST_EXE_PREFIX) || same_path(&path, &mine) {
            continue;
        }
        if conns.iter().any(|c| c.alive() && same_path(Path::new(&c.exe), &path)) || in_use_elsewhere(&path) {
            continue;
        }
        let _ = fs::remove_file(&path);
    }
}

/// Whether a live host of any profile (the dev copy beside the installed one) runs from `exe`.
fn in_use_elsewhere(exe: &Path) -> bool {
    let Some(dir) = hq_dir() else { return false };
    let Ok(entries) = fs::read_dir(dir) else { return false };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("hosts"))
        .flat_map(|e| discoveries(&e.path()))
        .any(|(_, d)| pid_alive(d.pid) && same_path(Path::new(&d.exe), exe))
}

/// Starts this build's host outside HQ's job and process group, and connects to it.
fn start_host(app: &AppHandle, profile: Option<&str>) -> Result<Arc<HostConn>, String> {
    let exe = host_exe()?;
    let dir = hosts_dir(profile).ok_or("no home folder")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--session-host");
    if let Some(p) = profile {
        cmd.args(["--profile", p]);
    }
    // In its own folder, so it holds no other open (HQ's install folder, which an uninstall removes).
    let _ = fs::create_dir_all(&dir);
    cmd.current_dir(&dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = spawn_detached(cmd)?;
    let pid = child.id();
    // Reaps the host when it ends, so no zombie stays behind on Unix.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let file = dir.join(format!("{pid}.json"));
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(d) = fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Discovery>(&b).ok()) {
            return HostConn::connect(app, &d);
        }
        if Instant::now() > deadline {
            return Err("the session host did not start in time".into());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(windows)]
fn spawn_detached(mut cmd: Command) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(c) => Ok(c),
        // HQ is in a job that forbids breaking away (started from a terminal that uses one, say).
        // The host still outlives HQ unless that job's owner closes too.
        Err(_) => {
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn().map_err(|e| e.to_string())
        }
    }
}

#[cfg(unix)]
fn spawn_detached(mut cmd: Command) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| e.to_string())
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// The environment a child started from `cmd` would get, as the host is to give it.
pub fn effective_env(cmd: &Command) -> serde_json::Map<String, Value> {
    let mut env: HashMap<String, String> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect();
    for (k, v) in cmd.get_envs() {
        let k = k.to_string_lossy().into_owned();
        // Windows variable names ignore case.
        if cfg!(windows) {
            let lower = k.to_ascii_lowercase();
            env.retain(|key, _| key.to_ascii_lowercase() != lower);
        }
        match v {
            Some(v) => {
                env.insert(k, v.to_string_lossy().into_owned());
            }
            None => {
                env.remove(&k);
            }
        }
    }
    env.into_iter().map(|(k, v)| (k, Value::String(v))).collect()
}

/// The spawn message for `cmd`, which must be built but not started.
pub fn spawn_message(kind: &str, cmd: &Command, meta: Value) -> Value {
    json!({
        "t": "spawn",
        "kind": kind,
        "program": cmd.get_program().to_string_lossy(),
        "args": cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "cwd": cmd.get_current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default(),
        "env": effective_env(cmd),
        "meta": meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_the_hosts_exit_line_from_a_session_line() {
        let exit = json!({ "type": "hq_exit", "code": 0, "stderr": ["x \"type\":\"hq_exit\"}"] }).to_string();
        assert!(is_exit(&exit), "{exit}");
        assert!(!is_exit(r#"{"type":"assistant","text":"{\"code\":1,\"type\":\"hq_exit\"}"}"#));
        assert!(last_is_exit(&format!(r#"{{"seq":3,"in":false,"line":{exit}}}"#)));
    }

    #[test]
    fn the_environment_follows_the_commands_changes() {
        let mut cmd = Command::new("x");
        cmd.env("HQ_TEST_SET", "1").env_remove("PATH");
        let env = effective_env(&cmd);
        assert_eq!(env["HQ_TEST_SET"], "1");
        assert!(!env.keys().any(|k| k.eq_ignore_ascii_case("PATH")));
    }
}

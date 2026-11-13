//! The session host: a background copy of HQ that owns the agent processes (Claude Code and
//! Hermes) so they outlive HQ's window. Installing an update closes HQ, and closing HQ used to end
//! everything it had started: turns, background shells, dev servers. The host is not HQ's child (it
//! breaks out of HQ's job and process group), runs from its own copy of the executable under
//! `~/.hq/bin` (the installer stops and replaces `hq.exe`, by name), and HQ reconnects to it when it
//! starts again (hostlink.rs).
//!
//! One host per build: a new build starts its own for new sessions and reattaches to the hosts of
//! older builds, which exit once their last session has ended. HQ talks to a host over 127.0.0.1
//! with a token from the host's discovery file, which only this user can read (the scheme mcp.rs
//! uses). The protocol is newline-delimited JSON. `PROTO` versions it: a build must go on speaking
//! every version an older host still running may speak.
//!
//! Each session keeps a spool of what went in and out of it, so a reconnecting HQ redraws the turn
//! in flight as the old window had it. The stream deltas of a finished turn are dropped from it.
//!
//! Lifetime: sessions wait `LOST_GRACE` for HQ when it vanishes without a word (a crash), and as
//! long as HQ asks when it closes to install an update; then they are ended. A quit ends them at
//! once. On Windows every session's process tree is in a kill-on-close job, so if the host itself
//! dies its sessions go with it. On Unix each is a process group, which the host ends on a kill, a
//! grace running out or its own exit, but which a crash of the host itself leaves running.
//!
//! While HQ is closed, sessions keep the old MCP URL (mcp.rs rebinds that port when HQ is back).
//! Another local process could take the port in that gap and receive their tool calls; accepted,
//! as the gap is an update's length and the calls carry no secret beyond the session's own token.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::session::{child_flags, is_forwardable, kill_group, spawn_stderr_reader, spawn_stdin_writer, wait_for_exit};

/// The protocol this build speaks. Bump only with a compatible fallback in hostlink.rs.
pub const PROTO: u64 = 1;
/// What a session's spool may hold; the oldest lines go first, deltas before whole messages.
const SPOOL_MAX_BYTES: usize = 64 << 20;
/// How long sessions wait for HQ after it went without saying why (a crash, or a kill).
const LOST_GRACE: Duration = Duration::from_secs(15 * 60);
/// The longest an update may ask sessions to wait.
const MAX_GRACE: Duration = Duration::from_secs(2 * 60 * 60);
/// How long a host started for HQ waits for it to connect.
const FIRST_CONNECT: Duration = Duration::from_secs(60);
/// A host with no sessions and nobody connected exits after this long.
const IDLE_EXIT: Duration = Duration::from_secs(20);
/// What a client has to send its hello in.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections still proving themselves: past this many, new ones are turned away until one
/// finishes (at most HELLO_TIMEOUT later), so nobody unauthenticated can pile up threads.
const MAX_UNPROVEN: usize = 16;
static UNPROVEN: AtomicUsize = AtomicUsize::new(0);
const LOG_MAX_BYTES: u64 = 1 << 20;
/// How much of each stderr line of a failed session goes into the log.
const STDERR_LOG_CHARS: usize = 500;

/// `~/.hq`, where HQ keeps what the host and the MCP bridge share with it.
pub(crate) fn hq_dir() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".hq"))
}

/// Where hosts of this profile put their discovery files and log.
pub(crate) fn hosts_dir(profile: Option<&str>) -> Option<PathBuf> {
    let name = match profile {
        Some(p) => format!("hosts-{p}"),
        None => "hosts".into(),
    };
    Some(hq_dir()?.join(name))
}

/// What a host's discovery file holds.
#[derive(Deserialize, Clone)]
pub(crate) struct Discovery {
    pub proto: u64,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    #[serde(default)]
    pub exe: String,
}

/// Milliseconds since the epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What went over the wire to the client. `Out` is a session's line, counted as delivered once
/// written, which is how a later reconnect tells what the window had already seen.
enum Wire {
    Text(String),
    Out { id: u64, seq: u64, text: String },
}

struct Entry {
    seq: u64,
    /// When the host had it (ms): what a reattach draws from the transcript instead ends before this.
    at: u64,
    inbound: bool,
    kind: Kind,
    line: String,
}

/// What a line is to the spool.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// A stream delta: dropped once its turn ends, since the whole message follows it.
    Stream,
    TurnEnd,
    /// The session's init and its latest background task list: state a redraw needs even when
    /// the turn they came in is long over (a dev server started three turns ago still runs).
    Init,
    BgTasks,
    /// A prompt HQ sent, or the agent's own message: what a turn is made of.
    Prompt,
    Message,
    Other,
}

impl Kind {
    /// Whether the line belongs to a turn, so one after the last turn's end means a turn is open.
    /// Not state lines (a background shell ending while idle) or answers to HQ's control requests.
    fn in_turn(self) -> bool {
        matches!(self, Kind::Prompt | Kind::Message | Kind::Stream)
    }
}

#[derive(Default)]
struct Spool {
    /// Whole lines, oldest first.
    entries: VecDeque<Entry>,
    /// The open turn's stream deltas, kept apart so a turn's end (or a full spool) drops them at once.
    deltas: VecDeque<Entry>,
    bytes: usize,
    delta_bytes: usize,
    /// The latest init and background task lines, should the spool have shed them as old.
    state: [Option<Entry>; 2],
    seq: u64,
    /// The seq of the last turn's end: lines up to it are whole in the transcript.
    turn_end_seq: u64,
    /// The last line shed for room was inside a turn (not its end): the oldest kept turn has lost its start.
    shed_mid_turn: bool,
    /// The connection this session's lines go to: (client generation, its queue).
    sink: Option<(u64, mpsc::Sender<Wire>)>,
    /// The highest seq written to a client.
    delivered: u64,
    /// The seq of the session's exit line, once it has ended.
    exit_seq: Option<u64>,
}

fn state_slot(kind: Kind) -> Option<usize> {
    match kind {
        Kind::Init => Some(0),
        Kind::BgTasks => Some(1),
        _ => None,
    }
}

impl Spool {
    fn push(&mut self, id: u64, inbound: bool, kind: Kind, line: String) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        if !inbound {
            if let Some((_, tx)) = &self.sink {
                let text = format!(r#"{{"t":"out","id":{id},"seq":{seq},"line":{line}}}"#);
                if tx.send(Wire::Out { id, seq, text }).is_err() {
                    self.sink = None;
                }
            }
        }
        let entry = Entry { seq, at: now_ms(), inbound, kind, line };
        if kind == Kind::Stream {
            self.delta_bytes += entry.line.len();
            self.deltas.push_back(entry);
        } else {
            if kind == Kind::TurnEnd {
                self.turn_end_seq = seq;
            }
            self.bytes += entry.line.len();
            self.entries.push_back(entry);
        }
        if self.bytes + self.delta_bytes > SPOOL_MAX_BYTES {
            self.drop_stream(); // deltas go first: the whole messages they become follow them
        }
        while self.bytes > SPOOL_MAX_BYTES && self.entries.len() > 1 {
            let Some(e) = self.entries.pop_front() else { break };
            self.bytes -= e.line.len();
            self.shed_mid_turn = e.kind != Kind::TurnEnd;
            // The latest state line is kept aside, since a redraw needed it however old it is.
            if let Some(slot) = state_slot(e.kind) {
                if !self.entries.iter().any(|x| x.kind == e.kind) {
                    self.state[slot] = Some(e);
                }
            }
        }
        seq
    }

    fn drop_stream(&mut self) {
        self.deltas.clear();
        self.delta_bytes = 0;
    }

    /// Where a redraw's replay starts. Lines before it are whole in the transcript, which draws
    /// them with its own paging. It is the start of the open turn, or of an earlier one the client
    /// has not seen all of (one that ended while HQ was away is news, so it is replayed too); now
    /// when there is neither.
    fn replay_from(&self) -> u64 {
        let all = || self.entries.iter().chain(self.deltas.iter());
        // The turn that holds a line: from the line after the last turn end before it.
        let turn_start = |seq: u64| {
            let after = self.entries.iter().rposition(|e| e.kind == Kind::TurnEnd && e.seq < seq).map_or(0, |i| i + 1);
            self.entries.get(after).map(|e| e.at)
        };
        let open = all().filter(|e| e.seq > self.turn_end_seq && e.kind.in_turn()).map(|e| e.at).min();
        let unseen = all().filter(|e| !e.inbound && e.seq > self.delivered).map(|e| e.seq).min().and_then(turn_start);
        match (open, unseen) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => now_ms(),
        }
    }

    /// Whether a replay from `from` lacks the start of its first turn: lines of it were shed for
    /// room, and no turn's end kept from before `from` marks a whole turn boundary.
    fn truncated_for(&self, from: u64) -> bool {
        self.shed_mid_turn && !self.entries.iter().any(|e| e.kind == Kind::TurnEnd && e.at < from)
    }

    /// The lines a redraw from `from` needs: the latest state lines from before it, then all after it.
    fn replay(&self, from: u64) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = Vec::new();
        for kind in [Kind::Init, Kind::BgTasks] {
            let latest = self.entries.iter().rev().find(|e| e.kind == kind).or(state_slot(kind).and_then(|s| self.state[s].as_ref()));
            if let Some(e) = latest.filter(|e| e.at < from) {
                out.push(e);
            }
        }
        out.extend(self.entries.iter().chain(self.deltas.iter()).filter(|e| e.at >= from));
        out.sort_by_key(|e| e.seq);
        out
    }
}

struct HostSession {
    id: u64,
    kind: String,
    meta: Value,
    started_ms: u64,
    input: Mutex<Option<mpsc::Sender<String>>>,
    child: Arc<Mutex<Child>>,
    #[cfg(windows)]
    _job: Option<crate::session::job::Job>,
    #[cfg(unix)]
    pgid: u32,
    spool: Mutex<Spool>,
}

impl HostSession {
    /// Ends the agent and everything it started, which also ends its output, so its exit follows.
    fn kill(&self) {
        self.input.lock().unwrap().take();
        let mut child = self.child.lock().unwrap();
        // Once the leader is reaped its group id is free for reuse; its exit already ended the group.
        #[cfg(unix)]
        if !matches!(child.try_wait(), Ok(Some(_))) {
            kill_group(self.pgid);
        }
        #[cfg(windows)]
        if let Some(job) = &self._job {
            job.terminate();
        }
        let _ = child.kill();
    }

    fn exited(&self) -> bool {
        self.spool.lock().unwrap().exit_seq.is_some()
    }
}

struct Client {
    gen: u64,
    tx: Option<mpsc::Sender<Wire>>,
    stream: Option<TcpStream>,
    /// When the last client went, or the host started.
    gone_since: Instant,
    /// How long sessions wait for a client from `gone_since`.
    grace: Duration,
    /// What the connected client asked for when it next goes (an update), if anything.
    asked_grace: Option<Duration>,
}

struct Host {
    token: String,
    sessions: Mutex<HashMap<u64, Arc<HostSession>>>,
    next_id: AtomicU64,
    client: Mutex<Client>,
    retired: AtomicBool,
    /// Whether HQ has connected yet: until then the host waits `FIRST_CONNECT`, not `IDLE_EXIT`.
    ever_connected: AtomicBool,
    discovery: PathBuf,
}

static LOG: Mutex<Option<File>> = Mutex::new(None);
static HOST: OnceLock<Host> = OnceLock::new();

fn log(msg: impl AsRef<str>) {
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{} [{}] {}", now_ms(), std::process::id(), msg.as_ref());
    }
}

/// Runs the host until it has nothing left to do. `hq --session-host [--profile <name>]`.
pub fn run(profile: Option<String>) -> i32 {
    #[cfg(unix)]
    unsafe {
        // A write to a closed socket must not end the host. (A terminal closing cannot: HQ starts it
        // in a session of its own. SIGHUP stays default, since the agents' shells inherit it.)
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let Some(dir) = hosts_dir(profile.as_deref()) else { return 2 };
    if fs::create_dir_all(&dir).is_err() {
        return 2;
    }
    let log_path = dir.join("host.log");
    if fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = fs::rename(&log_path, dir.join("host.old.log"));
    }
    *LOG.lock().unwrap() = OpenOptions::new().create(true).append(true).open(&log_path).ok();

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            log(format!("cannot listen: {e}"));
            return 3;
        }
    };
    let Ok(port) = listener.local_addr().map(|a| a.port()) else { return 3 };
    let token = crate::mcp::new_token();
    let discovery = dir.join(format!("{}.json", std::process::id()));
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let body = json!({
        "proto": PROTO, "port": port, "token": token, "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"), "exe": exe, "started": now_ms(),
    });
    if let Err(e) = crate::mcp::write_private(&discovery, body.to_string().as_bytes()) {
        log(format!("cannot write {}: {e}", discovery.display()));
        return 3;
    }
    let host = Host {
        token,
        sessions: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
        client: Mutex::new(Client { gen: 0, tx: None, stream: None, gone_since: Instant::now(), grace: FIRST_CONNECT, asked_grace: None }),
        retired: AtomicBool::new(false),
        ever_connected: AtomicBool::new(false),
        discovery,
    };
    let _ = HOST.set(host);
    log(format!("started v{} on port {port} from {exe}", env!("CARGO_PKG_VERSION")));
    std::thread::spawn(watchdog);
    for stream in listener.incoming().flatten() {
        if UNPROVEN.fetch_add(1, Ordering::AcqRel) >= MAX_UNPROVEN {
            UNPROVEN.fetch_sub(1, Ordering::AcqRel);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        std::thread::spawn(move || serve(stream));
    }
    0
}

fn host() -> &'static Host {
    HOST.get().expect("host not started")
}

/// Ends the host: its discovery file goes, and any session left is ended with it.
fn exit(why: &str) -> ! {
    let h = host();
    let sessions: Vec<_> = h.sessions.lock().unwrap().values().cloned().collect();
    for s in &sessions {
        s.kill();
    }
    let _ = fs::remove_file(&h.discovery);
    log(format!("exiting: {why} ({} sessions ended)", sessions.len()));
    std::process::exit(0);
}

fn watchdog() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let h = host();
        let (connected, gone_for, grace) = {
            let c = h.client.lock().unwrap();
            (c.tx.is_some(), c.gone_since.elapsed(), c.grace)
        };
        let empty = h.sessions.lock().unwrap().is_empty();
        if !connected && gone_for > grace && !empty {
            exit(&format!("HQ did not come back within {}s", grace.as_secs()));
        }
        let idle = if h.ever_connected.load(Ordering::Relaxed) { IDLE_EXIT } else { FIRST_CONNECT };
        if empty && (h.retired.load(Ordering::Relaxed) || (!connected && gone_for > idle)) {
            exit("no sessions left");
        }
    }
}

/// Compares without stopping at the first difference, so timing says nothing about the token.
pub(crate) fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Shows that `who` ("host" or "client") knows the token, for the other side's `nonce`: the
/// handshake proves both ends, and the token itself never crosses the socket. Anyone on the
/// machine can connect, or take the port of a host that died, so neither side trusts the other
/// until it has proved itself.
pub(crate) fn proof(token: &str, who: &str, nonce: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new().chain_update(token).chain_update(b"\0").chain_update(who).chain_update(b"\0").chain_update(nonce).finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize)]
struct Handshake {
    t: String,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    proof: String,
}

/// Reads one handshake line: bounded, since it comes before anyone has proved anything.
fn read_handshake(reader: &mut BufReader<TcpStream>) -> Option<Handshake> {
    let mut line = String::new();
    reader.by_ref().take(4096).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

/// The host's half of the handshake: the client sends a nonce, the host proves itself on it and
/// sends its own, and the client proves itself on that.
fn authenticate(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, token: &str) -> bool {
    let Some(hello) = read_handshake(reader).filter(|m| m.t == "hello" && !m.nonce.is_empty() && m.nonce.len() <= 256) else { return false };
    let mine = crate::mcp::new_token();
    let challenge = json!({ "t": "challenge", "nonce": mine, "proof": proof(token, "host", &hello.nonce) });
    if writeln!(stream, "{challenge}").is_err() {
        return false;
    }
    read_handshake(reader).is_some_and(|m| m.t == "auth" && same_token(&m.proof, &proof(token, "client", &mine)))
}

fn serve(stream: TcpStream) {
    let h = host();
    let _ = stream.set_nodelay(true);
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    let Ok(mut write_half) = stream.try_clone() else {
        UNPROVEN.fetch_sub(1, Ordering::AcqRel);
        return;
    };
    // The read timeout is per read: this ends a handshake trickled out a byte at a time.
    let proven = Arc::new(AtomicBool::new(false));
    if let Ok(s) = stream.try_clone() {
        let proven = proven.clone();
        std::thread::spawn(move || {
            std::thread::sleep(HELLO_TIMEOUT);
            if !proven.load(Ordering::Acquire) {
                let _ = s.shutdown(Shutdown::Both);
            }
        });
    }
    let ok = authenticate(&mut write_half, &mut reader, &h.token);
    proven.store(ok, Ordering::Release);
    UNPROVEN.fetch_sub(1, Ordering::AcqRel);
    if !ok {
        log("refused a connection that did not prove the token");
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let _ = stream.set_read_timeout(None);

    let (tx, rx) = mpsc::channel::<Wire>();
    let gen = {
        let mut c = h.client.lock().unwrap();
        // One client at a time: a new HQ replaces a stale connection the old one left behind.
        if let Some(old) = c.stream.take() {
            let _ = old.shutdown(Shutdown::Both);
        }
        c.gen += 1;
        c.tx = Some(tx.clone());
        c.stream = stream.try_clone().ok();
        c.asked_grace = None;
        c.gen
    };
    h.ever_connected.store(true, Ordering::Relaxed);
    // Lines of sessions the new client has not attached to yet have nowhere to go.
    for s in h.sessions.lock().unwrap().values() {
        s.spool.lock().unwrap().sink = None;
    }
    let Ok(write_half) = stream.try_clone() else { return };
    std::thread::spawn(move || writer(write_half, rx));
    log(format!("client {gen} connected"));
    let hello = json!({
        "t": "hello", "proto": PROTO, "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id(),
        "exe": std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
    });
    let _ = tx.send(Wire::Text(hello.to_string()));

    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
                handle(&msg, gen, &tx);
            }
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
    // Lines written into the closed socket would count as seen; they wait for the next attach.
    for s in h.sessions.lock().unwrap().values() {
        let mut sp = s.spool.lock().unwrap();
        if sp.sink.as_ref().is_some_and(|(g, _)| *g == gen) {
            sp.sink = None;
        }
    }
    let mut c = h.client.lock().unwrap();
    if c.gen == gen {
        c.tx = None;
        c.stream = None;
        c.gone_since = Instant::now();
        c.grace = c.asked_grace.take().unwrap_or(LOST_GRACE);
        log(format!("client {gen} went; sessions wait {}s", c.grace.as_secs()));
    }
}

fn writer(mut stream: TcpStream, rx: mpsc::Receiver<Wire>) {
    for w in rx {
        let (text, delivered) = match w {
            Wire::Text(t) => (t, None),
            Wire::Out { id, seq, text } => (text, Some((id, seq))),
        };
        if stream.write_all(text.as_bytes()).and_then(|_| stream.write_all(b"\n")).is_err() {
            break;
        }
        if let Some((id, seq)) = delivered {
            mark_delivered(id, seq);
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Notes what the client has; a session whose exit it has is done with.
fn mark_delivered(id: u64, seq: u64) {
    let h = host();
    let Some(s) = h.sessions.lock().unwrap().get(&id).cloned() else { return };
    let done = {
        let mut sp = s.spool.lock().unwrap();
        sp.delivered = sp.delivered.max(seq);
        sp.exit_seq.is_some_and(|e| sp.delivered >= e)
    };
    if done {
        h.sessions.lock().unwrap().remove(&id);
        log(format!("session {id} delivered its exit and is gone"));
    }
}

fn reply(tx: &mpsc::Sender<Wire>, req: &Value, body: Value) {
    let mut body = body;
    body["t"] = json!("reply");
    body["req"] = req.clone();
    let _ = tx.send(Wire::Text(body.to_string()));
}

fn get(id: &Value) -> Option<Arc<HostSession>> {
    let id = id.as_u64()?;
    host().sessions.lock().unwrap().get(&id).cloned()
}

fn handle(msg: &Value, gen: u64, tx: &mpsc::Sender<Wire>) {
    let h = host();
    let req = &msg["req"];
    match msg["t"].as_str().unwrap_or("") {
        "send" => {
            let (Some(s), Some(line)) = (get(&msg["id"]), msg["line"].as_str()) else { return };
            let input = s.input.lock().unwrap();
            let Some(input) = input.as_ref() else { return };
            if let Some((kind, kept)) = inbound_record(line) {
                s.spool.lock().unwrap().push(s.id, true, kind, kept);
            }
            let _ = input.send(line.to_owned());
        }
        "spawn" => match spawn(msg, gen, tx) {
            // `spawn` replied itself, ahead of the session's first line.
            Ok(()) => {}
            Err(e) => reply(tx, req, json!({ "err": e })),
        },
        "kill" => {
            if let Some(s) = get(&msg["id"]) {
                log(format!("session {} killed by HQ", s.id));
                s.kill();
                // Ended already, its exit lost with a broken connection: nothing will remove it but this.
                if s.exited() {
                    h.sessions.lock().unwrap().remove(&s.id);
                }
            }
        }
        "list" => {
            let list: Vec<Value> = h
                .sessions
                .lock()
                .unwrap()
                .values()
                .map(|s| {
                    let from = s.spool.lock().unwrap().replay_from();
                    json!({ "id": s.id, "kind": s.kind, "meta": s.meta, "startedMs": s.started_ms, "replayFromMs": from, "exited": s.exited() })
                })
                .collect();
            reply(tx, req, json!({ "ok": true, "sessions": list }));
        }
        "attach" => {
            let Some(s) = get(&msg["id"]) else {
                return reply(tx, req, json!({ "err": "no such session" }));
            };
            // Under the spool lock, so no live line can come between the replay and the reply.
            let mut sp = s.spool.lock().unwrap();
            let seen = sp.delivered;
            let from = msg["fromMs"].as_u64().unwrap_or(0);
            for e in sp.replay(from) {
                // A state line from before the replayed span says how things stand, not that a turn began.
                let state = u8::from(e.at < from);
                let text = format!(r#"{{"t":"out","id":{},"seq":{},"r":1,"in":{},"s":{state},"line":{}}}"#, s.id, e.seq, e.inbound, e.line);
                let _ = tx.send(Wire::Out { id: s.id, seq: e.seq, text });
            }
            sp.sink = Some((gen, tx.clone()));
            reply(tx, req, json!({ "ok": true, "attached": s.id, "seen": seen, "truncated": sp.truncated_for(from), "startedMs": s.started_ms }));
        }
        "detach" => {
            let secs = msg["graceSecs"].as_u64().unwrap_or(0);
            let grace = Duration::from_secs(secs).min(MAX_GRACE);
            h.client.lock().unwrap().asked_grace = Some(grace);
            log(format!("HQ is closing for an update; sessions wait {}s", grace.as_secs()));
            reply(tx, req, json!({ "ok": true }));
        }
        "kill_all" => {
            let sessions: Vec<_> = h.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
            for s in &sessions {
                s.kill();
            }
            log(format!("HQ quit: {} sessions ended", sessions.len()));
            reply(tx, req, json!({ "ok": true }));
        }
        "retire" => {
            h.retired.store(true, Ordering::Relaxed);
            reply(tx, req, json!({ "ok": true }));
        }
        "ping" => reply(tx, req, json!({ "ok": true })),
        _ => reply(tx, req, json!({ "err": "unknown message" })),
    }
}

/// What of a line HQ sent the session goes in the spool: the prompts (images blanked) and the
/// answers to the session's requests, which a redraw needs; not HQ's own control requests.
fn inbound_record(line: &str) -> Option<(Kind, String)> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    let head: Head = serde_json::from_str(line).ok()?;
    let kind = match head.kind.as_deref()? {
        "user" => Kind::Prompt,
        "control_response" => Kind::Other,
        _ => return None,
    };
    Some((kind, crate::store::blank_base64(line).into_owned()))
}

/// What a session's line is to the spool.
fn classify(line: &str) -> Kind {
    #[derive(Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        kind: Option<String>,
        subtype: Option<String>,
    }
    let Ok(head) = serde_json::from_str::<Head>(line) else { return Kind::Other };
    match (head.kind.as_deref(), head.subtype.as_deref()) {
        (Some("stream_event"), _) => Kind::Stream,
        (Some("result"), _) => Kind::TurnEnd,
        (Some("system"), Some("init")) => Kind::Init,
        (Some("system"), Some("background_tasks_changed")) => Kind::BgTasks,
        (Some("assistant" | "user"), _) => Kind::Message,
        _ => Kind::Other,
    }
}

fn spawn(msg: &Value, gen: u64, tx: &mpsc::Sender<Wire>) -> Result<(), String> {
    let h = host();
    let kind = msg["kind"].as_str().unwrap_or("claude").to_owned();
    let program = msg["program"].as_str().ok_or("no program")?;
    let cwd = msg["cwd"].as_str().ok_or("no cwd")?;
    let mut cmd = Command::new(program);
    if let Some(args) = msg["args"].as_array() {
        cmd.args(args.iter().filter_map(Value::as_str));
    }
    // HQ hands over the whole environment the child is to have; the host's own is not used.
    cmd.env_clear();
    if let Some(env) = msg["env"].as_object() {
        cmd.envs(env.iter().filter_map(|(k, v)| v.as_str().map(|v| (k, v))));
    }
    cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    child_flags(&mut cmd);
    // Read before the child starts, so a bad message leaves nothing running.
    #[cfg(feature = "hermes")]
    let hermes_opts: Option<crate::session::SpawnOptions> = match kind.as_str() {
        "hermes" => Some(serde_json::from_value(msg["hermes"]["opts"].clone()).map_err(|e| e.to_string())?),
        _ => None,
    };
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        kill_group(child.id());
        let _ = child.kill();
        return Err("pipes unavailable".into());
    };
    #[cfg(windows)]
    let Some(job) = crate::session::job::Job::new().filter(|j| j.assign(&child)) else {
        let _ = child.kill();
        return Err("could not put the session in a job object, so closing it could not end what it starts".into());
    };
    #[cfg(unix)]
    let pgid = child.id();

    // The agent's ends: claude's own pipes, or the ends of the translator that speaks for Hermes.
    let (input, out): (mpsc::Sender<String>, Box<dyn Read + Send>) = match kind.as_str() {
        #[cfg(feature = "hermes")]
        "hermes" => {
            let hermes = &msg["hermes"];
            let opts = hermes_opts.unwrap_or_else(|| unreachable!("parsed above for a hermes spawn"));
            let mirror = PathBuf::from(hermes["mirror"].as_str().unwrap_or_default());
            match crate::hermes::translate(stdin, stdout, mirror, opts, hermes["mcpServers"].clone()) {
                Ok((input, lines)) => (input, Box::new(lines)),
                Err(e) => {
                    kill_group(child.id());
                    let _ = child.kill();
                    return Err(e);
                }
            }
        }
        _ => (spawn_stdin_writer(stdin), Box::new(stdout)),
    };

    let id = h.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let child = Arc::new(Mutex::new(child));
    let session = Arc::new(HostSession {
        id,
        kind: kind.clone(),
        meta: msg["meta"].clone(),
        started_ms: now_ms(),
        input: Mutex::new(Some(input)),
        child: child.clone(),
        #[cfg(windows)]
        _job: Some(job),
        #[cfg(unix)]
        pgid,
        spool: Mutex::new(Spool { sink: Some((gen, tx.clone())), ..Spool::default() }),
    });
    h.sessions.lock().unwrap().insert(id, session.clone());
    // A new client came meanwhile (HQ reconnected after a broken connection, and stopped all it
    // had, or restarted): this spawn's HQ is gone or has run the session itself, so it must not
    // run here too. Checked after the insert, so a kill_all from the new client cannot miss it.
    if h.client.lock().unwrap().gen != gen {
        session.kill();
        h.sessions.lock().unwrap().remove(&id);
        return Err("superseded by a newer connection".into());
    }
    // The reply goes first, so HQ knows the session before its first line arrives.
    reply(tx, &msg["req"], json!({ "ok": true, "id": id, "startedMs": session.started_ms }));
    log(format!("session {id} ({kind}) started in {cwd}"));

    let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
    spawn_stderr_reader(stderr, tail.clone());
    std::thread::spawn(move || {
        let mut reader = BufReader::new(out);
        let mut buf = Vec::with_capacity(64 * 1024);
        // Whether the last turn ended in an error: a session HQ's Hermes bridge gives up on (one
        // that never opened, say) ends with code 0, and its stderr is still the only trace of why.
        let mut ended_on_error = false;
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim();
                    if !is_forwardable(line) {
                        continue;
                    }
                    let kind = classify(line);
                    let mut sp = session.spool.lock().unwrap();
                    if kind == Kind::TurnEnd {
                        ended_on_error = serde_json::from_str::<Value>(line).is_ok_and(|v| v["subtype"] == "error");
                        sp.drop_stream();
                    }
                    sp.push(id, false, kind, line.to_owned());
                }
            }
        }
        let code = wait_for_exit(&child);
        let stderr: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
        let exit = json!({ "type": "hq_exit", "code": code, "stderr": stderr }).to_string();
        session.input.lock().unwrap().take();
        let mut sp = session.spool.lock().unwrap();
        let seq = sp.push(id, false, Kind::Other, exit);
        sp.exit_seq = Some(seq);
        // Delivered already, or nowhere to go: a session no cell holds (killed unclaimed, or ended
        // while HQ was away) is forgotten, and its cell, if any, simply restores from the transcript.
        let done = sp.delivered >= seq || sp.sink.is_none();
        drop(sp);
        log(format!("session {id} ended with code {code:?}"));
        // The cell may be gone (a session HQ killed reports to no one), so a failed session's last
        // stderr lines are kept here too: they are the only trace of why an agent stalled. Like the
        // rest of this log they stay on this machine; each is cut short to bound what lands here.
        if code != Some(0) || ended_on_error {
            for line in &stderr {
                let line: String = line.chars().take(STDERR_LOG_CHARS).collect();
                log(format!("session {id} stderr: {line}"));
            }
        }
        if done {
            host().sessions.lock().unwrap().remove(&id);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_end_drops_its_deltas_but_keeps_the_messages() {
        let mut sp = Spool::default();
        sp.push(1, true, Kind::Other, r#"{"type":"user"}"#.into());
        sp.push(1, false, Kind::Stream, r#"{"type":"stream_event"}"#.into());
        sp.push(1, false, Kind::Other, r#"{"type":"assistant"}"#.into());
        sp.drop_stream();
        sp.push(1, false, Kind::TurnEnd, r#"{"type":"result"}"#.into());
        let kept: Vec<_> = sp.entries.iter().map(|e| (e.seq, e.line.as_str())).collect();
        assert_eq!(kept, [(1, r#"{"type":"user"}"#), (3, r#"{"type":"assistant"}"#), (4, r#"{"type":"result"}"#)]);
        assert_eq!(sp.bytes, kept.iter().map(|(_, l)| l.len()).sum::<usize>());
    }

    #[test]
    fn only_outbound_lines_reach_the_client_and_a_closed_client_is_forgotten() {
        let (tx, rx) = mpsc::channel();
        let mut sp = Spool { sink: Some((1, tx)), ..Spool::default() };
        sp.push(7, true, Kind::Other, r#"{"type":"user"}"#.into());
        sp.push(7, false, Kind::Other, r#"{"type":"assistant"}"#.into());
        match rx.try_recv().unwrap() {
            Wire::Out { id, seq, text } => {
                assert_eq!((id, seq), (7, 2));
                assert_eq!(text, r#"{"t":"out","id":7,"seq":2,"line":{"type":"assistant"}}"#);
            }
            Wire::Text(_) => panic!("expected a session line"),
        }
        assert!(rx.try_recv().is_err());
        drop(rx);
        sp.push(7, false, Kind::Other, "{}".into());
        assert!(sp.sink.is_none());
    }

    #[test]
    fn an_overfull_spool_sheds_deltas_first_then_the_oldest() {
        let mut sp = Spool::default();
        let big = "x".repeat(SPOOL_MAX_BYTES / 2);
        sp.push(1, false, Kind::Other, format!(r#"{{"a":"{big}"}}"#));
        sp.push(1, false, Kind::Stream, format!(r#"{{"d":"{big}"}}"#));
        assert!(!sp.truncated_for(0), "deltas go before any message");
        assert_eq!((sp.entries.len(), sp.deltas.len()), (1, 0));
        sp.push(1, false, Kind::Other, format!(r#"{{"b":"{big}"}}"#));
        sp.push(1, false, Kind::Other, format!(r#"{{"c":"{big}"}}"#));
        assert!(sp.truncated_for(sp.replay_from()));
        assert!(sp.bytes <= SPOOL_MAX_BYTES);
        assert_eq!(sp.entries.back().unwrap().seq, 4);
    }

    #[test]
    fn finished_turns_shed_for_room_are_not_truncation_and_state_survives() {
        let mut sp = Spool::default();
        let big = "x".repeat(SPOOL_MAX_BYTES / 2);
        sp.push(1, false, Kind::Init, "init".into());
        sp.push(1, false, Kind::Other, format!(r#"{{"a":"{big}"}}"#));
        sp.push(1, false, Kind::TurnEnd, "r1".into());
        sp.push(1, false, Kind::Other, format!(r#"{{"b":"{big}"}}"#));
        sp.push(1, false, Kind::Other, format!(r#"{{"c":"{big}"}}"#));
        assert!(!sp.entries.iter().any(|e| e.kind == Kind::Init), "init was shed from the entries");
        assert!(sp.state[0].is_some(), "but kept aside");
        let from = sp.replay_from();
        assert!(sp.truncated_for(from), "b, of the open turn, was shed");
        assert_eq!(sp.replay(from).first().map(|e| e.line.as_str()), Some("init"));
    }

    #[test]
    fn a_turn_that_ended_unseen_is_replayed() {
        let mut sp = Spool::default();
        sp.push(1, true, Kind::Prompt, "u1".into());
        sp.push(1, false, Kind::Message, "a1".into());
        sp.delivered = 2; // the window saw a1, then HQ went
        sp.push(1, false, Kind::TurnEnd, "r1".into());
        sp.push(1, true, Kind::Prompt, "u2".into());
        sp.push(1, false, Kind::TurnEnd, "r2".into());
        for (i, e) in sp.entries.iter_mut().enumerate() {
            e.at = 100 + i as u64 * 10;
        }
        assert_eq!(sp.replay_from(), 100, "from the turn holding the first unseen line");
        sp.delivered = 5;
        assert!(sp.replay_from() > 1_000, "all seen and nothing open: from now");
    }

    #[test]
    fn proofs_are_bound_to_the_side_the_token_and_the_nonce() {
        let p = proof("tok", "host", "n1");
        assert_eq!(p.len(), 64);
        assert_eq!(p, proof("tok", "host", "n1"));
        assert_ne!(p, proof("tok", "client", "n1"));
        assert_ne!(p, proof("tok", "host", "n2"));
        assert_ne!(p, proof("tok2", "host", "n1"));
    }

    #[test]
    fn keeps_prompts_and_answers_but_not_control_requests() {
        assert_eq!(inbound_record(r#"{"type":"user","message":{}}"#).map(|r| r.0), Some(Kind::Prompt));
        assert_eq!(inbound_record(r#"{"type":"control_response","response":{}}"#).map(|r| r.0), Some(Kind::Other));
        assert!(inbound_record(r#"{"type":"control_request","request":{"subtype":"interrupt"}}"#).is_none());
        assert!(inbound_record("not json").is_none());
        assert_eq!(classify(r#"{"type":"stream_event","event":{}}"#), Kind::Stream);
        assert_eq!(classify(r#"{"type":"result","subtype":"success"}"#), Kind::TurnEnd);
        assert_eq!(classify(r#"{"type":"system","subtype":"init"}"#), Kind::Init);
        assert_eq!(classify(r#"{"type":"system","subtype":"background_tasks_changed"}"#), Kind::BgTasks);
        assert_eq!(classify(r#"{"type":"assistant"}"#), Kind::Message);
        assert_eq!(classify(r#"{"type":"control_response"}"#), Kind::Other);
    }

    #[test]
    fn a_turn_after_an_overflowed_one_is_not_truncated() {
        let mut sp = Spool::default();
        let big = "x".repeat(SPOOL_MAX_BYTES / 2);
        sp.push(1, true, Kind::Prompt, "u1".into());
        sp.push(1, false, Kind::Message, format!(r#"{{"a":"{big}"}}"#));
        sp.push(1, false, Kind::Message, format!(r#"{{"b":"{big}"}}"#));
        sp.push(1, false, Kind::Message, format!(r#"{{"c":"{big}"}}"#));
        assert!(sp.truncated_for(sp.replay_from()), "the overflowing turn lost its start");
        sp.push(1, false, Kind::TurnEnd, "r1".into());
        sp.push(1, true, Kind::Prompt, "u2".into());
        sp.delivered = sp.seq;
        for e in sp.entries.iter_mut() {
            e.at = if e.seq <= 5 { 100 } else { 200 };
        }
        assert_eq!(sp.replay_from(), 200);
        assert!(!sp.truncated_for(200), "the next turn is whole");
    }

    #[test]
    fn state_and_control_lines_after_a_turn_do_not_open_one() {
        let mut sp = Spool::default();
        sp.push(1, true, Kind::Prompt, "u1".into());
        sp.push(1, false, Kind::TurnEnd, "r1".into());
        sp.push(1, false, Kind::BgTasks, "bg".into());
        sp.push(1, false, Kind::Other, "control_response".into());
        sp.delivered = sp.seq;
        for e in sp.entries.iter_mut() {
            e.at = 100;
        }
        assert!(sp.replay_from() > 1_000, "idle: nothing open");
        sp.push(1, true, Kind::Prompt, "u2".into());
        sp.entries.back_mut().unwrap().at = 200;
        assert_eq!(sp.replay_from(), 200);
    }

    #[test]
    fn a_redraw_replays_the_open_turn_and_the_state_before_it() {
        let mut sp = Spool::default();
        let mut at = 100;
        let mut add = |sp: &mut Spool, inbound, kind, line: &str| {
            sp.push(1, inbound, kind, line.into());
            sp.entries.back_mut().unwrap().at = at;
            at += 10;
        };
        add(&mut sp, false, Kind::Init, "init");
        add(&mut sp, true, Kind::Prompt, "u1");
        add(&mut sp, false, Kind::BgTasks, "bg-old");
        add(&mut sp, false, Kind::BgTasks, "bg");
        add(&mut sp, false, Kind::TurnEnd, "r1");
        add(&mut sp, true, Kind::Prompt, "u2");
        add(&mut sp, false, Kind::Message, "a2");
        sp.delivered = sp.seq; // the window saw it all
        let from = sp.replay_from();
        assert_eq!(from, 150, "the open turn starts at u2");
        let lines: Vec<_> = sp.replay(from).iter().map(|e| e.line.as_str()).collect();
        assert_eq!(lines, ["init", "bg", "u2", "a2"]);
        assert_eq!(sp.replay(0).len(), 7, "from 0 is everything");
        add(&mut sp, false, Kind::TurnEnd, "r2");
        sp.delivered = sp.seq;
        assert!(sp.replay_from() >= 170, "idle: nothing open, so from now");
    }
}

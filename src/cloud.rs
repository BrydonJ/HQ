//! Cloud cells: a session that runs on Anthropic's Managed Agents instead of a process on this PC,
//! billed to the API key saved in HQ's settings.
//!
//! Managed Agents hosts the agent loop and the container its tools run in; HQ only sends the user's
//! messages and reads the session's event stream (SSE). This file translates that stream into the
//! stream-json lines the frontend already speaks, as hermes.rs does for ACP, so the chat itself
//! needs nothing new. The cloud session is created with the first message: nothing is provisioned
//! (and nothing billed) for a cell that never sends one. Deleting this file, its `mod` line,
//! `Sessions::register_remote` and `ui/cloud.js` (with the hooks that call it) removes cloud cells.
//!
//! The container starts empty: it has none of the folder the cell was opened on.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{json, Map, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};

use crate::session::{Sessions, SpawnOptions};
use crate::store;

const API: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
const BETA: &str = "managed-agents-2026-04-01";

/// The models a cloud cell offers: id, name shown, description. The first is the default.
const MODELS: [(&str, &str, &str); 4] = [
    ("claude-sonnet-5-5", "Sonnet 5.5", "Fast and capable for everyday work ($2 / $10 per MTok)"),
    ("claude-opus-5-5", "Opus 5.5", "The most capable Opus ($4 / $20 per MTok)"),
    ("claude-haiku-5-5", "Haiku 5.5", "Cheapest and quickest ($0.10 / $0.50 per MTok)"),
    ("claude-fable-5-1", "Fable 5.1", "Anthropic's most capable model ($10 / $50 per MTok)"),
];

const SYSTEM_PROMPT: &str = "You are running in a cloud sandbox started from HQ, a dashboard on the user's PC. The user's own files are not in this sandbox: it starts empty, so say so rather than guess when a task needs them, and ask for the content or the repository URL.";

/// How many times a dropped event stream is reopened before the session is given up on.
const RECONNECTS: u32 = 5;

/// The IDs of what HQ made in the user's Console organization, kept so every cell reuses one
/// environment and one agent per model instead of leaving a new pair behind each time.
#[derive(Default)]
struct Provisioned {
    environment: Option<String>,
    /// Agent ID by model.
    agents: Map<String, Value>,
}

fn ids_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(store::state_path(app)?.with_file_name("cloud.json"))
}

fn load_ids(path: &PathBuf) -> Provisioned {
    let Some(v) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return Provisioned::default();
    };
    Provisioned {
        environment: v["environment"].as_str().map(str::to_owned),
        agents: v["agents"].as_object().cloned().unwrap_or_default(),
    }
}

fn save_ids(path: &PathBuf, ids: &Provisioned) {
    let _ = std::fs::write(path, json!({ "environment": ids.environment, "agents": ids.agents }).to_string());
}

#[derive(Clone)]
struct Api {
    client: Client,
    key: String,
}

impl Api {
    fn new(key: String) -> Result<Api, String> {
        let client = Client::builder().timeout(Duration::from_secs(60)).redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())?;
        Ok(Api { client, key })
    }

    fn request(&self, client: &Client, method: reqwest::Method, path: &str) -> RequestBuilder {
        let mut key = reqwest::header::HeaderValue::from_str(&self.key).unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""));
        key.set_sensitive(true);
        client
            .request(method, format!("{API}{path}"))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", BETA)
    }

    fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let mut rb = self.request(&self.client, method, path);
        if let Some(body) = body {
            rb = rb.json(body);
        }
        let resp = rb.send().map_err(|e| ApiError { status: None, message: format!("Could not reach Anthropic: {e}") })?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.is_success() {
            return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
        }
        Err(ApiError { status: Some(status.as_u16()), message: api_error_message(status.as_u16(), &text) })
    }
}

struct ApiError {
    status: Option<u16>,
    message: String,
}

fn api_error_message(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body).ok().and_then(|v| v["error"]["message"].as_str().map(str::to_owned));
    match status {
        401 => "Anthropic refused HQ's API key. Replace it in Settings > API key.".into(),
        403 => format!("This API key cannot use Managed Agents: {}", detail.unwrap_or_else(|| "forbidden".into())),
        429 => format!("Anthropic's rate limit was hit: {}", detail.unwrap_or_else(|| "try again shortly".into())),
        _ => format!("Anthropic returned {status}: {}", detail.unwrap_or_else(|| body.chars().take(200).collect())),
    }
}

/// Cells starting at once must not each make an environment and agent.
static PROVISIONING: Mutex<()> = Mutex::new(());

/// Makes the environment and agent for `model` if HQ has not yet, then a session on them.
fn create_session(api: &Api, ids_file: &PathBuf, model: &str, title: &str) -> Result<String, String> {
    let _one_at_a_time = PROVISIONING.lock().unwrap_or_else(|e| e.into_inner());
    let mut ids = load_ids(ids_file);
    for attempt in 0..2 {
        let environment = match ids.environment.clone() {
            Some(id) => id,
            None => {
                let body = json!({ "name": "HQ cloud", "config": { "type": "cloud", "networking": { "type": "unrestricted" } } });
                let id = api.call(reqwest::Method::POST, "/v1/environments", Some(&body)).map_err(|e| e.message)?["id"]
                    .as_str()
                    .ok_or("Anthropic made an environment but sent no ID")?
                    .to_owned();
                ids.environment = Some(id.clone());
                save_ids(ids_file, &ids);
                id
            }
        };
        let agent = match ids.agents.get(model).and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => {
                let name = MODELS.iter().find(|m| m.0 == model).map_or(model, |m| m.1);
                // Nothing in the sandbox is the user's, so its tools run without asking.
                let body = json!({
                    "name": format!("HQ cloud ({name})"), "model": model, "system": SYSTEM_PROMPT,
                    "tools": [{ "type": "agent_toolset_20260401", "default_config": { "permission_policy": { "type": "always_allow" } } }],
                });
                let id = api.call(reqwest::Method::POST, "/v1/agents", Some(&body)).map_err(|e| e.message)?["id"]
                    .as_str()
                    .ok_or("Anthropic made an agent but sent no ID")?
                    .to_owned();
                ids.agents.insert(model.to_owned(), json!(id));
                save_ids(ids_file, &ids);
                id
            }
        };
        let body = json!({ "agent": agent, "environment_id": environment, "title": title });
        match api.call(reqwest::Method::POST, "/v1/sessions", Some(&body)) {
            Ok(v) => return v["id"].as_str().map(str::to_owned).ok_or_else(|| "Anthropic made a session but sent no ID".into()),
            // The environment or agent was deleted or archived in the Console: make new ones once.
            Err(e) if attempt == 0 && e.status == Some(404) => {
                ids = Provisioned::default();
                save_ids(ids_file, &ids);
            }
            Err(e) => return Err(e.message),
        }
    }
    Err("Could not start a cloud session".into())
}

struct Shared {
    api: Api,
    ids_file: PathBuf,
    /// Dropped when the cell is closed, which ends the session's output for the frontend.
    out: Mutex<Option<PipeWriter>>,
    /// Set when the cell is closed, which ends the stream thread.
    stopped: AtomicBool,
    /// The session is over (ended, or its stream could not be kept): messages are refused.
    dead: AtomicBool,
    session: Mutex<Option<String>>,
    model: Mutex<String>,
}

impl Shared {
    fn emit(&self, msg: Value) {
        let mut out = self.out.lock().unwrap();
        let Some(writer) = out.as_mut() else { return };
        if writeln!(writer, "{msg}").is_err() {
            self.stopped.store(true, Ordering::Relaxed);
        }
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn reply_ok(&self, request_id: &str, response: Value) {
        self.emit(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": response } }));
    }

    fn reply_err(&self, request_id: &str, error: &str) {
        self.emit(json!({ "type": "control_response", "response": { "subtype": "error", "request_id": request_id, "error": error } }));
    }

    /// Ends the turn with an error the cell shows, as Claude Code's own failures are.
    fn fail(&self, error: &str) {
        self.emit(json!({ "type": "result", "subtype": "error", "is_error": true, "result": error }));
    }

    fn send_events(&self, session: &str, events: Value) -> Result<(), String> {
        let path = format!("/v1/sessions/{session}/events");
        self.api.call(reqwest::Method::POST, &path, Some(&json!({ "events": events }))).map(|_| ()).map_err(|e| e.message)
    }
}

pub fn spawn(app: &AppHandle, opts: SpawnOptions, events: Channel<InvokeResponseBody>) -> Result<u64, String> {
    if opts.resume.is_some() {
        return Err("A cloud session cannot be resumed after its cell is closed. Start a new one.".into());
    }
    let key = store::api_key(app)
        .map_err(|e| format!("Could not read the API key from the keychain: {e}"))?
        .ok_or("Cloud sessions run on an API key. Save one in Settings > API key for API credits first.")?;
    let model = opts.model.clone().filter(|m| MODELS.iter().any(|x| x.0 == m)).unwrap_or_else(|| MODELS[0].0.to_owned());
    let (lines, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let shared = Arc::new(Shared {
        api: Api::new(key)?,
        ids_file: ids_path(app)?,
        out: Mutex::new(Some(writer)),
        stopped: AtomicBool::new(false),
        dead: AtomicBool::new(false),
        session: Mutex::new(None),
        model: Mutex::new(model),
    });
    let (input_tx, input_rx) = mpsc::channel::<String>();
    {
        let shared = shared.clone();
        let cwd = opts.cwd.clone();
        std::thread::spawn(move || {
            let mut started = false;
            for line in input_rx {
                frontend(&shared, &cwd, &line, &mut started);
            }
        });
    }
    let on_kill = {
        let shared = shared.clone();
        move || {
            shared.stopped.store(true, Ordering::Relaxed);
            // A turn still running would go on billing the key: stop it, but leave the session in the Console.
            if let Some(session) = shared.session.lock().unwrap().clone() {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let _ = shared.send_events(&session, json!([{ "type": "user.interrupt" }]));
                });
            }
            // Ends the cell's output, which the stream thread's own handle would otherwise keep open.
            shared.out.lock().unwrap().take();
        }
    };
    Ok(app.state::<Sessions>().register_remote(app, input_tx, lines, events, on_kill))
}

fn frontend(shared: &Arc<Shared>, cwd: &str, line: &str, started: &mut bool) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
    match msg["type"].as_str().unwrap_or_default() {
        "user" => {
            let Some(content) = user_content(&msg["message"]["content"]) else { return };
            if shared.dead.load(Ordering::Relaxed) {
                return shared.fail("The cloud session has ended. Start a new one to continue.");
            }
            if !*started {
                *started = true;
                let title = folder_title(cwd);
                let model = shared.model();
                match create_session(&shared.api, &shared.ids_file, &model, &title) {
                    Ok(id) => {
                        *shared.session.lock().unwrap() = Some(id.clone());
                        // The stream replays nothing, so it must be open before the first message goes.
                        let (ready_tx, ready_rx) = mpsc::channel::<()>();
                        let stream = shared.clone();
                        std::thread::spawn(move || run_stream(&stream, ready_tx));
                        if ready_rx.recv_timeout(Duration::from_secs(30)).is_err() {
                            shared.stopped.store(true, Ordering::Relaxed);
                            shared.dead.store(true, Ordering::Relaxed);
                            return shared.fail("Could not open the cloud session's event stream.");
                        }
                    }
                    Err(e) => {
                        *started = false;
                        return shared.fail(&e);
                    }
                }
            }
            // Closed while the session was being made: nobody is listening, so no turn should run.
            if shared.stopped.load(Ordering::Relaxed) {
                return;
            }
            let Some(session) = shared.session.lock().unwrap().clone() else { return };
            shared.emit(json!({
                "type": "system", "subtype": "init", "session_id": session, "model": shared.model(),
                "permissionMode": "bypassPermissions", "cwd": cwd,
            }));
            if let Err(e) = shared.send_events(&session, json!([{ "type": "user.message", "content": content }])) {
                shared.fail(&e);
            }
        }
        "control_request" => {
            let request_id = msg["request_id"].as_str().unwrap_or_default().to_owned();
            control(shared, &request_id, &msg["request"], *started);
        }
        _ => {}
    }
}

fn control(shared: &Arc<Shared>, request_id: &str, req: &Value, started: bool) {
    match req["subtype"].as_str().unwrap_or_default() {
        "initialize" => {
            let models: Vec<Value> = MODELS
                .iter()
                .map(|(id, name, description)| json!({ "value": id, "displayName": name, "description": description, "resolvedModel": id }))
                .collect();
            shared.reply_ok(request_id, json!({ "models": models, "commands": [], "remote_control_available": false }));
        }
        "get_settings" => shared.reply_ok(request_id, json!({ "applied": { "model": shared.model() } })),
        "interrupt" => {
            let session = shared.session.lock().unwrap().clone();
            match session {
                // Off the input thread, so Stop works while a message is still being sent.
                Some(session) => {
                    let (shared, request_id) = (shared.clone(), request_id.to_owned());
                    std::thread::spawn(move || match shared.send_events(&session, json!([{ "type": "user.interrupt" }])) {
                        Ok(()) => shared.reply_ok(&request_id, json!({})),
                        Err(e) => shared.reply_err(&request_id, &e),
                    });
                }
                None => shared.reply_ok(request_id, json!({})),
            }
        }
        // The model belongs to the agent the session was made on, so it can change only before the first message.
        "set_model" => {
            let model = match req["model"].as_str().unwrap_or_default() {
                "default" => MODELS[0].0,
                m => m,
            };
            if started {
                shared.reply_err(request_id, "A cloud session keeps the model it started with. Start a new session to use another.");
            } else if MODELS.iter().any(|m| m.0 == model) {
                *shared.model.lock().unwrap() = model.to_owned();
                shared.reply_ok(request_id, json!({}));
            } else {
                shared.reply_err(request_id, "Cloud sessions offer Sonnet 5.5, Opus 5.5, Haiku 5.5 and Fable 5.1");
            }
        }
        // Nothing in the sandbox asks permission, so there is no mode to change.
        "set_permission_mode" => shared.reply_ok(request_id, json!({})),
        "rename_session" => shared.reply_ok(request_id, json!({ "title": req["title"] })),
        other => shared.reply_err(request_id, &format!("Cloud sessions do not support {other}")),
    }
}

/// The message as Managed Agents takes it: text and image blocks.
fn user_content(content: &Value) -> Option<Value> {
    let blocks: Vec<Value> = match content {
        Value::String(text) => vec![json!({ "type": "text", "text": text })],
        Value::Array(items) => items
            .iter()
            .filter(|b| matches!(b["type"].as_str(), Some("text" | "image")))
            .cloned()
            .collect(),
        _ => return None,
    };
    (!blocks.is_empty()).then_some(Value::Array(blocks))
}

fn folder_title(cwd: &str) -> String {
    let folder = std::path::Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if folder.is_empty() { "HQ".into() } else { format!("HQ - {folder}") }
}

/// Where the cell is in the event stream: the turn it is in and what it has added up.
#[derive(Default)]
struct Turn {
    started: Option<Instant>,
    seen: HashSet<String>,
    tools: HashSet<String>,
    tokens: (u64, u64, u64),
    seq: u64,
}

/// Reads the session's events until the cell is closed or the session ends, reopening the
/// stream when it drops. Events from before a reopening are fetched and replayed, deduped by ID.
fn run_stream(shared: &Arc<Shared>, ready: mpsc::Sender<()>) {
    let Some(session) = shared.session.lock().unwrap().clone() else { return };
    let mut turn = Turn::default();
    let mut failures = 0;
    let mut opened = false;
    while !shared.stopped.load(Ordering::Relaxed) {
        // Every connection after the first catches up on what the last one missed.
        let (seen_before, connected) = (turn.seen.len(), Instant::now());
        // A stream that stayed up, or delivered something, was healthy: an idle one ends at the
        // request timeout every so often. Only streams that fail or close at once count against it.
        let healthy = |turn: &Turn| turn.seen.len() > seen_before || connected.elapsed() > Duration::from_secs(30);
        match read_stream(shared, &session, &mut turn, opened, &ready) {
            Ok(Ended::Terminated) => break,
            Ok(Ended::Dropped) if healthy(&turn) => failures = 0,
            Ok(Ended::Dropped) => failures += 1,
            Err(_) if healthy(&turn) => failures = 0,
            Err(e) => {
                failures += 1;
                if failures > RECONNECTS {
                    shared.fail(&format!("The connection to the cloud session was lost: {e}"));
                    break;
                }
            }
        }
        if failures > RECONNECTS {
            shared.fail("The connection to the cloud session was lost.");
            break;
        }
        opened = true;
        std::thread::sleep(Duration::from_millis(500 * u64::from(failures + 1)));
    }
    shared.dead.store(true, Ordering::Relaxed);
}

enum Ended {
    /// The session is over for good.
    Terminated,
    /// The stream ended while the session lives.
    Dropped,
}

fn read_stream(shared: &Arc<Shared>, session: &str, turn: &mut Turn, replay: bool, ready: &mpsc::Sender<()>) -> Result<Ended, String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        // A dead connection is noticed by keepalive probes; the cap makes a stream that hangs anyway
        // reopen (and replay what it missed) now and then.
        .tcp_keepalive(Duration::from_secs(30))
        .timeout(Duration::from_secs(10 * 60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = shared
        .api
        .request(&client, reqwest::Method::GET, &format!("/v1/sessions/{session}/events/stream"))
        .header("accept", "text/event-stream")
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(api_error_message(status, &resp.text().unwrap_or_default()));
    }
    let _ = ready.send(());
    // The stream replays nothing, so anything that happened while it was down comes from the list.
    if replay {
        let path = format!("/v1/sessions/{session}/events?limit=1000");
        if let Ok(history) = shared.api.call(reqwest::Method::GET, &path, None) {
            let mut events: Vec<&Value> = history["data"].as_array().into_iter().flatten().collect();
            events.sort_by_key(|e| e["processed_at"].as_str().unwrap_or_default().to_owned());
            for ev in events {
                if translate(shared, turn, ev) {
                    return Ok(Ended::Terminated);
                }
            }
        }
    }
    for line in BufReader::new(resp).lines() {
        if shared.stopped.load(Ordering::Relaxed) {
            return Ok(Ended::Terminated);
        }
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
        if translate(shared, turn, &ev) {
            return Ok(Ended::Terminated);
        }
    }
    Ok(Ended::Dropped)
}

fn tool_name(name: &str) -> String {
    match name {
        "bash" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "glob" => "Glob",
        "grep" => "Grep",
        "web_fetch" => "WebFetch",
        "web_search" => "WebSearch",
        other => other,
    }
    .into()
}

/// A tool result's content as plain text.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Turns one session event into the lines the frontend reads. True when the session is over.
fn translate(shared: &Arc<Shared>, turn: &mut Turn, ev: &Value) -> bool {
    if let Some(id) = ev["id"].as_str().filter(|id| !id.is_empty()) {
        if !turn.seen.insert(id.to_owned()) {
            return false;
        }
    }
    let session = shared.session.lock().unwrap().clone();
    let model = shared.model();
    let assistant = |turn: &mut Turn, content: Value| {
        turn.seq += 1;
        let id = ev["id"].as_str().map_or_else(|| format!("cloud-msg-{}", turn.seq), str::to_owned);
        json!({
            "type": "assistant", "session_id": session, "parent_tool_use_id": null,
            "message": { "id": id, "type": "message", "role": "assistant", "model": model, "content": content },
        })
    };
    match ev["type"].as_str().unwrap_or_default() {
        "session.status_running" => {
            turn.started.get_or_insert_with(Instant::now);
        }
        "agent.message" => {
            let text = result_text(&ev["content"]);
            if !text.trim().is_empty() {
                let msg = assistant(turn, json!([{ "type": "text", "text": text }]));
                shared.emit(msg);
            }
        }
        "agent.tool_use" | "agent.mcp_tool_use" | "agent.custom_tool_use" => {
            let id = ev["id"].as_str().unwrap_or_default().to_owned();
            let name = tool_name(ev["name"].as_str().unwrap_or("tool"));
            turn.tools.insert(id.clone());
            let msg = assistant(turn, json!([{ "type": "tool_use", "id": id, "name": name, "input": ev["input"] }]));
            shared.emit(msg);
        }
        "agent.tool_result" | "agent.mcp_tool_result" => {
            let id = ev["tool_use_id"].as_str().unwrap_or_default();
            if turn.tools.remove(id) {
                shared.emit(json!({
                    "type": "user", "session_id": session, "parent_tool_use_id": null,
                    "message": { "role": "user", "content": [{
                        "type": "tool_result", "tool_use_id": id,
                        "content": result_text(&ev["content"]), "is_error": ev["is_error"].as_bool().unwrap_or(false),
                    }] },
                }));
            }
        }
        "span.model_request_end" => {
            let usage = &ev["model_usage"];
            let n = |k: &str| usage[k].as_u64().unwrap_or(0);
            turn.tokens.0 += n("input_tokens") + n("cache_creation_input_tokens");
            turn.tokens.1 += n("output_tokens");
            turn.tokens.2 += n("cache_read_input_tokens");
        }
        "session.error" => {
            let message = ev["error"]["message"].as_str().or_else(|| ev["message"].as_str()).unwrap_or("The cloud session reported an error");
            shared.fail(message);
            turn.started = None;
        }
        "session.status_idle" => {
            // Tools never ask, so waiting on the client means something else is wrong.
            match ev["stop_reason"]["type"].as_str() {
                Some("requires_action") => {}
                Some("budget_reached") => {
                    shared.fail("The cloud session reached its spending limit.");
                    turn.started = None;
                }
                _ => end_turn(shared, turn, &model),
            }
        }
        "session.status_terminated" => {
            shared.fail("The cloud session ended. Start a new one to continue.");
            return true;
        }
        _ => {}
    }
    false
}

fn end_turn(shared: &Arc<Shared>, turn: &mut Turn, model: &str) {
    let Some(started) = turn.started.take() else { return };
    turn.tools.clear();
    let tokens = std::mem::take(&mut turn.tokens); // each result reports its own turn
    let mut model_usage = Map::new();
    model_usage.insert(model.to_owned(), json!({ "inputTokens": tokens.0, "outputTokens": tokens.1, "cacheReadInputTokens": tokens.2 }));
    shared.emit(json!({
        "type": "result", "subtype": "success", "is_error": false, "session_id": shared.session.lock().unwrap().clone(),
        "duration_ms": started.elapsed().as_millis() as u64, "modelUsage": model_usage,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::PipeReader;

    fn shared() -> (Arc<Shared>, PipeReader) {
        let (reader, writer) = std::io::pipe().unwrap();
        let shared = Arc::new(Shared {
            api: Api::new("sk-ant-api03-test".into()).unwrap(),
            ids_file: std::env::temp_dir().join("hq-cloud-test.json"),
            out: Mutex::new(Some(writer)),
            stopped: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            session: Mutex::new(Some("sesn_1".into())),
            model: Mutex::new("claude-sonnet-5-5".into()),
        });
        (shared, reader)
    }

    /// Everything translated so far, read back as JSON lines.
    fn lines(shared: Arc<Shared>, reader: PipeReader) -> Vec<Value> {
        shared.out.lock().unwrap().take(); // closes the writer
        BufReader::new(reader).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect()
    }

    #[test]
    fn a_turn_becomes_assistant_tool_and_result_lines() {
        let (shared, reader) = shared();
        let mut turn = Turn::default();
        for ev in [
            json!({ "type": "session.status_running", "id": "e1" }),
            json!({ "type": "agent.tool_use", "id": "e2", "name": "bash", "input": { "command": "ls" } }),
            json!({ "type": "agent.tool_result", "id": "e3", "tool_use_id": "e2", "content": [{ "type": "text", "text": "a.txt" }] }),
            json!({ "type": "agent.message", "id": "e4", "content": [{ "type": "text", "text": "One file." }] }),
            json!({ "type": "span.model_request_end", "id": "e5", "model_usage": { "input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 2 } }),
            json!({ "type": "session.status_idle", "id": "e6", "stop_reason": { "type": "end_turn" } }),
        ] {
            assert!(!translate(&shared, &mut turn, &ev));
        }
        let out = lines(shared, reader);
        assert_eq!(out[0]["message"]["content"][0]["name"], "Bash");
        assert_eq!(out[1]["message"]["content"][0]["tool_use_id"], "e2");
        assert_eq!(out[1]["message"]["content"][0]["content"], "a.txt");
        assert_eq!(out[2]["message"]["content"][0]["text"], "One file.");
        assert_eq!(out[3]["type"], "result");
        assert_eq!(out[3]["subtype"], "success");
        assert_eq!(out[3]["modelUsage"]["claude-sonnet-5-5"]["outputTokens"], 5);
    }

    #[test]
    fn an_event_seen_twice_after_a_reconnect_is_translated_once() {
        let (shared, reader) = shared();
        let mut turn = Turn::default();
        let msg = json!({ "type": "agent.message", "id": "e1", "content": [{ "type": "text", "text": "Hi" }] });
        translate(&shared, &mut turn, &msg);
        translate(&shared, &mut turn, &msg);
        assert_eq!(lines(shared, reader).len(), 1);
    }

    #[test]
    fn a_terminated_session_ends_the_stream_with_an_error() {
        let (shared, reader) = shared();
        let mut turn = Turn::default();
        assert!(translate(&shared, &mut turn, &json!({ "type": "session.status_terminated", "id": "e1" })));
        let out = lines(shared, reader);
        assert_eq!(out[0]["is_error"], true);
    }

    #[test]
    fn an_idle_with_no_turn_open_sends_no_result() {
        let (shared, reader) = shared();
        let mut turn = Turn::default();
        translate(&shared, &mut turn, &json!({ "type": "session.status_idle", "id": "e1", "stop_reason": { "type": "end_turn" } }));
        assert!(lines(shared, reader).is_empty());
    }

    #[test]
    fn takes_text_and_images_and_nothing_else_from_a_message() {
        let content = json!([{ "type": "text", "text": "hi" }, { "type": "tool_result" }, { "type": "image", "source": {} }]);
        assert_eq!(user_content(&content).unwrap().as_array().unwrap().len(), 2);
        assert!(user_content(&json!([])).is_none());
        assert_eq!(user_content(&json!("hello")).unwrap()[0]["text"], "hello");
    }

    #[test]
    fn explains_the_errors_a_user_can_fix() {
        assert!(api_error_message(401, "{}").contains("Settings"));
        let body = r#"{"error":{"message":"no access"}}"#;
        assert!(api_error_message(403, body).contains("no access"));
    }

    #[test]
    fn offers_the_default_model_first() {
        assert_eq!(MODELS[0].0, "claude-sonnet-5-5");
    }
}

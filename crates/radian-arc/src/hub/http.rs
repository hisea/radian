//! `radian hub`'s HTTP API (ADR 0048 decision 16): loopback only, a bearer token on every
//! request, and the read routes that return exactly what `radian arc list --json` and
//! `radian arc show --json` print.
//!
//! The listener binds `127.0.0.1:<[hub] port>` and nothing else. The server takes `hub.lock`
//! before it binds, so a second hub is refused by the lock and not by the port; a port another
//! program holds is a startup error that names it. The token file is read at start and
//! re-read whenever its mtime changed, one `stat` per request, so `radian hub token rotate`
//! takes effect on the next request without a restart.
//!
//! The HTTP accept loop runs on a thread of its own; the scheduling loop (ADR 0048 decision 13)
//! runs on this thread, holding `hub.lock` for its life, so one slow pass delays no request.
//!
//! Every route is a `GET`; the router is hand-written and each route answers JSON, except
//! `GET /v1/events`, which is a server-sent-event stream. The list and show bodies come from
//! [`crate::store::cli::list_json_text`] and [`crate::store::cli::show_json_text`], the same
//! functions the CLI prints, so a field change in the CLI changes the route.
//!
//! The route work (files, git, the token's `stat`) runs on `spawn_blocking`, never on the
//! runtime's async threads, so the scheduling loop's threads (ADR 0048 decision 13) are not
//! blocked by a request.

use std::convert::Infallible;
use std::fs;
use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{
    AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue,
    WWW_AUTHENTICATE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::gate::Stage;
use crate::gate::Verdict;
use crate::hub::lock::HubLock;
use crate::hub::registry::Registry;
use crate::hub::token;
use crate::read;
use crate::store::attempt_ops;
use crate::store::{Error, Store, cli, model};
use crate::transitions::{CHANNEL_INTERVAL_SECS, Poller, Repo, Transition};

/// The gap between two `: keep-alive` comments on an event stream that has nothing to send.
pub const KEEP_ALIVE_SECS: u64 = 15;

/// The API version `GET /v1/version` reports. It names the route set and its behavior; the
/// plan and task file shape is `schema_version` (ADR 0048 decision 5) and changes on its own.
pub const API_VERSION: u32 = 1;

/// The most diff text `GET …/diff/<path>` returns, in bytes.
pub const DIFF_TEXT_LIMIT: usize = 256 * 1024;

/// The address every hub binds: loopback IPv4, on `port`. There is no key that moves it.
#[must_use]
pub fn bind_address(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Runs the hub until the process ends: it creates the token when it is missing, takes
/// `hub.lock`, binds [`bind_address`], prints `hub: listening on <addr>` as its first stdout
/// line, and then serves the HTTP API while the scheduling loop (ADR 0048 decision 13) runs
/// every registered repository on this thread.
///
/// # Errors
///
/// The hub directory or the token cannot be prepared, `hub.lock` is held by another hub, or the
/// port cannot be bound.
pub fn serve(port: u16) -> Result<(), Error> {
    let registry = Registry::open()?;
    let dir = registry.dir().to_path_buf();
    radian_host::ensure_private_dir(&dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let value = token::ensure(&dir)?;
    // Before any bind or any repository: two hubs are refused by the lock, never by the port
    // (ADR 0048 §16).
    let lock = HubLock::take(&dir)?;
    let address = bind_address(port);
    let listener = std::net::TcpListener::bind(address)
        .map_err(|error| Error::Failed(format!("cannot bind {address}: {error}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| Error::Failed(format!("cannot prepare {address}: {error}")))?;
    let bound = listener
        .local_addr()
        .map_err(|error| Error::Failed(format!("cannot read the hub address: {error}")))?;
    radian_host::write_line(&format!("hub: listening on {bound}"))
        .map_err(|error| Error::Failed(format!("cannot print the hub address: {error}")))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::Failed(format!("cannot start the hub runtime: {error}")))?;
    let tokens = Arc::new(Mutex::new(TokenState::new(&dir, value)));
    let http_registry = registry.clone();
    // The accept loop has a thread and a runtime of its own; no request waits on a pass.
    std::thread::spawn(move || {
        runtime.block_on(serve_http(listener, http_registry, tokens));
    });
    let emit = Arc::new(|text: &str| {
        let _ = radian_host::write_stdout(text);
    });
    super::r#loop::run(&registry, emit, lock)
}

/// Serves the hub's HTTP API on `listener` until the process ends.
async fn serve_http(
    listener: std::net::TcpListener,
    registry: Registry,
    tokens: Arc<Mutex<TokenState>>,
) {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("hub: cannot use the hub listener: {error}");
            return;
        }
    };
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("hub: cannot accept a hub connection: {error}");
                return;
            }
        };
        let registry = registry.clone();
        let tokens = Arc::clone(&tokens);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let registry = registry.clone();
                let tokens = Arc::clone(&tokens);
                async move {
                    let response =
                        tokio::task::spawn_blocking(move || handle(request, &registry, &tokens))
                            .await
                            .unwrap_or_else(|_| {
                                error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "the request handler panicked",
                                )
                            });
                    Ok::<_, Infallible>(response)
                }
            });
            let _ = http1::Builder::new()
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Prints what `radian hub pair` prints: the URL, the token, and (on a terminal, unless
/// `no_qr`) the QR code of `radian://hub?url=<url>&token=<token>` as Unicode half blocks. A
/// pipe or file gets the two text lines only.
///
/// # Errors
///
/// The token cannot be prepared, the QR code cannot be built, or stdout cannot be written.
pub fn pair(port: u16, no_qr: bool) -> Result<(), Error> {
    let registry = Registry::open()?;
    let value = token::ensure(registry.dir())?;
    let url = format!("http://127.0.0.1:{port}");
    radian_host::write_line(&url)
        .map_err(|error| Error::Failed(format!("cannot print the hub URL: {error}")))?;
    radian_host::write_line(&value)
        .map_err(|error| Error::Failed(format!("cannot print the hub token: {error}")))?;
    if no_qr || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    let payload = format!("radian://hub?url={url}&token={value}");
    let code = qrcode::QrCode::new(payload.as_bytes())
        .map_err(|error| Error::Failed(format!("cannot build the pairing QR code: {error}")))?;
    let art = code.render::<qrcode::render::unicode::Dense1x2>().build();
    radian_host::write_stdout(&format!("{art}\n"))
        .map_err(|error| Error::Failed(format!("cannot print the pairing QR code: {error}")))
}

/// Writes a new token and prints `rotated`. A running hub re-reads the file on its next
/// request, so the old token stops working then.
///
/// # Errors
///
/// The token file cannot be written.
pub fn rotate() -> Result<(), Error> {
    let registry = Registry::open()?;
    token::rotate(registry.dir())?;
    radian_host::write_line("rotated")
        .map_err(|error| Error::Failed(format!("cannot print the rotation result: {error}")))
}

// ---- startup -----------------------------------------------------------------------------

/// The token the server accepts. [`TokenState::authorized`] is the one `stat` per request; the
/// file is read only when its mtime differs from the recorded one, so a rotation is picked up
/// without a restart.
struct TokenState {
    path: PathBuf,
    value: String,
    mtime: Option<SystemTime>,
}

impl TokenState {
    fn new(dir: &Path, value: String) -> Self {
        let path = token::path(dir);
        let mtime = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        Self { path, value, mtime }
    }

    /// Whether `header` is `Bearer <token>` with the token the file holds now.
    fn authorized(&mut self, header: Option<&str>) -> bool {
        self.refresh();
        let Some(header) = header else {
            return false;
        };
        let Some((scheme, value)) = header.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("bearer") && value.trim() == self.value
    }

    fn refresh(&mut self) {
        let Ok(metadata) = fs::metadata(&self.path) else {
            return;
        };
        let mtime = metadata.modified().ok();
        if mtime == self.mtime {
            return;
        }
        if let Ok(text) = fs::read_to_string(&self.path) {
            let value = text.trim().to_owned();
            if !value.is_empty() {
                self.value = value;
            }
            self.mtime = mtime;
        }
    }
}

// ---- the router --------------------------------------------------------------------------

/// One route the router knows. A name and an id are percent-decoded; a diff path keeps its
/// `/` separators and is decoded after they are joined.
enum Route {
    Version,
    Repos,
    Items(String),
    Item(String, String),
    Report(String, String),
    Verdicts(String, String),
    Diff(String, String),
    DiffFile(String, String, String),
    Events,
    Unknown,
}

fn route(path: &str) -> Route {
    let path = path.strip_prefix('/').unwrap_or(path);
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["v1", "version"] => Route::Version,
        ["v1", "repos"] => Route::Repos,
        ["v1", "events"] => Route::Events,
        ["v1", "repos", name, "items"] => Route::Items(decode(name)),
        ["v1", "repos", name, "items", id] => Route::Item(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "report"] => Route::Report(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "verdicts"] => Route::Verdicts(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "diff"] => Route::Diff(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "diff", rest @ ..] if !rest.is_empty() => {
            Route::DiffFile(decode(name), decode(id), decode(&rest.join("/")))
        }
        _ => Route::Unknown,
    }
}

/// Percent-decodes a path segment or a joined path. Invalid escapes and non-UTF-8 bytes are
/// kept as they are, so a path the router does not understand becomes a 404 and never a panic.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle(
    request: Request<hyper::body::Incoming>,
    registry: &Registry,
    tokens: &Mutex<TokenState>,
) -> Response<ResponseBody> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let authorized = tokens
        .lock()
        .map(|mut state| state.authorized(header.as_deref()))
        .unwrap_or(false);
    if !authorized {
        let mut response = json_response(StatusCode::UNAUTHORIZED, json!({}));
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    let route = route(&path);
    if matches!(route, Route::Unknown) {
        return error_response(StatusCode::NOT_FOUND, "no such route");
    }
    if method != Method::GET {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    match route {
        Route::Version => json_response(
            StatusCode::OK,
            json!({
                "api_version": API_VERSION,
                "schema_version": model::SCHEMA_VERSION,
            }),
        ),
        Route::Repos => repos(registry),
        Route::Items(name) => items(registry, &name),
        Route::Item(name, id) => item(registry, &name, &id),
        Route::Report(name, task) => report(registry, &name, &task),
        Route::Verdicts(name, task) => verdicts(registry, &name, &task),
        Route::Diff(name, task) => diff(registry, &name, &task),
        Route::DiffFile(name, task, path) => diff_file(registry, &name, &task, &path),
        Route::Events => events(registry),
        Route::Unknown => unreachable!("an unknown route returned above"),
    }
}

// ---- the read routes ---------------------------------------------------------------------

fn repos(registry: &Registry) -> Response<ResponseBody> {
    match registry.repos() {
        Ok(entries) => {
            let list: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    // The scheduling loop holds `runner.lock` for every repository it schedules
                    // (ADR 0048 decision 13). Another process's pid means the hub only reads
                    // that repository; no pid means it is not locked at this moment.
                    let holder = Store::at(entry.path.clone()).runner_lock_pid();
                    json!({
                        "name": entry.name,
                        "path": entry.path.display().to_string(),
                        "scheduled": holder == Some(std::process::id()),
                        "lock_holder": holder.map(|pid| pid.to_string()),
                    })
                })
                .collect();
            json_response(StatusCode::OK, Value::Array(list))
        }
        Err(error) => failed(error),
    }
}

fn items(registry: &Registry, name: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    match cli::list_json_text(&store, None, false) {
        Ok(text) => bytes_response(StatusCode::OK, text.into_bytes()),
        Err(error) => failed(error),
    }
}

fn item(registry: &Registry, name: &str, id: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    match cli::show_json_text(&store, id) {
        Ok(text) => bytes_response(StatusCode::OK, text.into_bytes()),
        Err(error) => not_found(error),
    }
}

fn report(registry: &Registry, name: &str, task: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // The id must name a stored task before the run directories are looked at: an unknown id
    // (or a plan's) is the CLI's 404, not a task with no attempt.
    if let Err(error) = store.load_task(task) {
        return not_found(error);
    }
    let attempts = match read::task_attempts(&store, task) {
        Ok(attempts) => attempts,
        Err(error) => return failed(error),
    };
    let Some(newest) = attempts.last() else {
        return no_content();
    };
    // The ADR 0048 decision 7 hand-off reads the same file: the last `Result` line of the
    // newest attempt's `result.jsonl`.
    let result = attempt_ops::run_dir(&store, task, newest.attempt).join("result.jsonl");
    match last_report(&result) {
        Some(text) => json_response(
            StatusCode::OK,
            json!({
                "attempt": format!("{task}-{}", newest.attempt),
                "text": text,
            }),
        ),
        None => no_content(),
    }
}

/// The report line one attempt's `result.jsonl` ends with: the last `Result` record's `text`,
/// trimmed. `None` for no record, no `text`, or an empty one.
fn last_report(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("Result") {
            continue;
        }
        let text = value.get("text")?.as_str()?.trim();
        return (!text.is_empty()).then(|| text.to_owned());
    }
    None
}

fn verdicts(registry: &Registry, name: &str, task: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // The id must name a stored task, so an unknown id (or a plan's) is the CLI's 404 rather
    // than an empty list.
    if let Err(error) = store.load_task(task) {
        return not_found(error);
    }
    let attempts = match read::task_attempts(&store, task) {
        Ok(attempts) => attempts,
        Err(error) => return failed(error),
    };
    let Some(newest) = attempts.last() else {
        return json_response(StatusCode::OK, json!([]));
    };
    let dir = attempt_ops::run_dir(&store, task, newest.attempt).join("verdicts");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json_response(StatusCode::OK, json!([]));
        }
        Err(error) => {
            return failed(Error::Failed(format!(
                "cannot read {}: {error}",
                dir.display()
            )));
        }
    };
    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let verdict = match Verdict::read(&path) {
            Ok(verdict) => verdict,
            Err(error) => {
                return failed(Error::Failed(format!("cannot read a verdict: {error}")));
            }
        };
        list.push(json!({
            "stage": verdict.stage.as_str(),
            "result": if verdict.is_pass() { "pass" } else { "fail" },
            "reason": verdict.reason.clone(),
            "path": path.display().to_string(),
            "verdict": serde_json::to_value(&verdict).unwrap_or(Value::Null),
        }));
    }
    list.sort_by_key(|entry| stage_order(entry.get("stage").and_then(Value::as_str)));
    json_response(StatusCode::OK, Value::Array(list))
}

/// The order a verdict list prints in, the same order `radian arc show` reads its stages.
fn stage_order(stage: Option<&str>) -> usize {
    const ORDER: [Stage; 6] = [
        Stage::Commands,
        Stage::Mutation,
        Stage::Review,
        Stage::Ci,
        Stage::Human,
        Stage::Integration,
    ];
    stage
        .and_then(|name| ORDER.iter().position(|stage| stage.as_str() == name))
        .unwrap_or(ORDER.len())
}

fn diff(registry: &Registry, name: &str, task: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // Only `--numstat` is read: the list never holds a file's diff text (a huge changed file
    // costs one line), and the per-file route below reads at most `DIFF_TEXT_LIMIT` bytes.
    match read::task_diff_counts(&store, task) {
        Ok(files) => json_response(
            StatusCode::OK,
            Value::Array(
                files
                    .iter()
                    .map(|file| {
                        json!({
                            "path": file.path,
                            "added": file.added,
                            "removed": file.removed,
                        })
                    })
                    .collect(),
            ),
        ),
        Err(error) => not_found(error),
    }
}

fn diff_file(registry: &Registry, name: &str, task: &str, path: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // The requested file only: git's output is read up to the limit and the process stopped
    // there, so a multi-GB change never reaches memory whole.
    let diff = match read::task_diff_file(&store, task, path, DIFF_TEXT_LIMIT) {
        Ok(Some(diff)) => diff,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!("no file `{path}` in the task's diff"),
            );
        }
        Err(error) => return not_found(error),
    };
    let (text, cut) = cap(&diff.text, DIFF_TEXT_LIMIT);
    json_response(
        StatusCode::OK,
        json!({
            "path": path,
            "truncated": diff.truncated || cut,
            "text": text,
        }),
    )
}

/// `text` cut at `limit` bytes, never inside a character, and whether it was cut.
fn cap(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

// ---- the event stream --------------------------------------------------------------------

/// `GET /v1/events`: one `event: transition` per state change since the previous read, and a
/// `: keep-alive` comment while nothing changes. The stream is not durable: it starts with
/// the current lists as its baseline, so a client that reconnects re-reads them instead of
/// asking for a replay.
///
/// The read happens on a thread of its own, never on the runtime's async threads, and stops
/// when the response body is dropped (the client went away) or the process ends. The
/// registry is re-read every interval, so a repository registered while the stream is open
/// is picked up with a baseline of its own.
fn events(registry: &Registry) -> Response<ResponseBody> {
    let (sender, receiver) = mpsc::channel(EVENT_QUEUE);
    let registry = registry.clone();
    let _ = std::thread::Builder::new()
        .name("hub-events".to_owned())
        .spawn(move || stream_events(registry, sender));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(ResponseBody::Stream(receiver))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "no body"))
}

/// One transition frame: `event: transition` and its one-line JSON object.
fn event_frame(repo: &str, transition: &Transition) -> Bytes {
    let data = match transition {
        Transition::Task { id, task, previous } => json!({
            "repo": repo,
            "kind": "task",
            "id": id,
            "field": "state",
            "previous": previous,
            "value": task.full,
        }),
        Transition::Plan { id, event } => json!({
            "repo": repo,
            "kind": "plan",
            "id": id,
            "field": event.field,
            "previous": event.previous,
            "value": event.value,
        }),
    };
    Bytes::from(format!("event: transition\ndata: {data}\n\n"))
}

/// The event stream's producer: poll every [`event_poll_interval`], write every transition,
/// and write a keep-alive comment after [`event_keep_alive_interval`] with nothing sent. A
/// closed receiver is the client going away, so the thread ends there.
fn stream_events(registry: Registry, sender: mpsc::Sender<Result<Frame<Bytes>, Infallible>>) {
    let poll = event_poll_interval();
    let keep_alive = event_keep_alive_interval();
    let mut poller = Poller::new();
    let mut next_poll = Instant::now();
    let mut last_sent = Instant::now();
    loop {
        if sender.is_closed() {
            return;
        }
        let now = Instant::now();
        if now >= next_poll {
            next_poll = now + poll;
            for (repo, transition) in read_transitions(&registry, &mut poller) {
                if sender
                    .blocking_send(Ok(Frame::data(event_frame(&repo, &transition))))
                    .is_err()
                {
                    return;
                }
                last_sent = Instant::now();
            }
        }
        if last_sent.elapsed() >= keep_alive {
            if sender
                .blocking_send(Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n"))))
                .is_err()
            {
                return;
            }
            last_sent = Instant::now();
        }
        let now = Instant::now();
        let wake = next_poll.min(last_sent + keep_alive);
        if wake > now {
            std::thread::sleep(wake - now);
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// One round of the event stream's read: every store the registry names right now. A registry
/// that cannot be read or a store that cannot be opened is left out this round; the next
/// round tries again.
fn read_transitions(registry: &Registry, poller: &mut Poller) -> Vec<(String, Transition)> {
    let Ok(entries) = registry.repos() else {
        return Vec::new();
    };
    let repos: Vec<Repo> = entries
        .into_iter()
        .map(|entry| Repo {
            name: entry.name,
            store: Store::at(entry.path),
        })
        .collect();
    poller.poll(&repos)
}

/// The event stream's read interval. Test-only: `RADIAN_HUB_EVENTS_POLL_SECS` shortens the
/// channel's five-second cadence so a test does not wait a whole interval per transition; no
/// user sets it.
fn event_poll_interval() -> Duration {
    test_interval(
        std::env::var("RADIAN_HUB_EVENTS_POLL_SECS").ok(),
        CHANNEL_INTERVAL_SECS,
    )
}

/// The keep-alive gap. Test-only: `RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS` shortens the fifteen
/// seconds so a test can see a keep-alive without waiting; no user sets it.
fn event_keep_alive_interval() -> Duration {
    test_interval(
        std::env::var("RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS").ok(),
        KEEP_ALIVE_SECS,
    )
}

/// `value` as seconds, when it holds a positive integer; `default` otherwise. The two readers
/// above are the only callers, and both variables exist only for tests.
fn test_interval(value: Option<String>, default: u64) -> Duration {
    let secs = value
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

// ---- responses ---------------------------------------------------------------------------

/// How many event frames may wait for the connection to take them. A client that stops reading
/// blocks this producer on a full queue; a client that goes away closes the receiver.
const EVENT_QUEUE: usize = 64;

/// A response body: one byte string (a JSON route, or nothing for a `204`), or the frames of
/// an event stream.
enum ResponseBody {
    Once(Option<Bytes>),
    Stream(mpsc::Receiver<Result<Frame<Bytes>, Infallible>>),
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            ResponseBody::Once(bytes) => {
                Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes))))
            }
            ResponseBody::Stream(receiver) => receiver.poll_recv(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ResponseBody::Once(bytes) => bytes.is_none(),
            ResponseBody::Stream(_) => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let ResponseBody::Once(Some(bytes)) = self {
            hint.set_exact(bytes.len() as u64);
        }
        hint
    }
}

fn response(status: StatusCode, body: Option<Vec<u8>>) -> Response<ResponseBody> {
    let mut builder = Response::builder()
        .status(status)
        .header(CONNECTION, "close");
    match body {
        Some(bytes) => {
            builder = builder
                .header(CONTENT_TYPE, "application/json")
                .header(CONTENT_LENGTH, bytes.len().to_string());
            builder
                .body(ResponseBody::Once(Some(Bytes::from(bytes))))
                .unwrap_or_else(|_| Response::new(ResponseBody::Once(None)))
        }
        None => builder
            .body(ResponseBody::Once(None))
            .unwrap_or_else(|_| Response::new(ResponseBody::Once(None))),
    }
}

fn bytes_response(status: StatusCode, bytes: Vec<u8>) -> Response<ResponseBody> {
    response(status, Some(bytes))
}

fn json_response(status: StatusCode, value: Value) -> Response<ResponseBody> {
    response(status, Some(serde_json::to_vec(&value).unwrap_or_default()))
}

fn error_response(status: StatusCode, message: &str) -> Response<ResponseBody> {
    json_response(status, json!({ "error": message }))
}

fn not_found(error: Error) -> Response<ResponseBody> {
    error_response(StatusCode::NOT_FOUND, &error.message())
}

fn failed(error: Error) -> Response<ResponseBody> {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.message())
}

fn no_content() -> Response<ResponseBody> {
    response(StatusCode::NO_CONTENT, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bind_address_is_loopback() {
        let address = bind_address(7341);
        assert_eq!(address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(address.port(), 7341);
        assert_ne!(address.ip(), IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn a_path_is_decoded_and_an_invalid_escape_is_kept() {
        assert_eq!(decode("src%2Fmain.rs"), "src/main.rs");
        assert_eq!(decode("a%2"), "a%2");
        assert_eq!(decode("plain.txt"), "plain.txt");
    }

    #[test]
    fn the_diff_cap_is_256_kib_and_never_splits_a_character() {
        assert_eq!(DIFF_TEXT_LIMIT, 262_144);
        let text = "aé";
        let (cut, truncated) = cap(text, 2);
        assert!(truncated);
        assert_eq!(cut, "a", "two bytes would split the character after it");
        let (whole, truncated) = cap("abc", 3);
        assert!(!truncated);
        assert_eq!(whole, "abc");
    }
}

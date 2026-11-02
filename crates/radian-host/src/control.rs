//! The process control port (ADR 0043): one Unix socket per hosting process,
//! JSON lines, the same input verbs the inbox exposes.
//!
//! **What it is.** Every process that hosts an interactive or `radian exec` session binds
//! `$RADIAN_HOME/run/<pid>.sock` (directory 0700, file 0600). The socket is where an external
//! script or agent drives the session: it can enqueue `followup`/`steer`/`inject` inputs,
//! `cancel` the running turn, `list_sessions` (the hosting session), and `subscribe` to the
//! committed event stream. There are no task verbs (`spawn_task`, `wait_task`, `task_status`)
//! and no separate `radian serve` (ADR 0043): the process that owns a session owns its socket.
//!
//! **Sources (ADR 0043, decided item 4; ADR 0011 L2, ADR 0012).** The source is fixed by
//! the port, never by the client's word: it defaults to [`InputSource::Scheduler`], a client
//! may declare `Subagent` or `Mcp`, and `User`/`PermissionDecision` are refused with an error
//! line. The one exception is the owner's token: a request whose `token` equals the trimmed
//! content of `$RADIAN_HOME/hub/token` is the owner's input and is `User` (with
//! `{"via":"hub"}` recorded in the message's `origin`, ADR 0005 G1). The input's source is
//! what the inbox records and what the turn it starts runs with. A turn that is already
//! running keeps its own effective source, which only ever lowers (ADR 0011 L2): a `steer` or
//! `inject` joins it and can lower it, never raise it, while a `followup` belongs to a later
//! turn and leaves the running turn alone — the turn it opens applies its own source when the
//! driver claims it. Approvals cannot be answered over the port —
//! there is deliberately no such verb, and an unknown verb (including an `approve`) changes
//! nothing.
//!
//! **Inputs carry attachments.** `followup`/`steer`/`inject` may carry `images` (base64 plus a
//! MIME) and `files` (a name plus text). They become the message's parts through
//! [`crate::compose::user_message`], the same composer the TUI uses for a dropped image and
//! an `@` file. One request is one JSON line, at most [`MAX_REQUEST_BYTES`] bytes; a longer
//! line is answered with an error and discarded through its newline, and the connection stays
//! usable.
//!
//! **Never blocks the runtime.** Handlers run on the runtime and use the async `Inbox`, not
//! `LocalSession`'s `block_on` wrappers (ADR 0043; the background's rule). `subscribe`
//! fans the committed events out to each client through a **bounded** per-client channel: a
//! client that falls behind is disconnected, and the producer never waits on a client.
//!
//! **Cleanup (ADR 0043).** The socket is removed when the server is dropped — the normal
//! exit path and the panic-unwind path both drop it. At startup every `*.sock` whose pid is no
//! longer alive is removed, so a `kill -9` does not leave it behind.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use radian_coding::image::LoadedImage;
use radian_core::{AgentEvent, EventSink, InputSource, Message, OpaqueJson};
use radian_runtime::{CancelCause, RuntimeEvents};
use radian_session::RuntimeEvent;
use serde_json::{Map, Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::compose::{Attachment, MAX_FILE_BYTES, MAX_TOTAL_BYTES, user_message};
use crate::session::LocalSession;

/// What the port needs from the hosting session: its id, its inbox, its permission hooks and
/// whether a turn is running. Every session handle is **weak**, so serving the socket never
/// keeps a closed session (or its audit sink) alive — a host's teardown still observes the
/// session dropping. A request upgrades what it needs; if the session has closed it is refused.
#[derive(Clone)]
pub struct ControlTarget {
    pub session_id: String,
    inbox: Weak<radian_runtime::Inbox>,
    hooks: Weak<radian_permissions::PermissionHooks>,
    /// The driver's turn flag (a standalone atomic, cloned weakly): a mid-turn input must not
    /// raise the turn's source (ADR 0011 L2).
    turn_running: Weak<AtomicBool>,
}

impl ControlTarget {
    /// The port's view of an open session.
    pub fn of(session: &LocalSession) -> Self {
        Self {
            session_id: session.id().as_str().to_owned(),
            inbox: Arc::downgrade(session.inbox()),
            hooks: Arc::downgrade(session.hooks()),
            turn_running: Arc::downgrade(&session.turn_running_handle()),
        }
    }

    /// The session's inbox, or `None` once the session has closed.
    fn inbox(&self) -> Option<Arc<radian_runtime::Inbox>> {
        self.inbox.upgrade()
    }

    /// The session's permission hooks, or `None` once the session has closed.
    fn hooks(&self) -> Option<Arc<radian_permissions::PermissionHooks>> {
        self.hooks.upgrade()
    }

    /// Whether a turn is running right now, or `None` once the session has closed.
    fn turn_running(&self) -> Option<bool> {
        self.turn_running
            .upgrade()
            .map(|flag| flag.load(Ordering::SeqCst))
    }
}

/// How many lines may wait for one subscriber before it is disconnected. Small on purpose: a
/// subscriber that cannot keep up with a live session is dropped, never allowed to slow the
/// session down (ADR 0043's "缓冲有界，慢客户端被断开").
pub const SUBSCRIBER_BUFFER: usize = 32;

/// The largest request line accepted, in bytes, excluding its terminating newline (16 MiB). A
/// few phone photos fit: base64 grows a 5 MiB image to about 6.7 MiB. A longer line is
/// answered with one error line and discarded through its newline; the connection stays
/// usable, so the client can retry with a smaller request.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// How much the kernel may buffer for one accepted connection before the writer starts to see
/// backpressure. Small so a subscriber that stops reading reaches the bounded buffer quickly;
/// the exact number does not matter to correctness.
#[cfg(unix)]
const SOCKET_SEND_BUFFER: i32 = 4096;

/// One subscribed client's hand-off slot: the connection loop drains `tx`, and the fan-out
/// only ever `try_send`s, so a slow reader can never block the runtime.
struct Client {
    id: u64,
    tx: mpsc::Sender<Value>,
    /// Woken (and removed) when the client falls behind or the connection ends.
    close: Arc<Notify>,
}

/// The process's control socket and the live subscribers feeding off its committed events.
pub struct ControlServer {
    /// The `$RADIAN_HOME` the socket lives under. The owner token is read from
    /// `<home>/hub/token` on every request, so a rotation takes effect at the next request.
    home: PathBuf,
    path: PathBuf,
    #[cfg(unix)]
    listener: Mutex<Option<tokio::net::UnixListener>>,
    clients: Mutex<Vec<Client>>,
    next_id: AtomicU64,
}

impl ControlServer {
    /// Binds `$RADIAN_HOME/run/<pid>.sock`, creating the directory 0700 and the socket 0600,
    /// and removing any stale `*.sock` of a dead pid first (ADR 0043). A bind failure is
    /// reported to the caller, which then runs without a control port.
    pub fn bind(home: &Path) -> std::io::Result<Arc<Self>> {
        #[cfg(unix)]
        {
            bind_unix(home)
        }
        #[cfg(not(unix))]
        {
            let _ = home;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "the control port is Unix-only",
            ))
        }
    }

    /// The socket path this server owns.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A guard that removes this server's socket file when it drops. The host holds it as a
    /// local, so the file goes on the normal return path **and** on a panic unwind even when
    /// an in-flight connection still holds a reference to the server (ADR 0043).
    pub fn guard(&self) -> SocketGuard {
        SocketGuard {
            path: self.path.clone(),
        }
    }

    /// Binds the control server for this process when it is enabled and a home exists
    /// (`[control] enabled` in the config). A bind failure is returned so the host can warn and
    /// run without a port; a disabled control port is `Ok(None)`.
    pub fn bind_if(
        enabled: bool,
        home: Option<&Path>,
    ) -> std::io::Result<Option<Arc<ControlServer>>> {
        if !enabled {
            return Ok(None);
        }
        let Some(home) = home else {
            return Ok(None);
        };
        ControlServer::bind(home).map(Some)
    }

    /// Wraps the host's committed-`AgentEvent` sink so subscribers see the same events the
    /// host does, minus the live-only deltas (the TUI's channel also drops those).
    pub fn events(self: &Arc<Self>, inner: Arc<dyn EventSink>) -> Arc<dyn EventSink> {
        Arc::new(AgentFanout {
            inner,
            server: Arc::clone(self),
        })
    }

    /// Wraps the host's `RuntimeEvent` sink the same way.
    pub fn runtime_events(
        self: &Arc<Self>,
        inner: Arc<dyn RuntimeEvents>,
    ) -> Arc<dyn RuntimeEvents> {
        Arc::new(RuntimeFanout {
            inner,
            server: Arc::clone(self),
        })
    }

    /// Accepts connections until the process ends. A no-op when the platform has no Unix
    /// sockets, or when [`ControlServer::serve`] already took the listener.
    #[cfg(unix)]
    pub fn serve(self: &Arc<Self>, runtime: &tokio::runtime::Handle, target: ControlTarget) {
        let Some(listener) = self
            .listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                let Ok((stream, _addr)) = listener.accept().await else {
                    break;
                };
                let _ = set_small_send_buffer(&stream);
                // Only a weak reference lives in the accept loop: when the host drops its
                // `Arc`, `ControlServer::drop` removes the socket file. A connection task keeps
                // its own strong reference for as long as the client is connected.
                let Some(server) = weak.upgrade() else {
                    break;
                };
                let target = target.clone();
                tokio::spawn(async move {
                    server.serve_connection(stream, target).await;
                });
            }
        });
    }

    /// The non-Unix no-op; see the Unix [`ControlServer::serve`].
    #[cfg(not(unix))]
    pub fn serve(self: &Arc<Self>, _runtime: &tokio::runtime::Handle, _target: ControlTarget) {}

    /// Registers a subscriber's output channel and returns its id.
    fn register(&self, tx: mpsc::Sender<Value>, close: Arc<Notify>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Client { id, tx, close });
        id
    }

    /// Removes a subscriber (idempotent).
    fn deregister(&self, id: u64) {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|client| client.id != id);
    }

    /// Fans one pre-serialized line out to every subscriber. Non-blocking: a full or closed
    /// slot means the client is gone, so it is removed and its connection woken to close.
    fn push(&self, value: Value) {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let mut slow: Vec<u64> = Vec::new();
        for client in clients.iter() {
            if client.tx.try_send(value.clone()).is_err() {
                slow.push(client.id);
            }
        }
        if !slow.is_empty() {
            clients.retain(|client| {
                if slow.contains(&client.id) {
                    client.close.notify_one();
                    false
                } else {
                    true
                }
            });
        }
    }

    /// One connection: read request lines, dispatch them, and write responses and subscribed
    /// events on the same bounded channel. The runtime thread only ever `push`es here.
    #[cfg(unix)]
    async fn serve_connection(
        self: Arc<Self>,
        stream: tokio::net::UnixStream,
        target: ControlTarget,
    ) {
        use tokio::io::{AsyncWriteExt, BufReader};

        let (read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Value>(SUBSCRIBER_BUFFER);
        let close = Arc::new(Notify::new());
        let mut reader = BufReader::new(read);
        let mut client_id: Option<u64> = None;
        loop {
            tokio::select! {
                biased;
                _ = close.notified() => break,
                line = read_request_line(&mut reader) => {
                    match line {
                        RequestLine::Line(bytes) => {
                            let Ok(line) = String::from_utf8(bytes) else {
                                let response = failure(Value::Null, "the request line is not UTF-8");
                                if tx.send(response).await.is_err() {
                                    break;
                                }
                                continue;
                            };
                            if line.trim().is_empty() {
                                continue;
                            }
                            let response = self
                                .dispatch(&line, &target, &tx, &close, &mut client_id)
                                .await;
                            if tx.send(response).await.is_err() {
                                break;
                            }
                        }
                        RequestLine::TooLong => {
                            if tx.send(failure(Value::Null, over_limit_message())).await.is_err() {
                                break;
                            }
                        }
                        RequestLine::Eof | RequestLine::Error => break,
                    }
                }
                Some(value) = rx.recv() => {
                    let mut encoded = match serde_json::to_vec(&value) {
                        Ok(encoded) => encoded,
                        Err(_) => break,
                    };
                    encoded.push(b'\n');
                    if write.write_all(&encoded).await.is_err() {
                        break;
                    }
                    let _ = write.flush().await;
                }
            }
        }
        if let Some(id) = client_id {
            self.deregister(id);
        }
        // `write` and `read` drop here: the client sees EOF.
    }

    /// Parses and runs one request line, returning the response object (ADR 0043's
    /// protocol: `{"id": n, "verb": …}` → `{"id": n, "ok": true, …}`).
    async fn dispatch(
        &self,
        line: &str,
        target: &ControlTarget,
        tx: &mpsc::Sender<Value>,
        close: &Arc<Notify>,
        client_id: &mut Option<u64>,
    ) -> Value {
        let request: Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(error) => return failure(Value::Null, format!("invalid JSON: {error}")),
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let Some(verb) = request.get("verb").and_then(Value::as_str) else {
            return failure(id, "a request needs a string `verb`");
        };
        match verb {
            "followup" | "steer" | "inject" => self.input(verb, &id, &request, target).await,
            "cancel" => {
                let Some(inbox) = target.inbox() else {
                    return failure(id, "the session has closed");
                };
                inbox.cancel(CancelCause::Requested);
                success(id, None)
            }
            "list_sessions" => success(id, Some(json!({ "sessions": self.sessions(target) }))),
            "subscribe" => {
                *client_id = Some(self.register(tx.clone(), Arc::clone(close)));
                success(id, None)
            }
            other => failure(
                id,
                format!("unknown verb `{other}`: the control port has no approval or answer verb"),
            ),
        }
    }

    /// `followup`/`steer`/`inject` (ADR 0011 L1's four ops minus `cancel`): fix the source the
    /// port decides (the owner token makes it `User`, otherwise the client's declared
    /// `Scheduler`/`Subagent`/`Mcp`), build the message through the shared composer, then
    /// enqueue on the async inbox.
    async fn input(
        &self,
        verb: &str,
        id: &Value,
        request: &Value,
        target: &ControlTarget,
    ) -> Value {
        let Some(text) = request.get("text").and_then(Value::as_str) else {
            return failure(id.clone(), "`text` is required");
        };
        // A bad attachment rejects the whole request before anything is enqueued, so a
        // partially built message can never reach the inbox.
        let (images, files) = match parse_attachments(request) {
            Ok(attachments) => attachments,
            Err(reason) => return failure(id.clone(), reason),
        };
        let via_hub = self.is_owner(request);
        let source = if via_hub {
            InputSource::User
        } else {
            match parse_source(request) {
                Ok(source) => source,
                Err(reason) => return failure(id.clone(), reason),
            }
        };
        let (Some(hooks), Some(inbox), Some(turn_running)) =
            (target.hooks(), target.inbox(), target.turn_running())
        else {
            return failure(id.clone(), "the session has closed");
        };
        // The source the port fixed is what the durable record and the inbox enqueue see
        // (ADR 0011 L2), and the turn-claimed observer resets the permission source to it
        // when this input's own turn starts. Here only a turn that is already in flight
        // matters: a `steer`/`inject` is admitted into it, so it can lower its source but
        // never raise it; a `followup` belongs to a later turn, so it must not touch this
        // one at all. With nothing in flight the input may start the next turn, and the set
        // keeps the state right until the claim restates it.
        if turn_running || inbox.inflight_turns() > 0 {
            if verb != "followup" {
                hooks.observe_input_source(source);
            }
        } else {
            hooks.set_turn_source(Some(source));
        }
        let mut message = user_message(text, &files, &images);
        if via_hub {
            set_origin(&mut message, HUB_ORIGIN);
        }
        let result = match verb {
            "followup" => inbox.followup(message, source).await,
            "steer" => inbox.steer(message, source).await,
            _ => inbox.inject(message, source).await,
        };
        match result {
            Ok(message_id) => success(id.clone(), Some(json!({ "message_id": message_id }))),
            Err(error) => failure(id.clone(), error.to_string()),
        }
    }

    /// Whether the request carries the owner's token. The token file is read on every request
    /// (`$RADIAN_HOME/hub/token`, so a rotation takes effect at once); a missing, unreadable or
    /// empty file means no token matches. The comparison is constant-time over the content.
    fn is_owner(&self, request: &Value) -> bool {
        let Some(provided) = request.get("token").and_then(Value::as_str) else {
            return false;
        };
        let Some(expected) = self.owner_token() else {
            return false;
        };
        token_matches(provided, &expected)
    }

    /// The trimmed content of `$RADIAN_HOME/hub/token`, or `None` when it cannot be read or is
    /// empty.
    fn owner_token(&self) -> Option<String> {
        let path = crate::home::hub_dir_for(&self.home).join("token");
        let content = std::fs::read_to_string(path).ok()?;
        let token = content.trim();
        (!token.is_empty()).then(|| token.to_owned())
    }

    /// `list_sessions`: the hosting session (ADR 0043). The port serves one process, so this
    /// is one `main` row.
    fn sessions(&self, target: &ControlTarget) -> Vec<Value> {
        vec![json!({ "id": target.session_id, "kind": "main" })]
    }
}

/// One read from a connection: a complete line, an over-limit line that was discarded through
/// its newline, end of file, or an I/O error.
#[cfg(unix)]
enum RequestLine {
    Line(Vec<u8>),
    TooLong,
    Eof,
    Error,
}

/// Reads one newline-terminated request line without ever holding more than [`MAX_REQUEST_BYTES`]
/// bytes of it. An over-limit line is still consumed through its newline (so the next read
/// starts at a line boundary) and reported as [`RequestLine::TooLong`]; the caller answers it and
/// keeps the connection. A `\r` before the newline is not part of the line, as `BufRead::lines`
/// treated it.
#[cfg(unix)]
async fn read_request_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> RequestLine {
    use tokio::io::AsyncBufReadExt;

    let mut line: Vec<u8> = Vec::new();
    let mut over = false;
    loop {
        let (take, found, chunk) = {
            let available = match reader.fill_buf().await {
                Ok(available) => available,
                Err(_) => return RequestLine::Error,
            };
            if available.is_empty() {
                return if over {
                    RequestLine::TooLong
                } else if line.is_empty() {
                    RequestLine::Eof
                } else {
                    RequestLine::Line(line)
                };
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(pos) => (pos + 1, true, available[..=pos].to_vec()),
                None => (available.len(), false, available.to_vec()),
            }
        };
        reader.consume(take);
        if over {
            if found {
                return RequestLine::TooLong;
            }
            continue;
        }
        if found {
            let content = &chunk[..chunk.len() - 1];
            // One extra byte is allowed for a `\r` that sits exactly at the limit before the
            // newline.
            if line.len() + content.len() > MAX_REQUEST_BYTES + 1 {
                return RequestLine::TooLong;
            }
            line.extend_from_slice(content);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_REQUEST_BYTES {
                return RequestLine::TooLong;
            }
            return RequestLine::Line(line);
        }
        // One extra byte is allowed for a `\r` that may sit exactly at the limit before the
        // newline arrives in the next chunk.
        if line.len() + chunk.len() > MAX_REQUEST_BYTES + 1 {
            over = true;
            line = Vec::new();
        } else {
            line.extend_from_slice(&chunk);
        }
    }
}

/// The error line an over-limit request gets.
#[cfg(unix)]
fn over_limit_message() -> String {
    format!(
        "the request line is over the {} MiB limit",
        MAX_REQUEST_BYTES / (1024 * 1024)
    )
}

/// The `origin` a token-bearing control-port input records (ADR 0005 G1).
const HUB_ORIGIN: &str = r#"{"via":"hub"}"#;

/// Records the hub origin on a user message without touching any other meta field.
fn set_origin(message: &mut Message, origin: &str) {
    let Ok(origin) = OpaqueJson::parse(origin) else {
        return;
    };
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.origin = Some(origin),
    }
}

/// Constant-time comparison of a provided token with the expected one. The loop length is the
/// longer of the two, and every byte is visited, so a prefix never matches and a timing signal
/// does not narrow the token.
fn token_matches(provided: &str, expected: &str) -> bool {
    let provided = provided.as_bytes();
    let expected = expected.as_bytes();
    let mut difference = provided.len() ^ expected.len();
    for index in 0..provided.len().max(expected.len()) {
        let left = provided.get(index).copied().unwrap_or(0);
        let right = expected.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

/// The MIME set the TUI composer accepts; the bytes' magic number still decides the kind
/// (ADR 0018, `radian-coding::image`).
const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Parses the optional `images` and `files` of an input request into the composer's inputs. A
/// failure names the item (an image by its 1-based position, a file by its name) and rejects
/// the whole request before anything is enqueued.
fn parse_attachments(request: &Value) -> Result<(Vec<LoadedImage>, Vec<Attachment>), String> {
    let images = parse_images(request)?;
    let files = parse_files(request)?;
    Ok((images, files))
}

fn parse_images(request: &Value) -> Result<Vec<LoadedImage>, String> {
    use base64::Engine as _;

    let items = match request.get("images") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err("`images` must be an array".into()),
    };
    let mut images = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let number = index + 1;
        let mime = item
            .get("mime")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("image {number}: `mime` is required"))?;
        if !IMAGE_MIMES.contains(&mime) {
            return Err(format!(
                "image {number}: unknown MIME `{mime}`: image/png, image/jpeg, image/gif or image/webp"
            ));
        }
        let data = item
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("image {number}: `data` is required"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| format!("image {number}: `data` is not valid base64"))?;
        let name = format!("image {number}");
        let image = radian_coding::image::load(&name, &bytes)
            .map_err(|error| format!("image {number}: {error}"))?;
        images.push(image);
    }
    Ok(images)
}

fn parse_files(request: &Value) -> Result<Vec<Attachment>, String> {
    let items = match request.get("files") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err("`files` must be an array".into()),
    };
    let mut files = Vec::with_capacity(items.len());
    let mut total = 0usize;
    for item in items {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "a file needs a string `name`".to_owned())?;
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("file {name}: `text` is required"))?;
        if text.len() > MAX_FILE_BYTES {
            return Err(format!(
                "file {name}: {} bytes, over the {MAX_FILE_BYTES}-byte file limit",
                text.len()
            ));
        }
        total += text.len();
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "the attached files exceed the {MAX_TOTAL_BYTES}-byte total limit"
            ));
        }
        files.push(Attachment {
            path: name.to_owned(),
            content: text.to_owned(),
        });
    }
    Ok(files)
}

/// The committed `AgentEvent` stream, fanned out to subscribers (ADR 0043).
struct AgentFanout {
    inner: Arc<dyn EventSink>,
    server: Arc<ControlServer>,
}

impl EventSink for AgentFanout {
    fn emit(&self, event: AgentEvent) {
        if committed_agent(&event) {
            self.server
                .push(json!({ "event": agent_event_json(&event) }));
        }
        self.inner.emit(event);
    }
}

/// The `RuntimeEvent` stream, fanned out the same way.
struct RuntimeFanout {
    inner: Arc<dyn RuntimeEvents>,
    server: Arc<ControlServer>,
}

impl RuntimeEvents for RuntimeFanout {
    fn on_runtime_event(&self, event: &RuntimeEvent) {
        self.server.push(json!({
            "event": {
                "source": "runtime",
                "name": event.name(),
                "data": runtime_event_json(event),
            }
        }));
        self.inner.on_runtime_event(event);
    }

    fn on_model_error(&self, error: &radian_core::ModelError) {
        self.inner.on_model_error(error);
    }

    fn on_stop_failure(&self, error: &radian_core::ModelError, last_assistant_message: &str) {
        self.inner.on_stop_failure(error, last_assistant_message);
    }

    fn on_hook_event(&self, name: &str, data: &serde_json::Value) {
        self.server.push(json!({
            "event": {
                "source": "hook",
                "name": name,
                "data": data,
            }
        }));
        self.inner.on_hook_event(name, data);
    }
}

/// JSON for one `RuntimeEvent`. Like [`agent_event_json`], it lives host-side because
/// `radian-session`'s `RuntimeEvent` is not `Serialize` and `radian-core` is not changed.
fn runtime_event_json(event: &RuntimeEvent) -> Value {
    match event {
        RuntimeEvent::InboxEnqueued {
            message,
            message_id,
            lane,
            source,
            wake,
            degraded,
            pending_wake,
        } => json!({
            "message": message,
            "message_id": message_id,
            "lane": lane,
            "source": source,
            "wake": wake,
            "degraded": degraded,
            "pending_wake": pending_wake,
        }),
        RuntimeEvent::InputClaimed {
            message_ids,
            turn_id,
        } => json!({ "message_ids": message_ids, "turn_id": turn_id }),
        RuntimeEvent::InputAdmitted {
            message_id,
            transcript_index,
        } => json!({ "message_id": message_id, "transcript_index": transcript_index }),
        RuntimeEvent::TurnStarted { turn_id } => json!({ "turn_id": turn_id }),
        RuntimeEvent::TurnFinished {
            turn_id,
            outcome,
            reason,
            stop_reason,
        } => json!({
            "turn_id": turn_id,
            "outcome": outcome,
            "reason": reason,
            "stop_reason": stop_reason,
        }),
        RuntimeEvent::TurnFailed { turn_id, message } => {
            json!({ "turn_id": turn_id, "message": message })
        }
        RuntimeEvent::ConfigChanged { revision, change } => {
            json!({ "revision": revision, "change": change })
        }
        RuntimeEvent::ContextBudgetReached {
            turn_id,
            source,
            projected_tokens,
            window_tokens,
            threshold_percent,
            baseline_input_tokens,
            estimated_growth_tokens,
        } => json!({
            "turn_id": turn_id,
            "source": source,
            "projected_tokens": projected_tokens,
            "window_tokens": window_tokens,
            "threshold_percent": threshold_percent,
            "baseline_input_tokens": baseline_input_tokens,
            "estimated_growth_tokens": estimated_growth_tokens,
        }),
        RuntimeEvent::PermissionResolved {
            request_id,
            call_id,
            decision,
            approver,
            reason,
            policy,
        } => json!({
            "request_id": request_id,
            "call_id": call_id,
            "decision": decision,
            "approver": approver,
            "reason": reason,
            "policy": policy,
        }),
        RuntimeEvent::ApprovalPolicyChanged { policy } => json!({ "policy": policy }),
        RuntimeEvent::SessionForked {
            child_session_id,
            parent_seq,
        } => json!({ "child_session_id": child_session_id, "parent_seq": parent_seq }),
        RuntimeEvent::ProjectTrustChanged { path, decision } => {
            json!({ "path": path, "decision": decision })
        }
        RuntimeEvent::UserBash {
            command,
            exclude_from_context,
            exit_code,
            cancelled,
        } => json!({
            "command": command,
            "exclude_from_context": exclude_from_context,
            "exit_code": exit_code,
            "cancelled": cancelled,
        }),
        RuntimeEvent::BackgroundJobStarted {
            job_id,
            command,
            output,
        } => json!({ "job_id": job_id, "command": command, "output": output }),
        RuntimeEvent::BackgroundJobSettled {
            job_id,
            state,
            duration_ms,
        } => json!({ "job_id": job_id, "state": state, "duration_ms": duration_ms }),
        // `#[non_exhaustive]`: a future variant carries no data this build knows.
        _ => json!({}),
    }
}

/// The events a subscriber gets: every committed `AgentEvent` except the live-only deltas
/// (the TUI's channel drops those too; ADR 0010's live vs durable split).
fn committed_agent(event: &AgentEvent) -> bool {
    !matches!(
        event,
        AgentEvent::ModelTextDelta { .. } | AgentEvent::ModelReasoningDelta { .. }
    )
}

/// JSON for one `AgentEvent`. `radian-core` is not touched (no `radian-core` change),
/// so this host-side encoder is the wire form; it is deliberate, not a `Serialize` impl.
fn agent_event_json(event: &AgentEvent) -> Value {
    let (name, data) = match event {
        AgentEvent::TurnStarted => ("turn_started", json!({})),
        AgentEvent::SteeringApplied { count } => ("steering_applied", json!({ "count": count })),
        AgentEvent::ModelRequestStarted { model_call, budget } => (
            "model_request_started",
            json!({ "model_call": model_call, "budget": budget_json(budget.as_ref()) }),
        ),
        AgentEvent::ModelRetrying {
            attempt,
            max,
            delay,
            reason,
        } => (
            "model_retrying",
            json!({
                "attempt": attempt,
                "max": max,
                "delay_ms": delay.as_millis(),
                "reason": reason,
            }),
        ),
        AgentEvent::ModelTextDelta { .. } => ("model_text_delta", json!({})),
        AgentEvent::ModelReasoningDelta { .. } => ("model_reasoning_delta", json!({})),
        AgentEvent::ModelResponseReceived {
            stop_reason,
            tool_calls,
            usage,
            // The control port does not forward `limits` in this slice (ADR 0027
            // slice ④): rate-limit windows are a live footer fact, not a committed one.
            ..
        } => (
            "model_response_received",
            json!({
                "stop_reason": format!("{stop_reason:?}"),
                "tool_calls": tool_calls,
                "usage": usage,
            }),
        ),
        AgentEvent::ToolStarted {
            index,
            call_id,
            name,
        } => (
            "tool_started",
            json!({ "index": index, "call_id": call_id, "name": name }),
        ),
        AgentEvent::TranscriptAppended { index, message } => (
            "transcript_appended",
            json!({ "index": index, "message": message }),
        ),
        AgentEvent::TranscriptReplaced { revision, len } => (
            "transcript_replaced",
            json!({ "revision": revision, "len": len }),
        ),
        AgentEvent::CompactionStarted {
            revision,
            tokens_before,
            keep_target,
            trigger,
        } => (
            "compaction_started",
            json!({
                "revision": revision,
                "tokens_before": tokens_before,
                "keep_target": keep_target,
                "trigger": format!("{trigger:?}"),
            }),
        ),
        AgentEvent::CompactionFinished {
            revision,
            tokens_before,
            tokens_after,
            below_target,
            deterministic,
            dropped,
        } => (
            "compaction_finished",
            json!({
                "revision": revision,
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
                "below_target": below_target,
                "deterministic": deterministic,
                "dropped": dropped,
            }),
        ),
        AgentEvent::CompactionUsage {
            revision,
            model,
            usage,
        } => (
            "compaction_usage",
            json!({ "revision": revision, "model": model, "usage": usage }),
        ),
        AgentEvent::CompactionFailed { reason } => {
            ("compaction_failed", json!({ "reason": reason }))
        }
        AgentEvent::CompactionSkipped { reason } => {
            ("compaction_skipped", json!({ "reason": reason }))
        }
        AgentEvent::ToolProgress { index, progress } => (
            "tool_progress",
            json!({ "index": index, "progress": progress }),
        ),
        AgentEvent::ToolFinished {
            index,
            call_id,
            is_error,
        } => (
            "tool_finished",
            json!({ "index": index, "call_id": call_id, "is_error": is_error }),
        ),
        AgentEvent::HookFailed { hook, message } => (
            "hook_failed",
            json!({ "hook": format!("{hook:?}"), "message": message }),
        ),
        AgentEvent::RunFinished { status } => (
            "run_finished",
            json!({ "status": run_status_label(status) }),
        ),
        // `#[non_exhaustive]`: a future variant is forwarded with its name only.
        _ => ("unknown", json!({})),
    };
    let mut object = Map::new();
    object.insert("source".to_owned(), Value::String("agent".to_owned()));
    object.insert("name".to_owned(), Value::String(name.to_owned()));
    object.insert("data".to_owned(), data);
    Value::Object(object)
}

/// A compact, honest form of a budget report (its fields are public but the type is not
/// `Serialize`; `radian-core` is not changed, ADR 0043).
fn budget_json(budget: Option<&radian_core::BudgetReport>) -> Value {
    match budget {
        Some(report) => json!({
            "source": report.source,
            "projected_tokens": report.projected_tokens,
            "input_budget_tokens": report.input_budget_tokens,
            "request_bytes": report.request_bytes,
            "baseline_input_tokens": report.baseline_input_tokens,
            "estimated_growth_tokens": report.estimated_growth_tokens,
        }),
        None => Value::Null,
    }
}

/// The `RunStatus` as a stable snake_case label (it is not `Serialize`).
fn run_status_label(status: &radian_core::RunStatus) -> &'static str {
    use radian_core::RunStatus;
    match status {
        RunStatus::Completed => "completed",
        RunStatus::Incomplete(_) => "incomplete",
        RunStatus::Refused => "refused",
        RunStatus::BudgetExceeded => "budget_exceeded",
        RunStatus::Cancelled(_) => "cancelled",
        RunStatus::Failed => "failed",
        RunStatus::Poisoned => "poisoned",
        _ => "unknown",
    }
}

/// The source a client may declare (ADR 0043, decided item 4). Absent is `Scheduler`;
/// `User`/`PermissionDecision` are refused.
fn parse_source(request: &Value) -> Result<InputSource, String> {
    match request.get("source") {
        None => Ok(InputSource::Scheduler),
        Some(Value::String(name)) => match name.as_str() {
            "scheduler" => Ok(InputSource::Scheduler),
            "subagent" => Ok(InputSource::Subagent),
            "mcp" => Ok(InputSource::Mcp),
            "user" => Err(
                "a client cannot declare the `User` source; only the owner token grants it".into(),
            ),
            "permission_decision" => {
                Err("the control port never accepts the `PermissionDecision` source".into())
            }
            other => Err(format!(
                "unknown source `{other}`: Scheduler (default), Subagent or Mcp"
            )),
        },
        Some(_) => Err("`source` must be a string".into()),
    }
}

/// A successful response: `{"id": n, "ok": true}` plus the payload's fields (ADR 0043's
/// protocol puts the verb's own fields at the top level). A non-object payload is nested under
/// `result`.
fn success(id: Value, payload: Option<Value>) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), id);
    object.insert("ok".to_owned(), Value::Bool(true));
    match payload {
        Some(Value::Object(fields)) => object.extend(fields),
        Some(other) => {
            object.insert("result".to_owned(), other);
        }
        None => {}
    }
    Value::Object(object)
}

/// A failed response: `{"id": n, "ok": false, "error": "…"}`.
fn failure(id: Value, message: impl Into<String>) -> Value {
    json!({ "id": id, "ok": false, "error": message.into() })
}

// ---------------------------------------------------------------------------
// Unix-only bind/cleanup helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn bind_unix(home: &Path) -> std::io::Result<Arc<ControlServer>> {
    let dir = crate::home::run_dir_for(home);
    crate::home::ensure_private_dir(&dir)?;
    clean_stale_sockets(&dir);
    let path = dir.join(format!("{}.sock", std::process::id()));
    // A previous run that reused this pid: its socket is stale even though the pid is alive.
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    set_socket_mode(&path, 0o600)?;
    Ok(Arc::new(ControlServer {
        home: home.to_path_buf(),
        path,
        listener: Mutex::new(Some(listener)),
        clients: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(1),
    }))
}

/// Removes every `*.sock` in `dir` whose pid is not alive (ADR 0043). A name that is not
/// `<pid>.sock` is left alone.
#[cfg(unix)]
fn clean_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = name.strip_suffix(".sock") else {
            continue;
        };
        if let Ok(pid) = pid.parse::<i32>()
            && pid > 0
            && !pid_alive(pid)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether a pid is alive, checked with signal 0 (`kill(2)`). `EPERM` means the process exists
/// but this user may not signal it, which still counts as alive.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission/existence check only.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Sets the socket file's mode (0600; ADR 0043).
#[cfg(unix)]
fn set_socket_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Keeps the kernel's send buffer for an accepted socket small, so a subscriber that stops
/// reading hits the bounded buffer quickly (the exact size is not correctness-critical).
#[cfg(unix)]
fn set_small_send_buffer(stream: &tokio::net::UnixStream) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let value: libc::c_int = SOCKET_SEND_BUFFER;
    // SAFETY: a plain setsockopt on a live fd with a correctly sized value.
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Removes one control socket file when dropped (ADR 0043). A host keeps this as a local:
/// unlike the server's own `Drop`, it is not shared with any task, so the socket is removed on
/// the normal return path and on a panic unwind even while the process's driver still runs.
pub struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    /// The path this guard owns.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

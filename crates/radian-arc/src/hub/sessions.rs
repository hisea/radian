//! The hub's view of this machine's radian sessions (ADR 0048 decision 16, read routes).
//!
//! **Discovery.** Every hosting radian process binds `$RADIAN_HOME/run/<pid>.sock` and serves the
//! control protocol there (ADR 0043). A scanner thread reads that directory every
//! [`SCAN_INTERVAL_SECS`] and calls `list_sessions` on each socket whose file name is a live
//! pid's; the row it answers names the session's workspace, and the hub assigns the session to
//! the registered repository whose path is the same directory, compared after canonicalizing
//! both sides ([`radian_host::sessions::same_directory`]). A live session in a workspace that is
//! not registered is not shown anywhere. A socket whose process died, or that does not answer
//! within [`PROBE_TIMEOUT`], leaves the list on the next scan.
//!
//! **Ended sessions.** They are not on any socket; they come from the same lookup
//! `radian exec --list` uses — the session index under `$RADIAN_SESSION_DIR` (default
//! `$RADIAN_HOME/sessions`), filtered by the repository's workspace.
//!
//! **Subscriptions.** One long-lived `subscribe` connection per live session feeds that
//! session's event stream (the `…/events` route). It carries the hub token, so the port treats
//! it as the owner's and the `pending` event carries the full head; it reconnects when it drops,
//! and the scanner forgets it when the socket goes away. The connection's events are folded
//! into frames by polling the journal with the same [`AttachFold`] `radian attach` uses, so the
//! stream and the transcript route can never disagree. The same connection is the
//! `[hub.hooks] after_pending` point: the hub remembers, per socket, the head it ran the hook
//! for, so a head the port re-sends starts nothing — also when the scanner forgot the socket
//! and found it again — and a new process starts over with its own `request_id` sequence.
//!
//! **Threads.** The scan, every probe and every subscription is blocking socket and file work on
//! an OS thread of its own, never on the HTTP runtime's async threads (the rule the scheduling
//! loops follow, ADR 0048 decision 16). A socket that accepts and never answers therefore
//! delays no request: only the two routes that name one session's port talk to it, and they run
//! on `spawn_blocking`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use hyper::body::Bytes;
use radian_host::config::HubHooks;
use radian_host::transcript::{AcpFold, AttachFold};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::hub::hooks::{self, PendingHead};
use crate::process;
use crate::store::Store;

/// The interval between two scans of the run directory.
pub const SCAN_INTERVAL_SECS: u64 = 2;

/// The longest a `list_sessions` probe waits for a socket that accepts and does not answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The longest the `models` route waits for a session's port.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a dropped subscription waits before it reconnects.
const RECONNECT_PAUSE: Duration = Duration::from_millis(200);

/// How often a subscription's read wakes to notice that its socket was forgotten.
const READ_POLL: Duration = Duration::from_millis(250);

/// How many event frames one session's stream holds for a slow client before it is dropped; a
/// client that fell behind re-reads the transcript, which is the rule the stream already has.
const EVENT_QUEUE: usize = 64;

/// One live session, as its process's control port last reported it (`list_sessions`, ADR 0043).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// The session id.
    pub id: String,
    /// The workspace the port reported, as written; it is canonicalized where it is compared.
    pub workspace: String,
    /// `idle`, `working` or `blocked` (ADR 0034 H2's classification).
    pub state: String,
    /// The approval and question queue's length.
    pub pending: u64,
    /// The model route in force.
    pub provider: String,
    pub model: String,
}

impl LiveSession {
    /// The `model` object the session routes report.
    #[must_use]
    pub fn route(&self) -> Value {
        json!({ "provider": self.provider, "model": self.model })
    }

    /// Whether this live session belongs to the repository at `repo`.
    #[must_use]
    pub fn in_repo(&self, repo: &Path) -> bool {
        radian_host::sessions::same_directory(&self.workspace, &repo.to_string_lossy())
    }
}

/// Why a session route cannot answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The session is not this repository's, or there is no such session or task: `404`.
    Unknown(String),
    /// The session has no live process, so there is no port to ask: `409`.
    NotLive(String),
    /// The session's port failed: `502`.
    Port(String),
    /// A read of the session data or the index failed: `500`.
    Failed(String),
}

impl SessionError {
    /// The message the error body carries.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            SessionError::Unknown(message)
            | SessionError::NotLive(message)
            | SessionError::Port(message)
            | SessionError::Failed(message) => message,
        }
    }
}

/// The hub's live-session state: what each socket's port last said, and one event fan-out per
/// live session.
pub struct SessionHub {
    /// `$RADIAN_HOME`: the `run/` directory is scanned and `hub/token` is read from here.
    home: PathBuf,
    /// `[hub.hooks]` (ADR 0048 §16), read once when the hub starts.
    hooks: HubHooks,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The live sessions, keyed by the socket path that reported them.
    live: BTreeMap<PathBuf, LiveSession>,
    /// The event fan-out of each live session id.
    events: BTreeMap<String, broadcast::Sender<Bytes>>,
    /// The head the `after_pending` hook already ran for on each socket. It belongs to the hub
    /// and not to the subscription's [`Stream`] because the scanner forgets a socket that misses
    /// two probes and builds a fresh stream when the port answers again, and the port then
    /// re-sends the head it already pushed (ADR 0048 §16: one run per new head).
    heads: BTreeMap<PathBuf, SeenHead>,
}

/// The last pending head the `after_pending` hook ran for on one socket.
struct SeenHead {
    /// The session the socket reported. A socket that now hosts another session is another
    /// process's queue, whose `request_id`s start over.
    session: String,
    /// The head's `request_id`.
    request: u64,
}

impl SessionHub {
    /// A hub that has not scanned anything yet, with no `[hub.hooks]` commands.
    #[must_use]
    pub fn new(home: PathBuf) -> Arc<Self> {
        Self::with_hooks(home, HubHooks::default())
    }

    /// A hub that runs `hooks` for every new pending head of a live session.
    #[must_use]
    pub fn with_hooks(home: PathBuf, hooks: HubHooks) -> Arc<Self> {
        Arc::new(Self {
            home,
            hooks,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Starts the scanner on a thread of its own. The thread ends when the process does; the
    /// hub has no stop path of its own (a signal ends the whole process).
    pub fn start(self: &Arc<Self>) {
        let hub = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("hub-sessions".to_owned())
            .spawn(move || scan(&hub));
    }

    /// The run directory: `$RADIAN_HOME/run`.
    #[must_use]
    pub fn run_dir(&self) -> PathBuf {
        radian_host::run_dir_for(&self.home)
    }

    /// The hub token, re-read for each connection so a rotation takes effect at the next one.
    /// `None` when the file cannot be read or is empty (the port then refuses the owner verbs).
    fn token(&self) -> Option<String> {
        let text = fs::read_to_string(radian_host::hub_dir_for(&self.home).join("token")).ok()?;
        let token = text.trim();
        (!token.is_empty()).then(|| token.to_owned())
    }

    /// The live sessions, in socket order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(PathBuf, LiveSession)> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .live
            .iter()
            .map(|(socket, live)| (socket.clone(), live.clone()))
            .collect()
    }

    /// The live session with `id`, with the socket that hosts it.
    #[must_use]
    pub fn live(&self, id: &str) -> Option<(PathBuf, LiveSession)> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .live
            .iter()
            .find(|(_, live)| live.id == id)
            .map(|(socket, live)| (socket.clone(), live.clone()))
    }

    /// The registered repository whose path is the session's workspace, with its registry name.
    /// A workspace that is not registered has none, and no hook runs for it.
    fn repo_for(&self, workspace: &str) -> Option<(String, PathBuf)> {
        let registry = crate::hub::Registry::at(radian_host::hub_dir_for(&self.home));
        let repos = registry.repos().ok()?;
        repos
            .into_iter()
            .find(|entry| {
                radian_host::sessions::same_directory(workspace, &entry.path.to_string_lossy())
            })
            .map(|entry| (entry.name, entry.path))
    }

    /// Runs the `[hub.hooks] after_pending` commands for a new head, off this thread. A session
    /// whose workspace is not registered has no repository name and runs nothing.
    fn fire(&self, workspace: &str, session: &str, head: &PendingHead) {
        let Some((repo, root)) = self.repo_for(workspace) else {
            return;
        };
        hooks::fire(&self.home, &self.hooks, &root, &repo, session, head);
    }

    /// Whether `request_id` is a head the `after_pending` hook has not run for on `socket` yet,
    /// which makes it a new one; records it when it is. The memory outlives a forgotten socket,
    /// so the port's re-sent head after a recovery is not new, and it starts over for another
    /// socket (another process, whose `request_id`s begin at 1 again) or another session id.
    fn claim_head(&self, socket: &Path, session: &str, request_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let new = inner
            .heads
            .get(socket)
            .is_none_or(|seen| seen.session != session || seen.request != request_id);
        if new {
            inner.heads.insert(
                socket.to_path_buf(),
                SeenHead {
                    session: session.to_owned(),
                    request: request_id,
                },
            );
        }
        new
    }

    /// Forgets the head memory of sockets whose pid is gone, so it does not grow with every
    /// process that ever reported here. A socket whose process is still alive keeps its memory
    /// while the scanner has forgotten the session: that is what a recovery needs.
    fn forget_dead_heads(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .heads
            .retain(|socket, _| socket_pid(socket).is_some_and(process::alive));
    }

    /// The event fan-out of the live session `id`, or `None` when no process hosts it.
    #[must_use]
    pub fn channel(&self, id: &str) -> Option<broadcast::Sender<Bytes>> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.events.get(id).cloned()
    }

    /// Records what `socket`'s port said and returns whether this is a new session for that
    /// socket (a first sight, or a different id than the one recorded), which is what makes the
    /// scanner start a subscription.
    fn record(&self, socket: &Path, live: LiveSession) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let previous = inner.live.get(socket).map(|known| known.id.clone());
        let new = previous.as_deref() != Some(live.id.as_str());
        if new {
            // A socket that now reports another session stops feeding the old one's stream.
            if let Some(previous) = previous {
                inner.events.remove(&previous);
            }
            inner
                .events
                .entry(live.id.clone())
                .or_insert_with(|| broadcast::channel(EVENT_QUEUE).0);
        }
        inner.live.insert(socket.to_path_buf(), live);
        new
    }

    /// Forgets the session a socket reported: it leaves every list, and its event stream ends.
    fn forget(&self, socket: &Path) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(live) = inner.live.remove(socket) {
            inner.events.remove(&live.id);
        }
    }

    /// The sockets in the run directory whose file name is a live pid's, in name order. A file
    /// that is not `<pid>.sock`, or whose pid is gone, is not a session (ADR 0043: the socket is
    /// removed on exit, and a `kill -9` leaves one behind).
    fn sockets(&self) -> Vec<PathBuf> {
        let dir = self.run_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut sockets: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| socket_pid(path).is_some_and(process::alive))
            .collect();
        sockets.sort();
        sockets
    }
}

/// The pid a socket's file name names, when the file is `<pid>.sock`.
fn socket_pid(path: &Path) -> Option<u32> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("sock") {
        return None;
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<u32>().ok())
}

/// One subscription thread and the flag that stops it.
struct Subscription {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Subscription {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How many scans in a row a socket may fail its probe while its pid is alive before the hub
/// forgets the session. One failed probe is a process too busy to answer within the timeout; a
/// session that is really gone fails its pid check and leaves in one scan.
const PROBE_FAILURES: u8 = 2;

/// The scanner: read the run directory, probe every untracked socket, keep one subscription per
/// live session, and forget the sockets that are gone. Runs on its own thread for the hub's
/// whole life.
fn scan(hub: &Arc<SessionHub>) {
    let mut tracked: BTreeMap<PathBuf, Subscription> = BTreeMap::new();
    let mut failures: BTreeMap<PathBuf, u8> = BTreeMap::new();
    loop {
        // A pid that is gone cannot report again; its memory would only grow the map.
        hub.forget_dead_heads();
        let sockets = hub.sockets();
        tracked.retain(|socket, subscription| {
            if sockets.contains(socket) {
                return true;
            }
            subscription.stop();
            failures.remove(socket);
            hub.forget(socket);
            false
        });
        for socket in sockets {
            match probe(&socket) {
                Ok(Some(live)) => {
                    failures.remove(&socket);
                    let new = hub.record(&socket, live.clone());
                    if new {
                        if let Some(mut old) = tracked.remove(&socket) {
                            old.stop();
                        }
                        tracked.insert(socket.clone(), subscribe(Arc::clone(hub), socket, live));
                    }
                }
                Ok(None) | Err(_) => {
                    let count = failures.entry(socket.clone()).or_insert(0);
                    *count = count.saturating_add(1);
                    if *count < PROBE_FAILURES {
                        continue;
                    }
                    failures.remove(&socket);
                    if let Some(mut old) = tracked.remove(&socket) {
                        old.stop();
                    }
                    hub.forget(&socket);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(SCAN_INTERVAL_SECS));
    }
}

/// `list_sessions` on one socket: the session it hosts, or `None` when the port refuses or
/// reports no `main` row. Every read is bounded by [`PROBE_TIMEOUT`] so a socket that accepts and
/// never answers costs one timeout, not the scan.
fn probe(socket: &Path) -> Result<Option<LiveSession>, String> {
    let response = request(
        socket,
        &json!({ "id": 1, "verb": "list_sessions" }),
        PROBE_TIMEOUT,
        None,
    )?;
    let Some(rows) = response.get("sessions").and_then(Value::as_array) else {
        return Ok(None);
    };
    for row in rows {
        if row.get("kind").and_then(Value::as_str) != Some("main") {
            continue;
        }
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(workspace) = row.get("workspace").and_then(Value::as_str) else {
            continue;
        };
        let model = row.get("model");
        return Ok(Some(LiveSession {
            id: id.to_owned(),
            workspace: workspace.to_owned(),
            state: row
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("idle")
                .to_owned(),
            pending: row.get("pending").and_then(Value::as_u64).unwrap_or(0),
            provider: model
                .and_then(|model| model.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            model: model
                .and_then(|model| model.get("model"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        }));
    }
    Ok(None)
}

/// One request and its answer line on a fresh connection, within `timeout`. `token` is the
/// owner token the owner verbs need; the probe and the subscription use it too, so the hub sees
/// the full facts.
fn request(
    socket: &Path,
    request: &Value,
    timeout: Duration,
    token: Option<&str>,
) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let mut request = request.clone();
    if let Some(token) = token
        && let Some(object) = request.as_object_mut()
    {
        object.insert("token".to_owned(), json!(token));
    }
    let mut line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|error| format!("cannot write to {}: {error}", socket.display()))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut answer = String::new();
    reader
        .read_line(&mut answer)
        .map_err(|error| format!("cannot read from {}: {error}", socket.display()))?;
    let value: Value = serde_json::from_str(answer.trim())
        .map_err(|error| format!("{} answered no JSON: {error}", socket.display()))?;
    if value.get("ok") == Some(&Value::Bool(true)) {
        return Ok(value);
    }
    Err(value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the control port refused the request")
        .to_owned())
}

/// The `models` route's answer: the control port's own list (ADR 0026 D5).
///
/// # Errors
///
/// [`SessionError::NotLive`] when no process hosts the session, [`SessionError::Port`] when its
/// port fails.
pub fn models(hub: &SessionHub, id: &str) -> Result<Value, SessionError> {
    let Some((socket, _)) = hub.live(id) else {
        return Err(SessionError::NotLive(format!(
            "session {id:?} is not live: no process hosts it"
        )));
    };
    let token = hub.token();
    match request(
        &socket,
        &json!({ "id": 1, "verb": "models" }),
        REQUEST_TIMEOUT,
        token.as_deref(),
    ) {
        Ok(answer) => Ok(json!({
            "models": answer.get("models").cloned().unwrap_or(Value::Array(Vec::new())),
        })),
        Err(error) => Err(SessionError::Port(error)),
    }
}

// ---- the route bodies ---------------------------------------------------------------------

/// The `sessions` route body for the repository at `repo` (ADR 0048 decision 16): every session
/// of that workspace — the live ones the scanner found and the ended ones the session index
/// lists — newest activity first.
///
/// # Errors
///
/// The session directory, its index or the stored rows cannot be read.
pub fn sessions_body(hub: &SessionHub, repo: &Path) -> Result<Value, SessionError> {
    let workspace = repo.to_string_lossy().into_owned();
    let dir = radian_host::session_dir(None).map_err(SessionError::Failed)?;
    let rows = match radian_host::sessions::open_index(&dir).map_err(SessionError::Failed)? {
        Some(index) => {
            radian_host::sessions::list(&index, &workspace).map_err(SessionError::Failed)?
        }
        // The session directory does not exist yet: nothing was ever created.
        None => Vec::new(),
    };
    let live: Vec<LiveSession> = hub
        .snapshot()
        .into_iter()
        .map(|(_, live)| live)
        .filter(|live| live.in_repo(repo))
        .collect();
    // One row per id, newest activity first; a live session the index has not caught up with is
    // still listed (its journal may be being written right now), with the facts no index row
    // carries.
    let mut out: Vec<(Option<String>, String, Value)> = Vec::with_capacity(rows.len());
    for row in rows {
        let current = live.iter().find(|live| live.id == row.session_id);
        out.push((
            row.last_ts.clone(),
            row.session_id.clone(),
            json!({
                "id": row.session_id,
                "created": row.created_ts,
                "last_activity": row.last_ts,
                "title": (!row.summary.is_empty()).then_some(row.summary),
                "live": if current.is_some() { "tui" } else { "none" },
                "state": current.map_or("idle", |live| live.state.as_str()),
                "pending": current.map_or(0, |live| live.pending),
                "model": current.map(LiveSession::route),
            }),
        ));
    }
    for current in live {
        if out.iter().any(|(_, id, _)| *id == current.id) {
            continue;
        }
        out.push((
            None,
            current.id.clone(),
            json!({
                "id": current.id,
                "created": Value::Null,
                "last_activity": Value::Null,
                "title": Value::Null,
                "live": "tui",
                "state": current.state,
                "pending": current.pending,
                "model": current.route(),
            }),
        ));
    }
    out.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Ok(json!({
        "sessions": out.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
    }))
}

/// The journal of session `id` when it belongs to the repository at `repo`, or the refusal the
/// route answers with. The id is validated first (`../x` cannot name a file outside the session
/// directory), then the recorded workspace is compared with the repository's path.
///
/// # Errors
///
/// [`SessionError::Unknown`] for an id that names no session of this repository.
pub fn journal_for(repo: &Path, id: &str) -> Result<PathBuf, SessionError> {
    radian_host::validate_session_id(id).map_err(SessionError::Unknown)?;
    let dir = radian_host::session_dir(None).map_err(SessionError::Failed)?;
    let path = dir.join(format!("{id}.jsonl"));
    if !path.is_file() {
        return Err(SessionError::Unknown(format!(
            "no session {id:?} in {}",
            dir.display()
        )));
    }
    let workspace = repo.to_string_lossy();
    match radian_host::sessions::file_workspace(&dir, id) {
        Some(recorded) => {
            if let Some(refusal) =
                radian_host::sessions::workspace_mismatch(id, &recorded, &workspace)
            {
                return Err(SessionError::Unknown(refusal));
            }
        }
        None => {
            return Err(SessionError::Unknown(format!(
                "no session {id:?} in {}",
                dir.display()
            )));
        }
    }
    Ok(path)
}

/// The `transcript` route body: the journal folded into the shared transcript entries, by the
/// same fold `radian attach` uses (ADR 0023 U1).
///
/// # Errors
///
/// [`SessionError::Unknown`] for an unknown session, [`SessionError::Failed`] for a journal that
/// cannot be read.
pub fn transcript_body(repo: &Path, id: &str) -> Result<Value, SessionError> {
    let path = journal_for(repo, id)?;
    let mut fold = AttachFold::open(&path, radian_host::memory_dir())
        .map_err(|error| SessionError::Failed(format!("cannot read session {id:?}: {error}")))?;
    let _ = fold.poll();
    Ok(json!({
        "session": id,
        "entries": fold.entries(),
    }))
}

/// The `conversation` route body: one attempt's `result.jsonl` folded with the ACP fold, the
/// same entries the arc TUI's conversation page shows. `attempt` selects one attempt by number;
/// `None` takes the newest. A task with no attempt is an empty list with `attempt` `null`.
///
/// # Errors
///
/// [`SessionError::Unknown`] for an unknown task or attempt number, [`SessionError::Failed`] for
/// an unreadable attempts directory.
pub fn conversation_body(
    store: &Store,
    task: &str,
    attempt: Option<u32>,
) -> Result<Value, SessionError> {
    let attempts = crate::read::task_attempts(store, task)
        .map_err(|error| SessionError::Failed(error.message()))?;
    let chosen = match attempt {
        Some(number) => match attempts.iter().find(|view| view.attempt == number) {
            Some(view) => Some(view.attempt),
            None => {
                return Err(SessionError::Unknown(format!(
                    "no attempt {number} for {task}"
                )));
            }
        },
        None => attempts.last().map(|view| view.attempt),
    };
    let Some(number) = chosen else {
        return Ok(json!({ "task": task, "attempt": Value::Null, "entries": [] }));
    };
    let path = crate::store::attempt_ops::run_dir(store, task, number).join("result.jsonl");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        // A worker that never wrote its log is an empty conversation, not a failure.
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(SessionError::Failed(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    let mut fold = AcpFold::new();
    for line in text.lines() {
        fold.push_line(line);
    }
    Ok(json!({
        "task": task,
        "attempt": number,
        "entries": fold.entries(),
    }))
}

// ---- the subscription ---------------------------------------------------------------------

/// One long-lived `subscribe` connection: it feeds the session's event stream, reconnects when it
/// drops, and ends when the socket is forgotten. Returns the handle the scanner keeps.
fn subscribe(hub: Arc<SessionHub>, socket: PathBuf, live: LiveSession) -> Subscription {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name(format!("hub-subscribe-{}", live.id))
        .spawn(move || {
            let journal = radian_host::session_dir(None)
                .map(|dir| dir.join(format!("{}.jsonl", live.id)))
                .unwrap_or_default();
            let mut stream = Stream::new(Arc::clone(&hub), socket.clone(), &live, journal);
            follow(&mut stream, &flag, || connect(&socket, &hub, &flag));
        })
        .ok();
    Subscription { stop, handle }
}

/// The subscription's frames: one state machine per connection.
struct Stream {
    hub: Arc<SessionHub>,
    /// The control socket the session's port listens on: the identity of the process whose
    /// heads the hook has already seen.
    socket: PathBuf,
    id: String,
    /// The workspace the port reported, matched against the registry when a head fires.
    workspace: String,
    journal: PathBuf,
    /// The memory root a settled call's card resolves against; `None` in a process with none.
    memory_root: Option<PathBuf>,
    fold: Option<AttachFold>,
    /// The fold's state at the last frame sent, so a no-op read sends nothing.
    sent_state: Option<&'static str>,
    pending: u64,
}

impl Stream {
    fn new(hub: Arc<SessionHub>, socket: PathBuf, live: &LiveSession, journal: PathBuf) -> Self {
        let mut stream = Self {
            hub,
            socket,
            id: live.id.clone(),
            workspace: live.workspace.clone(),
            journal,
            memory_root: radian_host::memory_dir(),
            fold: None,
            sent_state: None,
            pending: live.pending,
        };
        // The fold opens with the subscription, so the baseline it drops is the transcript as
        // the session stood when the hub began following it.
        let _ = stream.fold_mut();
        stream
    }

    /// The fold, opened on first use so a session whose journal is not written yet costs nothing.
    /// The entries the fold opens with are the client's baseline — it read the transcript before
    /// it followed the stream — so that first change mark is dropped rather than re-sent.
    fn fold_mut(&mut self) -> Option<&mut AttachFold> {
        if self.fold.is_none() {
            self.fold = AttachFold::open(&self.journal, self.memory_root.clone()).ok();
            if let Some(fold) = self.fold.as_mut() {
                let _ = fold.take_change();
            }
        }
        self.fold.as_mut()
    }

    /// One subscribed event line, as the frames it produces.
    fn on_line(&mut self, line: &str) -> Vec<Bytes> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let Some(event) = value.get("event") else {
            return Vec::new();
        };
        let source = event.get("source").and_then(Value::as_str).unwrap_or("");
        let name = event.get("name").and_then(Value::as_str).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        let mut frames = Vec::new();
        match (source, name) {
            ("control", "pending") => {
                self.pending = data
                    .get("length")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.pending);
                if let Some(head) = self.new_pending_head(&data) {
                    self.hub.fire(&self.workspace, &self.id, &head);
                }
                frames.push(frame("pending", &data));
            }
            // The port fans a `RuntimeEvent` out under `event.name()`, the v1 disk name
            // (ADR 0006 §4) — `ConfigChanged`, not a lowercase spelling.
            ("runtime", "ConfigChanged") => frames.push(frame("config", &data)),
            _ => {}
        }
        frames.extend(self.poll_fold());
        frames.extend(self.state_frame());
        frames
    }

    /// The head this pending event carries, when it is one the `after_pending` hook has not run
    /// for. A `null` head (an emptied queue) is never one, and the same `request_id` seen again —
    /// after a reconnect, or because the queue behind the head changed, or because the scanner
    /// forgot the socket and found it again — is not a new head.
    fn new_pending_head(&mut self, data: &Value) -> Option<PendingHead> {
        let head = data.get("head")?;
        if head.is_null() {
            return None;
        }
        let request_id = head.get("request_id").and_then(Value::as_u64)?;
        if !self.hub.claim_head(&self.socket, &self.id, request_id) {
            return None;
        }
        Some(PendingHead {
            kind: head
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            tool: head
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// The journal's entries that are new or changed since the last read, one frame each. The
    /// journal is the authority for the stream, exactly as it is for the transcript route.
    fn poll_fold(&mut self) -> Vec<Bytes> {
        let Some(fold) = self.fold_mut() else {
            return Vec::new();
        };
        if fold.poll().is_err() {
            return Vec::new();
        }
        let Some(change) = fold.take_change() else {
            return Vec::new();
        };
        let entries = fold.entries();
        let from = change.from.min(entries.len());
        entries[from..]
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let index = from + offset;
                frame(
                    "entry",
                    &json!({
                        "index": index,
                        "reset": change.reset && offset == 0,
                        "entry": entry,
                    }),
                )
            })
            .collect()
    }

    /// The turn state, when it differs from the last one sent. The hub's own view has the
    /// queue's length and the journal's open turn; the trust modal, the wake block and the
    /// background-job count are the process's own facts and stay on the `sessions` route, which
    /// asks the port.
    fn state_frame(&mut self) -> Option<Bytes> {
        let running = self.fold.as_ref().is_some_and(AttachFold::running);
        let state = if self.pending > 0 {
            "blocked"
        } else if running {
            "working"
        } else {
            "idle"
        };
        if self.sent_state == Some(state) {
            return None;
        }
        self.sent_state = Some(state);
        Some(frame("state", &json!({ "state": state })))
    }

    /// Hands every frame to the session's subscribers.
    fn send(&self, frames: Vec<Bytes>) {
        let Some(sender) = self.hub.channel(&self.id) else {
            return;
        };
        for frame in frames {
            let _ = sender.send(frame);
        }
    }
}

/// The subscription's loop: follow each connection `connect` returns, read its event lines, and
/// reconnect when one drops, until the socket is forgotten. A connection is a reader and the
/// bytes a line read may already hold.
fn follow(
    stream: &mut Stream,
    stop: &Arc<AtomicBool>,
    mut connect: impl FnMut() -> Result<(std::io::BufReader<UnixStream>, Vec<u8>), String>,
) {
    while !stop.load(Ordering::SeqCst) {
        if let Ok((mut reader, mut partial)) = connect() {
            // The journal may have grown while the connection was down: emit what the
            // subscribers missed before following the live events.
            let frames = stream.poll_fold();
            stream.send(frames);
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                match reader.read_until(b'\n', &mut partial) {
                    Ok(0) => break,
                    Ok(_) => {
                        // `read_until` only stops without a delimiter at EOF. A torn last line
                        // is not an event: reconnect and read the journal again.
                        if partial.last() != Some(&b'\n') {
                            break;
                        }
                        let text = String::from_utf8_lossy(&partial).trim().to_owned();
                        partial.clear();
                        let frames = stream.on_line(&text);
                        stream.send(frames);
                    }
                    // A read timeout: nothing yet, and a chance to notice the stop flag. The
                    // partial line stays where it is for the next read.
                    Err(error)
                        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }
        if !stop.load(Ordering::SeqCst) {
            std::thread::sleep(RECONNECT_PAUSE);
        }
    }
}

/// Connects and subscribes, returning the connection once the port acknowledged the
/// subscription and the bytes a line read may already hold. A connect that fails is the
/// caller's pause-and-retry. The acknowledgement is read one line at a time because the port
/// answers the request and starts pushing events on the same stream: a buffered read could keep
/// bytes a dropped reader swallowed.
fn connect(
    socket: &Path,
    hub: &SessionHub,
    stop: &Arc<AtomicBool>,
) -> Result<(std::io::BufReader<UnixStream>, Vec<u8>), String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(READ_POLL));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let mut request = json!({ "id": 1, "verb": "subscribe" });
    if let Some(token) = hub.token()
        && let Some(object) = request.as_object_mut()
    {
        object.insert("token".to_owned(), json!(token));
    }
    let mut line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|error| format!("cannot write to {}: {error}", socket.display()))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut answer: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("the subscription was forgotten".to_owned());
        }
        match reader.read_until(b'\n', &mut answer) {
            Ok(0) => return Err(format!("{} closed the subscription", socket.display())),
            Ok(_) => {
                if answer.last() != Some(&b'\n') {
                    return Err(format!("{} closed the subscription", socket.display()));
                }
                break;
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(error) => {
                return Err(format!("cannot read from {}: {error}", socket.display()));
            }
        }
    }
    let text = String::from_utf8_lossy(&answer);
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| format!("{} answered no JSON: {error}", socket.display()))?;
    if value.get("ok") != Some(&Value::Bool(true)) {
        return Err(format!(
            "{} refused the subscription: {}",
            socket.display(),
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no reason")
        ));
    }
    Ok((reader, Vec::new()))
}

/// One event-stream frame.
fn frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One live session row for the frame tests.
    fn live_row(id: &str, pending: u64) -> LiveSession {
        LiveSession {
            id: id.to_owned(),
            workspace: String::new(),
            state: "idle".to_owned(),
            pending,
            provider: "p".to_owned(),
            model: "m".to_owned(),
        }
    }

    /// The frames as `(event name, data)`, decoded from their SSE text.
    fn frames(bytes: &[Bytes]) -> Vec<(String, Value)> {
        bytes
            .iter()
            .map(|frame| {
                let text = std::str::from_utf8(frame).expect("a frame is UTF-8");
                let mut lines = text.lines();
                let name = lines
                    .next()
                    .and_then(|line| line.strip_prefix("event: "))
                    .expect("a frame starts with its event name")
                    .to_owned();
                let data = lines
                    .next()
                    .and_then(|line| line.strip_prefix("data: "))
                    .expect("a frame carries its data");
                (
                    name,
                    serde_json::from_str(data).expect("the frame's data is JSON"),
                )
            })
            .collect()
    }

    #[test]
    fn a_live_session_matches_a_repository_through_another_path_spelling() {
        let dir = std::env::temp_dir().join(format!("radian-hub-sessions-{}", std::process::id()));
        let real = dir.join("real").join("repo");
        fs::create_dir_all(&real).unwrap();
        let link = dir.join("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();
        let live = LiveSession {
            id: "s".to_owned(),
            workspace: link.join("repo").to_string_lossy().into_owned(),
            state: "idle".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
        };
        assert!(
            live.in_repo(&real),
            "{} and {} are the same directory",
            live.workspace,
            real.display()
        );
        assert!(!live.in_repo(&dir.join("other")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_socket_of_a_dead_pid_is_not_a_session() {
        let dir = std::env::temp_dir().join(format!("radian-hub-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let run = radian_host::run_dir_for(&dir);
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("2147483647.sock"), b"").unwrap();
        fs::write(run.join("notes.txt"), b"").unwrap();
        let hub = SessionHub::new(dir.clone());
        assert!(hub.sockets().is_empty(), "{:?}", hub.sockets());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Mutation: fire on every pending event instead of a new head (drop the `request_id`
    /// comparison). The second call with the same head then returns a head again.
    #[test]
    fn a_repeated_pending_head_is_not_new() {
        let dir = crate::test_support::TempDir::new("hub-sessions-head");
        let hub = SessionHub::new(dir.path().to_path_buf());
        let mut stream = Stream::new(
            Arc::clone(&hub),
            dir.path().join("run/1.sock"),
            &live_row("s", 0),
            dir.path().join("s.jsonl"),
        );
        let approval = json!({
            "head": {"request_id": 5, "kind": "approval", "tool": "shell"},
            "length": 1,
        });
        let head = stream
            .new_pending_head(&approval)
            .expect("a first head is new");
        assert_eq!(head.kind, "approval");
        assert_eq!(head.tool, "shell");
        assert!(
            stream.new_pending_head(&approval).is_none(),
            "the same request id is not a new head"
        );
        assert!(
            stream
                .new_pending_head(&json!({"head": null, "length": 0}))
                .is_none(),
            "an emptied queue is no head"
        );
        let question = json!({
            "head": {"request_id": 6, "kind": "question", "question": {"questions": []}},
            "length": 1,
        });
        let head = stream
            .new_pending_head(&question)
            .expect("a later request id is new");
        assert_eq!(head.kind, "question");
        assert_eq!(head.tool, "", "a question carries no tool");
    }

    /// Review: a socket the scanner forgot and found again keeps the head memory, so the port's
    /// re-sent head does not run the hook twice (ADR 0048 §16: one run per new head).
    #[test]
    fn a_forgotten_socket_does_not_fire_the_same_head_again() {
        let dir = crate::test_support::TempDir::new("hub-sessions-forgotten-head");
        let socket = dir.path().join("run/1.sock");
        let hub = SessionHub::new(dir.path().to_path_buf());
        let live = live_row("s", 0);
        assert!(hub.record(&socket, live.clone()));
        let head = json!({
            "head": { "request_id": 1, "kind": "approval", "tool": "shell" },
            "length": 1,
        });
        let journal = dir.path().join("s.jsonl");
        let mut first = Stream::new(Arc::clone(&hub), socket.clone(), &live, journal.clone());
        assert!(
            first.new_pending_head(&head).is_some(),
            "a first sight is a new head"
        );

        // The port missed two `list_sessions` probes, so the scanner stopped the subscription
        // and forgot the session; it answers again, `record` reports it as new, and `subscribe`
        // builds a fresh `Stream`. The port then re-sends the head it already pushed.
        hub.forget(&socket);
        assert!(
            hub.record(&socket, live.clone()),
            "the recovered socket is new again"
        );
        let mut recovered = Stream::new(Arc::clone(&hub), socket, &live, journal);
        assert!(
            recovered.new_pending_head(&head).is_none(),
            "the head the hook already ran for is not new on the recovered socket"
        );

        // Another process (another socket path) starts its `request_id`s over: the same id is a
        // new head there.
        let other = dir.path().join("run/2.sock");
        let mut elsewhere = Stream::new(Arc::clone(&hub), other, &live, dir.path().join("s.jsonl"));
        assert!(
            elsewhere.new_pending_head(&head).is_some(),
            "a new socket's request ids start over"
        );
    }

    /// Edge 1/2/3/5 through the subscription: a new head runs the hook with the four values,
    /// the same head seen again does not, a question sets an empty tool, and an emptied queue
    /// runs nothing. The socket tests of `tests/hub_sessions.rs` need a `/tmp` path; this one
    /// uses the workspace temp tree and runs inside the sandbox.
    #[test]
    fn a_new_pending_head_runs_the_configured_hook_once() {
        let dir = crate::test_support::TempDir::new("hub-sessions-hook");
        let home = dir.path().to_path_buf();
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".radian/arc")).unwrap();
        let config = crate::store::config::default_text("main")
            .replace("command_timeout_secs = 3600", "command_timeout_secs = 5");
        std::fs::write(repo.join(".radian/arc/config.toml"), config).unwrap();
        let registry = crate::hub::Registry::at(radian_host::hub_dir_for(&home));
        registry
            .add(&repo, Some("repo"))
            .expect("the repository registers");
        let runs = repo.join("runs.txt");
        let command = format!(
            "printf '%s|%s|%s|%s\\n' \"$RADIAN_HUB_REPO\" \"$RADIAN_HUB_SESSION\" \
             \"$RADIAN_HUB_PENDING_KIND\" \"$RADIAN_HUB_TOOL\" >> '{}'",
            runs.display()
        );
        let hub = SessionHub::with_hooks(
            home.clone(),
            HubHooks {
                after_pending: vec![command],
            },
        );
        let live = LiveSession {
            id: "s".to_owned(),
            workspace: std::fs::canonicalize(&repo)
                .expect("the repository canonicalizes")
                .to_string_lossy()
                .into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
        };
        let mut stream = Stream::new(
            Arc::clone(&hub),
            home.join("run/1.sock"),
            &live,
            home.join("s.jsonl"),
        );
        let line = |head: Value, length: u64| {
            json!({
                "event": { "source": "control", "name": "pending",
                           "data": { "head": head, "length": length } },
            })
            .to_string()
        };
        let wait = |count: usize| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let lines: Vec<String> = std::fs::read_to_string(&runs)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_owned)
                    .collect();
                if lines.len() >= count {
                    return lines;
                }
                assert!(std::time::Instant::now() < deadline, "the hook never ran");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        // Edge 1: a new approval head, once.
        stream.on_line(&line(
            json!({ "request_id": 1, "kind": "approval", "tool": "shell",
                    "arguments": { "command": "secret" } }),
            1,
        ));
        let lines = wait(1);
        assert_eq!(lines[0], "repo|s|approval|shell");
        // Edge 2: the same head again is not a new one.
        stream.on_line(&line(
            json!({ "request_id": 1, "kind": "approval", "tool": "shell" }),
            1,
        ));
        // Edge 5: the queue empties; nothing runs.
        stream.on_line(&line(Value::Null, 0));
        // Edge 3: a question is a new head with an empty tool.
        stream.on_line(&line(
            json!({ "request_id": 2, "kind": "question",
                    "question": { "questions": [{ "question": "secret" }] } }),
            1,
        ));
        wait(2);
        std::thread::sleep(Duration::from_millis(300));
        let lines: Vec<String> = std::fs::read_to_string(&runs)
            .expect("the hook wrote its runs")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines,
            vec![
                "repo|s|approval|shell".to_owned(),
                "repo|s|question|".to_owned()
            ]
        );
    }

    #[test]
    fn a_pending_event_becomes_one_pending_frame_and_a_blocked_state() {
        let dir = crate::test_support::TempDir::new("hub-sessions-pending");
        let hub = SessionHub::new(dir.path().to_path_buf());
        let mut stream = Stream::new(
            Arc::clone(&hub),
            dir.path().join("run/1.sock"),
            &live_row("s", 0),
            dir.path().join("s.jsonl"),
        );
        let out = stream.on_line(
            r#"{"event":{"source":"control","name":"pending","data":{"head":null,"length":2}}}"#,
        );
        let decoded = frames(&out);
        assert_eq!(decoded[0].0, "pending");
        assert_eq!(decoded[0].1, json!({ "head": null, "length": 2 }));
        let state = decoded
            .iter()
            .find(|(name, _)| name == "state")
            .expect("the queue's length is a blocked state");
        assert_eq!(state.1, json!({ "state": "blocked" }));
    }

    #[test]
    fn a_committed_message_becomes_one_entry_frame() {
        let dir = crate::test_support::TempDir::new("hub-sessions-entry");
        let journal = dir.path().join("s.jsonl");
        let line = json!({
            "schema_version": 1,
            "session_id": "s",
            "seq": 0,
            "ts": "2026-11-02T00:00:00.000000Z",
            "event": "SessionCreated",
            "critical": true,
            "data": { "workspace": "/w", "config_snapshot": {} },
        });
        fs::write(&journal, format!("{line}\n")).unwrap();
        // A live session holds the journal's writer lock; the fold follows it incrementally
        // instead of rebuilding the whole history on every read.
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal)
            .unwrap();
        fs2::FileExt::lock_exclusive(&writer).expect("the journal's writer lock is free");
        let hub = SessionHub::new(dir.path().to_path_buf());
        let mut stream = Stream::new(
            Arc::clone(&hub),
            dir.path().join("run/1.sock"),
            &live_row("s", 0),
            journal.clone(),
        );
        let mut file = fs::OpenOptions::new().append(true).open(&journal).unwrap();
        let appended = json!({
            "schema_version": 1,
            "session_id": "s",
            "seq": 1,
            "ts": "2026-11-02T00:00:01.000000Z",
            "event": "MessageAppended",
            "critical": true,
            "data": { "index": 0, "message": { "user": { "content": [{ "text": { "text": "hi" } }] } } },
        });
        file.write_all(format!("{appended}\n").as_bytes()).unwrap();
        file.flush().unwrap();
        let out = stream.on_line(
            r#"{"event":{"source":"agent","name":"transcript_appended","data":{"index":0}}}"#,
        );
        let decoded = frames(&out);
        let entry = decoded
            .iter()
            .find(|(name, _)| name == "entry")
            .expect("the commit is one entry frame");
        assert_eq!(entry.1["index"], json!(0));
        assert_eq!(entry.1["reset"], json!(false));
        assert_eq!(entry.1["entry"]["kind"], json!("user"));
        assert_eq!(entry.1["entry"]["text"], json!("hi"));
        fs2::FileExt::unlock(&writer).expect("the writer lock is released");
    }

    /// The next frame on the session's fan-out, within `within`.
    fn next_frame(receiver: &mut broadcast::Receiver<Bytes>, within: Duration) -> Option<Bytes> {
        let deadline = std::time::Instant::now() + within;
        loop {
            match receiver.try_recv() {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(broadcast::error::TryRecvError::Closed) => return None,
            }
        }
    }

    /// The next frame whose text contains `needle`, within `within`.
    fn next_frame_containing(
        receiver: &mut broadcast::Receiver<Bytes>,
        needle: &str,
        within: Duration,
    ) -> Option<Bytes> {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            let frame = next_frame(
                receiver,
                deadline.saturating_duration_since(std::time::Instant::now()),
            )?;
            if String::from_utf8_lossy(&frame).contains(needle) {
                return Some(frame);
            }
        }
        None
    }

    /// Mutation: drop the loop in [`follow`] (follow one connection only). The second
    /// connection's event then never becomes a frame.
    #[test]
    fn the_subscription_reconnects_after_the_connection_drops() {
        let dir = crate::test_support::TempDir::new("hub-sessions-follow");
        let hub = SessionHub::new(dir.path().to_path_buf());
        let live = live_row("s", 0);
        // The channel a session's subscribers read; the scanner records it for every live
        // session before it starts the subscription.
        let socket = PathBuf::from("/run/s.sock");
        assert!(hub.record(&socket, live.clone()));
        let mut receiver = hub
            .channel("s")
            .expect("a live session has a fan-out")
            .subscribe();
        let mut stream = Stream::new(Arc::clone(&hub), socket, &live, dir.path().join("s.jsonl"));
        // Two connections: the first is closed by the test, the second carries the event. The
        // read timeout is what `connect` sets on a real socket, so the loop wakes to see `stop`.
        let (first, mut peer_one) = UnixStream::pair().expect("a socket pair");
        let (second, mut peer_two) = UnixStream::pair().expect("a socket pair");
        for connection in [&first, &second] {
            connection
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("a read timeout is settable");
        }
        let mut connections = vec![first, second].into_iter();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            follow(&mut stream, &flag, || {
                connections.next().map_or_else(
                    || Err("no more connections".to_owned()),
                    |connection| Ok((std::io::BufReader::new(connection), Vec::new())),
                )
            });
        });

        let event = |revision: u64| {
            format!(
                "{}\n",
                json!({"event": {"source": "runtime", "name": "ConfigChanged",
                       "data": {"revision": revision, "change": {}}}})
            )
        };
        peer_one
            .write_all(event(1).as_bytes())
            .expect("the first connection's event is written");
        let frame = next_frame_containing(&mut receiver, "event: config", Duration::from_secs(5))
            .expect("the first connection's event becomes a frame");
        assert!(String::from_utf8_lossy(&frame).contains("event: config"));
        // The connection drops; the loop must open the second one.
        drop(peer_one);
        peer_two
            .write_all(event(2).as_bytes())
            .expect("the second connection's event is written");
        let frame = next_frame_containing(&mut receiver, "\"revision\":2", Duration::from_secs(5))
            .expect("the reconnected connection's event becomes a frame");
        assert!(
            String::from_utf8_lossy(&frame).contains("\"revision\":2"),
            "the frame came from the second connection: {:?}",
            String::from_utf8_lossy(&frame)
        );
        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }

    #[test]
    fn a_config_change_becomes_one_config_frame() {
        let dir = crate::test_support::TempDir::new("hub-sessions-config");
        let hub = SessionHub::new(dir.path().to_path_buf());
        let mut stream = Stream::new(
            Arc::clone(&hub),
            dir.path().join("run/1.sock"),
            &live_row("s", 0),
            dir.path().join("s.jsonl"),
        );
        let out = stream.on_line(
            r#"{"event":{"source":"runtime","name":"ConfigChanged","data":{"revision":2,"change":{"model":{"provider":"p","model":"m"}}}}}"#,
        );
        let decoded = frames(&out);
        let config = decoded
            .iter()
            .find(|(name, _)| name == "config")
            .expect("a ConfigChanged record is one config frame");
        assert_eq!(config.1["revision"], json!(2));
        assert_eq!(config.1["change"]["model"]["provider"], json!("p"));
    }
}

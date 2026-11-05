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
//! delays no request: the routes that name one session's port talk to it on `spawn_blocking`,
//! and the event stream reads the queue snapshot on a blocking task of its own.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// The probe timeout while a just-started process's port is being awaited: short enough that
/// the wait retries several times a second, long enough for a loaded process to answer one
/// line. The scan's own [`PROBE_TIMEOUT`] stays longer so a busy session is not forgotten.
const START_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// The longest the `models` route waits for a session's port.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a dropped subscription waits before it reconnects.
const RECONNECT_PAUSE: Duration = Duration::from_millis(200);

/// The longest a newly started hub-held process may take to answer its control port before
/// the write route fails with `502` (ADR 0048 §16). Startup includes MCP servers and the
/// session recovery, so this is generous; a process that exits early fails at once.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the wait for a new process's control port retries.
const START_POLL: Duration = Duration::from_millis(25);

/// How long a held process gets after SIGTERM before the hub kills it while stopping (ADR
/// 0048 §16). Its own SIGTERM path cancels the running turn, closes the inbox and joins the
/// driver, which kills every background job's process group.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// How often the hub stops waiting for a signalled held process to leave its socket.
const STOP_POLL: Duration = Duration::from_millis(25);

/// The largest a session write route's body may be, before the route answers `413`. The
/// control port's request line is 16 MiB (CONTROL.md); forwarding adds a short envelope, so
/// the body keeps 1 KiB of headroom under it.
pub const SESSION_BODY_LIMIT: usize = radian_host::control::MAX_REQUEST_BYTES - 1024;

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
    /// `hub` when a process the hub started in its hub-held mode hosts the session, `tui` for
    /// any other hosting process (ADR 0048 §16).
    pub holder: String,
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
    /// The answer arrived after the queue entry was completed or abandoned: `409`, and
    /// nothing changed.
    Late(String),
    /// A required field is missing or has the wrong JSON type: `400`.
    Bad(String),
    /// The body is well formed but a value is refused (an attachment over a limit, a model
    /// object that is not a route, a config change the port refuses): `422`.
    Invalid(String),
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
            | SessionError::Late(message)
            | SessionError::Bad(message)
            | SessionError::Invalid(message)
            | SessionError::Port(message)
            | SessionError::Failed(message) => message,
        }
    }
}

/// The hub's live-session state: what each socket's port last said, one event fan-out per
/// live session, and the hub-held processes it started (ADR 0048 §16).
pub struct SessionHub {
    /// `$RADIAN_HOME`: the `run/` directory is scanned and `hub/token` is read from here.
    home: PathBuf,
    /// `[hub] session_idle_secs`: what a hub-held process is told as its reclaim window.
    idle_secs: u64,
    /// `[hub.hooks]` (ADR 0048 §16), read once when the hub starts.
    hooks: HubHooks,
    /// The configuration file the hub was itself started with, passed on to every hub-held
    /// process (ADR 0048 §16). `serve` always resolves a path (the explicit file or the
    /// machine's default) and hands it to [`SessionHub::with_hooks`], so production never
    /// takes the `None` branch; `None` is only a direct construction that has no file to pass
    /// on (a test), and the child then resolves the machine's file itself.
    config: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// The hub-held processes, keyed by pid. A process is listed here from the moment it is
    /// spawned (with its session id) so a second request never starts a second process for
    /// the same journal; the reaper removes it when it exits.
    held: Mutex<BTreeMap<u32, Held>>,
    /// One start (or resume) at a time. A prompt that must start a process holds this across
    /// the spawn and the wait for the port; a second prompt then finds the process the first
    /// one started instead of starting its own.
    start: Mutex<()>,
}

/// One hub-held process (ADR 0048 §16).
pub struct Held {
    child: Child,
    /// The session this process opened; known before the spawn for both a new session
    /// (`--session-id`) and a resume (`--resume`).
    session_id: String,
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
    /// re-sends the head it already pushed (ADR 0048 §16: one run per new head). The key pairs
    /// the socket path with the socket file's inode, so a pid reused by a new process (a new
    /// socket file at the same path) is a new process whose `request_id`s start over.
    heads: BTreeMap<(PathBuf, u64), SeenHead>,
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
    /// A hub that has not scanned anything yet, with no `[hub.hooks]` commands. `idle_secs` is
    /// the window a hub-held process gets before it reclaims itself (`[hub]
    /// session_idle_secs`).
    #[must_use]
    pub fn new(home: PathBuf, idle_secs: u64) -> Arc<Self> {
        Self::with_hooks(home, idle_secs, HubHooks::default(), None)
    }

    /// A hub that runs `hooks` for every new pending head of a live session, and tells each
    /// hub-held process that `idle_secs` is its reclaim window.
    #[must_use]
    pub fn with_hooks(
        home: PathBuf,
        idle_secs: u64,
        hooks: HubHooks,
        config: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            home,
            idle_secs,
            hooks,
            config,
            inner: Mutex::new(Inner::default()),
            held: Mutex::new(BTreeMap::new()),
            start: Mutex::new(()),
        })
    }

    /// The reclaim window a hub-held process is told.
    #[must_use]
    pub fn idle_secs(&self) -> u64 {
        self.idle_secs
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
    /// socket file (another process, whose `request_id`s begin at 1 again; the inode tells a
    /// reused pid's new socket from the old one) or another session id.
    fn claim_head(&self, socket: &Path, session: &str, request_id: u64) -> bool {
        let key = (socket.to_path_buf(), socket_ino(socket));
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let new = inner
            .heads
            .get(&key)
            .is_none_or(|seen| seen.session != session || seen.request != request_id);
        if new {
            inner.heads.insert(
                key,
                SeenHead {
                    session: session.to_owned(),
                    request: request_id,
                },
            );
        }
        new
    }

    /// Forgets the head memory of sockets whose pid is gone, and of entries whose socket file
    /// has been replaced (a reused pid's new process), so it does not grow with every process
    /// that ever reported here. A socket whose process is still alive and whose file is still the
    /// one it recorded keeps its memory while the scanner has forgotten the session: that is what
    /// a recovery needs.
    fn forget_dead_heads(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.heads.retain(|(socket, ino), _| {
            socket_pid(socket).is_some_and(process::alive) && *ino == socket_ino(socket)
        });
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

    /// Forgets the live entry for session `id` when its socket is gone but the last scan has not
    /// noticed yet. The scanner owns the cache; dropping the stale row here is what makes the
    /// next request see the session as ended (ADR 0048 §16).
    fn forget_session(&self, id: &str) {
        let socket = {
            let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner
                .live
                .iter()
                .find(|(_, live)| live.id == id)
                .map(|(socket, _)| socket.clone())
        };
        if let Some(socket) = socket {
            self.forget(&socket);
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

    /// The socket of a process hosting `id`, starting a hub-held one with `--resume` when no
    /// process hosts it (ADR 0048 §16). The start lock is held across the spawn and the wait
    /// for the port, so two concurrent prompts for one ended session start one process: the
    /// second finds the first one's process (or its port) and never spawns another.
    ///
    /// # Errors
    ///
    /// [`SessionError::Failed`] when the process cannot be started and [`SessionError::Port`]
    /// when it exits or does not answer its control port.
    fn start_for(&self, repo: &Path, id: &str) -> Result<PathBuf, SessionError> {
        let _guard = self.start.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(socket) = self.hosting(id, |socket| {
            probe(socket)
                .map(|live| live.is_some_and(|live| live.id == id))
                .unwrap_or(false)
        }) {
            return Ok(socket);
        }
        if let Some((pid, socket)) = self.held_socket(id) {
            // A previous start held the lock until its port answered, so this one is up unless
            // it has since reclaimed itself; a dead one is forgotten and replaced.
            if self.wait_for_session(pid, id).is_ok() {
                return Ok(socket);
            }
            self.forget_held(pid);
        }
        let pid = self.spawn_held(repo, id, true, None)?;
        match self.wait_for_session(pid, id) {
            Ok(_) => Ok(self.socket_for(pid)),
            Err(error) => {
                self.forget_held(pid);
                // The child may have been refused by the one-process-per-journal check: the
                // process that hosts the session was listening but too slow to answer this
                // scan, and answered the child's own scan (which is what makes it refuse, or
                // what makes its `--resume` find the journal locked). Probe again and route
                // through that process instead of failing the prompt (ADR 0048 §16).
                match self.probe_hosts(id) {
                    Some(socket) => Ok(socket),
                    None => Err(error),
                }
            }
        }
    }

    /// The socket of a process that already hosts `id`, if one does. This is the
    /// one-process-per-journal check: the scanner's cache answers first, but only through
    /// `alive`, because a process can exit between two scans; when the cached socket is dead,
    /// or on a cache miss, every socket is probed afresh, so a process the two-second scan has
    /// not seen yet is still found. A fresh row is not recorded: the scanner owns the cache and
    /// the subscription that goes with it.
    ///
    /// A dead cached socket, a `None` from `alive`, or a port that reports another id falls
    /// through to a fresh scan (`probe_hosts`), so the cached row is never the only answer; a
    /// fresh scan that finds no host is `None`, and the caller then starts the process.
    fn hosting(&self, id: &str, alive: impl Fn(&Path) -> bool) -> Option<PathBuf> {
        if let Some((socket, live)) = self.live(id)
            && live.id == id
            && alive(&socket)
        {
            return Some(socket);
        }
        self.probe_hosts(id)
    }

    /// A fresh scan of the run directory for a socket whose port reports `id`, ignoring the
    /// scanner's cache. Used when a cached row is stale or a port that was silent has to be
    /// asked again; a fresh row is not recorded (the scanner owns the cache).
    fn probe_hosts(&self, id: &str) -> Option<PathBuf> {
        for socket in self.sockets() {
            if let Ok(Some(live)) = probe(&socket)
                && live.id == id
            {
                return Some(socket);
            }
        }
        None
    }

    /// The live session with `id`, looked up in the scanner's cache first and, on a miss,
    /// probed afresh in every socket of the run directory. A fresh row is returned but not
    /// recorded: the scanner owns the cache and the subscription that goes with it, and a
    /// route that recorded a session would leave it without an event stream until the next
    /// scan. The fresh probe is what keeps a process the two-second scan has not seen yet from
    /// being started a second time (ADR 0048 §16).
    #[must_use]
    pub fn locate(&self, id: &str) -> Option<(PathBuf, LiveSession)> {
        if let Some(found) = self.live(id) {
            return Some(found);
        }
        for socket in self.sockets() {
            if let Ok(Some(live)) = probe(&socket)
                && live.id == id
            {
                return Some((socket, live));
            }
        }
        None
    }

    /// The socket of the hub-held process for `id`, once it was spawned.
    fn held_socket(&self, id: &str) -> Option<(u32, PathBuf)> {
        let held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        held.iter()
            .find(|(_, process)| process.session_id == id)
            .map(|(pid, _)| (*pid, self.socket_for(*pid)))
    }

    /// `<run dir>/<pid>.sock`.
    fn socket_for(&self, pid: u32) -> PathBuf {
        self.run_dir().join(format!("{pid}.sock"))
    }

    /// Starts one hub-held process (ADR 0048 §16) and returns its pid. `resume` selects
    /// `--resume <session_id>`; otherwise the process creates that id (`--session-id`). Its
    /// workspace is `repo`, its configuration is this machine's, and `route` overrides the
    /// configured model when the request named one. stdin is closed (the process must never
    /// read a terminal) and its stderr joins the hub's, so a startup failure is visible.
    fn spawn_held(
        &self,
        repo: &Path,
        session_id: &str,
        resume: bool,
        route: Option<(&str, &str)>,
    ) -> Result<u32, SessionError> {
        let program = std::env::current_exe()
            .map_err(|error| SessionError::Failed(format!("cannot find this program: {error}")))?;
        let arguments = self.held_arguments(repo, session_id, resume, route);
        let child = Command::new(program)
            .args(&arguments)
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                SessionError::Failed(format!(
                    "cannot start a session process in {}: {error}",
                    repo.display()
                ))
            })?;
        let pid = child.id();
        self.held
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                pid,
                Held {
                    child,
                    session_id: session_id.to_owned(),
                },
            );
        Ok(pid)
    }

    /// The arguments a hub-held child is started with, the program name excluded.
    fn held_arguments(
        &self,
        repo: &Path,
        session_id: &str,
        resume: bool,
        route: Option<(&str, &str)>,
    ) -> Vec<String> {
        let mut arguments = vec![
            "exec".to_owned(),
            "--hub-held".to_owned(),
            "--hub-idle-secs".to_owned(),
            self.idle_secs.to_string(),
            "--workspace".to_owned(),
            repo.display().to_string(),
        ];
        if resume {
            arguments.push("--resume".to_owned());
        } else {
            arguments.push("--session-id".to_owned());
        }
        arguments.push(session_id.to_owned());
        if let Some((provider, model)) = route {
            arguments.push("--provider".to_owned());
            arguments.push(provider.to_owned());
            arguments.push("--model".to_owned());
            arguments.push(model.to_owned());
        }
        // The child resolves its own configuration otherwise, which is not the file the hub was
        // told to read when the two differ (ADR 0048 §16).
        if let Some(config) = &self.config {
            arguments.push("--config".to_owned());
            arguments.push(config.display().to_string());
        }
        arguments
    }

    /// Waits until `pid`'s control port answers `list_sessions` with `id`, or fails. A process
    /// that exits before its port answers is a start failure at once, not a timeout.
    fn wait_for_session(&self, pid: u32, id: &str) -> Result<LiveSession, SessionError> {
        let socket = self.socket_for(pid);
        let deadline = Instant::now() + START_TIMEOUT;
        let mut last: String;
        loop {
            if let Some(status) = self.exited(pid) {
                return Err(SessionError::Port(format!(
                    "the session process exited before its control port answered ({status})"
                )));
            }
            match probe_within(&socket, START_PROBE_TIMEOUT) {
                Ok(Some(live)) if live.id == id => return Ok(live),
                Ok(Some(live)) => last = format!("the port reports session {:?}", live.id),
                Ok(None) => last = "the port reported no main row".to_owned(),
                Err(error) => last = error,
            }
            if Instant::now() >= deadline {
                return Err(SessionError::Port(format!(
                    "the session process did not answer its control port within {} seconds ({last})",
                    START_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(START_POLL);
        }
    }

    /// The exit status of a held process that has already exited (reaping it), or `None` while
    /// it runs. The entry stays in the held map until the reaper removes it.
    fn exited(&self, pid: u32) -> Option<std::process::ExitStatus> {
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        let process = held.get_mut(&pid)?;
        process.child.try_wait().ok().flatten()
    }

    /// Forgets a held process whose start failed: SIGTERM first (its own handler closes the
    /// session), a moment to exit, then SIGKILL if it is still there.
    fn forget_held(&self, pid: u32) {
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        let Some(mut process) = held.remove(&pid) else {
            return;
        };
        let _ = process::signal(pid, process::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(process.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(STOP_POLL);
        }
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    /// Removes held processes that have exited, so the map does not grow with every idle
    /// reclaim. Called by the scanner on every pass.
    fn reap(&self) {
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        held.retain(|_, process| match process.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => false,
        });
    }

    /// Stops every hub-held process cleanly (ADR 0048 §16), before the hub releases
    /// `hub.lock`: SIGTERM (which the process handles by cancelling its turn, closing the
    /// session and flushing the journal), the [`STOP_GRACE`] window, then SIGKILL for what is
    /// left. A hub-held process this hub did not spawn (one that outlived an `exec` of a
    /// rebuilt binary) is found by its `holder` on the run directory's sockets and signalled
    /// the same way.
    pub fn shutdown(&self) {
        let pids: Vec<u32> = {
            let held = self.held.lock().unwrap_or_else(|error| error.into_inner());
            held.keys().copied().collect()
        };
        let mut signalled: Vec<(u32, Option<Child>)> = Vec::with_capacity(pids.len());
        for pid in pids {
            let _ = process::signal(pid, process::SIGTERM);
            signalled.push((pid, None));
        }
        for socket in self.sockets() {
            let Ok(Some(live)) = probe(&socket) else {
                continue;
            };
            if live.holder != "hub" {
                continue;
            }
            let Some(pid) = socket
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u32>().ok())
            else {
                continue;
            };
            if signalled.iter().any(|(known, _)| *known == pid) {
                continue;
            }
            let _ = process::signal(pid, process::SIGTERM);
            signalled.push((pid, None));
        }
        // Take the children out so the waits happen without holding the map.
        {
            let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
            for (pid, slot) in signalled.iter_mut() {
                if let Some(process) = held.remove(pid) {
                    *slot = Some(process.child);
                }
            }
        }
        let deadline = Instant::now() + STOP_GRACE;
        let mut waiting: Vec<(u32, Child)> = signalled
            .into_iter()
            .filter_map(|(pid, child)| child.map(|child| (pid, child)))
            .collect();
        while !waiting.is_empty() {
            waiting.retain_mut(|(_, child)| match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => false,
            });
            if waiting.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                for (_, child) in &mut waiting {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                break;
            }
            std::thread::sleep(STOP_POLL);
        }
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

/// The socket file's inode, or `0` when it cannot be read. A reused pid binds a **new** socket
/// file at the same path, so the inode tells its process from the one that owned the path
/// before: the head memory of the old process does not block the new process's first head.
fn socket_ino(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    fs::metadata(path)
        .map(|metadata| metadata.ino())
        .unwrap_or(0)
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
        hub.reap();
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
    probe_within(socket, PROBE_TIMEOUT)
}

/// [`probe`] with an explicit read timeout.
fn probe_within(socket: &Path, timeout: Duration) -> Result<Option<LiveSession>, String> {
    let response = request(
        socket,
        &json!({ "id": 1, "verb": "list_sessions" }),
        timeout,
        None,
    )
    .map_err(|error| error.to_string())?;
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
            holder: row
                .get("holder")
                .and_then(Value::as_str)
                .unwrap_or("tui")
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
) -> Result<Value, CallError> {
    let mut stream = UnixStream::connect(socket).map_err(|error| connect_error(socket, &error))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let mut request = request.clone();
    if let Some(token) = token
        && let Some(object) = request.as_object_mut()
    {
        object.insert("token".to_owned(), json!(token));
    }
    let mut line =
        serde_json::to_vec(&request).map_err(|error| CallError::Failed(error.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|error| {
        CallError::Failed(format!("cannot write to {}: {error}", socket.display()))
    })?;
    let mut reader = std::io::BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).map_err(|error| {
        CallError::Failed(format!("cannot read from {}: {error}", socket.display()))
    })?;
    let value: Value = serde_json::from_str(answer.trim()).map_err(|error| {
        CallError::Failed(format!("{} answered no JSON: {error}", socket.display()))
    })?;
    if value.get("ok") == Some(&Value::Bool(true)) {
        return Ok(value);
    }
    Err(CallError::Refused(
        value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the control port refused the request")
            .to_owned(),
    ))
}

/// The [`CallError`] one failed connect is. A socket file that is gone, or one that is still
/// there and nothing listens on, means the process that hosted the session has ended: the
/// scanner may not have noticed yet (it reads the run directory every two seconds), so the
/// caller must answer as it does for an ended session rather than report a port failure
/// (ADR 0048 §16). Every other connect failure (a permission error, a resource limit) is the
/// port failing.
fn connect_error(socket: &Path, error: &std::io::Error) -> CallError {
    let message = format!("cannot connect to {}: {error}", socket.display());
    if matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused
    ) {
        CallError::Gone(message)
    } else {
        CallError::Failed(message)
    }
}

/// Why one call to a control port did not produce a successful answer line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CallError {
    /// The port answered, and the answer is an error line: the request was refused.
    Refused(String),
    /// The connection, the write or the read failed, or the answer was not JSON: the port
    /// itself failed.
    Failed(String),
    /// The connect failed because the socket is gone: the hosting process has ended, and the
    /// session is an ended one whatever the last scan recorded.
    Gone(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Refused(message) | CallError::Failed(message) | CallError::Gone(message) => {
                f.write_str(message)
            }
        }
    }
}

/// The `models` route's answer: the control port's own list (ADR 0026 D5).
///
/// # Errors
///
/// [`SessionError::NotLive`] when no process hosts the session, [`SessionError::Port`] when its
/// port fails.
pub fn models(hub: &SessionHub, id: &str) -> Result<Value, SessionError> {
    let Some((socket, _)) = hub.live(id) else {
        return Err(ended(id));
    };
    let answer = call_live(hub, id, &socket, &json!({ "id": 1, "verb": "models" }))?;
    Ok(json!({
        "models": answer.get("models").cloned().unwrap_or(Value::Array(Vec::new())),
    }))
}

/// The session event stream's first frame: the control port's `pending` answer reduced to the
/// queue's current state, `{"head", "length"}`, the same shape a live `pending` frame carries.
/// It is read after the hub has subscribed to the session's live events, so a change raised
/// while the read is in flight stays buffered behind the snapshot.
///
/// # Errors
///
/// [`SessionError::NotLive`] when no process hosts the session, [`SessionError::Port`] when its
/// port fails, and the route's refusal mapping for an error line.
pub fn pending(hub: &SessionHub, id: &str) -> Result<Value, SessionError> {
    let Some((socket, _)) = hub.live(id) else {
        return Err(ended(id));
    };
    let answer = call_live(hub, id, &socket, &json!({ "id": 1, "verb": "pending" }))?;
    Ok(json!({
        "head": answer.get("head").cloned().unwrap_or(Value::Null),
        "length": answer.get("length").cloned().unwrap_or_else(|| json!(0)),
    }))
}

/// The snapshot frame a session event stream opens with.
#[must_use]
pub fn pending_frame(data: &Value) -> Bytes {
    frame("pending", data)
}

// ---- the write routes ---------------------------------------------------------------------

/// One model route a create request names.
struct ModelRoute<'a> {
    provider: &'a str,
    model: &'a str,
}

/// One session write route's validated body (ADR 0048 §16). `raw` is the JSON object the
/// route forwards, so the port parses exactly what the client sent (attachments included).
struct SessionInput {
    raw: Value,
    steer: bool,
}

/// Parses and validates a session write route's body. A missing or wrong-typed field is
/// [`SessionError::Bad`] (`400`); a well-formed value the composer refuses is
/// [`SessionError::Invalid`] (`422`). Nothing is started or forwarded when this fails.
fn parse_input(body: &[u8]) -> Result<SessionInput, SessionError> {
    let raw = parse_object(body)?;
    match raw.get("text") {
        Some(Value::String(_)) => {}
        None => return Err(SessionError::Bad("`text` is required".to_owned())),
        Some(_) => return Err(SessionError::Bad("`text` must be a string".to_owned())),
    }
    let steer = match raw.get("steer") {
        None => false,
        Some(Value::Bool(steer)) => *steer,
        Some(_) => return Err(SessionError::Bad("`steer` must be a boolean".to_owned())),
    };
    // A present `null` is a wrong-typed field, not an absent one: the ADR says a missing or
    // wrong-typed field is `400`, and the composer's own check (`validate_attachments`) cannot
    // tell a shape error from a content refusal (`422`).
    for field in ["images", "files"] {
        match raw.get(field) {
            None | Some(Value::Array(_)) => {}
            Some(_) => return Err(SessionError::Bad(format!("`{field}` must be an array"))),
        }
    }
    radian_host::control::validate_attachments(&raw).map_err(SessionError::Invalid)?;
    Ok(SessionInput { raw, steer })
}

/// Parses a write route's body as a JSON object; an empty body is `{}`.
fn parse_object(body: &[u8]) -> Result<Value, SessionError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| SessionError::Bad(format!("the body is not JSON: {error}")))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(SessionError::Bad(
            "the body must be a JSON object".to_owned(),
        ))
    }
}

/// The create route's optional `model`: exactly `{"provider": string, "model": string}`.
fn parse_model(raw: &Value) -> Result<Option<ModelRoute<'_>>, SessionError> {
    let Some(model) = raw.get("model") else {
        return Ok(None);
    };
    let Some(object) = model.as_object() else {
        return Err(SessionError::Bad(
            "`model` must be an object with `provider` and `model`".to_owned(),
        ));
    };
    if object.keys().any(|key| key != "provider" && key != "model") {
        return Err(SessionError::Bad(
            "`model` accepts only `provider` and `model`".to_owned(),
        ));
    }
    let provider = object
        .get("provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SessionError::Bad("`model.provider` must be a non-empty string".to_owned())
        })?;
    let name = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| SessionError::Bad("`model.model` must be a non-empty string".to_owned()))?;
    Ok(Some(ModelRoute {
        provider,
        model: name,
    }))
}

/// Forwards one already-built control request to the live session `id` over `socket`, with the
/// hub token. A refusal keeps its meaning: the queue's two answer outcomes get their own route
/// errors, and every other refusal is a value the route forwarded, so it is `422`. A connect
/// that finds the socket gone is the session ending between the last scan and this call, so it
/// is the ended-session answer (`409`), and the stale live entry is dropped.
fn forward(
    socket: &Path,
    hub: &SessionHub,
    id: &str,
    request: &Value,
) -> Result<Value, SessionError> {
    call_live(hub, id, socket, request)
}

/// One port call for the live session `id`; see [`forward`] for the error mapping.
fn call_live(
    hub: &SessionHub,
    id: &str,
    socket: &Path,
    request: &Value,
) -> Result<Value, SessionError> {
    let token = hub.token();
    match self::request(socket, request, REQUEST_TIMEOUT, token.as_deref()) {
        Ok(answer) => Ok(answer),
        Err(CallError::Gone(_)) => {
            hub.forget_session(id);
            Err(ended(id))
        }
        Err(CallError::Failed(message)) => Err(SessionError::Port(message)),
        Err(CallError::Refused(message)) => Err(refusal(message)),
    }
}

/// The route error a session no process hosts answers: `409`, and nothing changed.
fn ended(id: &str) -> SessionError {
    SessionError::NotLive(format!("session {id:?} is not live: no process hosts it"))
}

/// The route error a control port's error line becomes.
fn refusal(message: String) -> SessionError {
    if message.contains("the request is no longer pending") {
        SessionError::Late(message)
    } else if message.contains("no pending request has that id") {
        SessionError::Unknown(message)
    } else if message.contains("the session has closed") {
        SessionError::NotLive(message)
    } else {
        SessionError::Invalid(message)
    }
}

/// `POST /v1/repos/<name>/sessions`: start a hub-held process for a new session, send the
/// body's first message, and return the session id. The id is generated here and handed to
/// the process with `--session-id`, so the route knows it before the port answers.
///
/// # Errors
///
/// [`SessionError::Bad`]/[`SessionError::Invalid`] for a refused body (before anything
/// starts), [`SessionError::Failed`] when the process cannot be started, and
/// [`SessionError::Port`] when it exits or does not answer its port.
pub fn start_session(hub: &SessionHub, repo: &Path, body: &[u8]) -> Result<String, SessionError> {
    let input = parse_input(body)?;
    let route = parse_model(&input.raw)?;
    let id = radian_host::generate_session_id();
    let pid = hub.spawn_held(
        repo,
        &id,
        false,
        route.as_ref().map(|route| (route.provider, route.model)),
    )?;
    if let Err(error) = hub.wait_for_session(pid, &id) {
        hub.forget_held(pid);
        return Err(error);
    }
    if let Err(error) = send_input(hub, &id, &hub.socket_for(pid), &input) {
        hub.forget_held(pid);
        return Err(error);
    }
    Ok(id)
}

/// Sends one validated input to the session `id` over its port: `steer` when the body asked for
/// it, `followup` otherwise, with the hub token so the port records it as the owner's.
fn send_input(
    hub: &SessionHub,
    id: &str,
    socket: &Path,
    input: &SessionInput,
) -> Result<Value, SessionError> {
    let verb = if input.steer { "steer" } else { "followup" };
    let mut request = input.raw.clone();
    if let Some(object) = request.as_object_mut() {
        object.remove("steer");
        object.remove("model");
        object.insert("id".to_owned(), json!(1));
        object.insert("verb".to_owned(), json!(verb));
    }
    forward(socket, hub, id, &request)
}

/// `POST …/sessions/<id>/prompt`: deliver the body to the session. A live session receives it
/// through its socket; a session no process hosts is resumed by a hub-held process first. A
/// second prompt for the same ended session finds the process the first one started (the
/// hub's one-start lock), so only one process ever opens the journal.
///
/// The route checks the control sockets directly before it starts anything (`locate`), the
/// same probe the opener and the `hub_holder` refusal use: a process the two-second scan has
/// not seen yet still hosts the session, and starting a second one would only be refused by
/// the one-process-per-journal rule.
///
/// # Errors
///
/// [`SessionError::Bad`]/[`SessionError::Invalid`] for a refused body, [`SessionError::Unknown`]
/// for a session that is not this repository's, [`SessionError::Port`] when its port fails.
pub fn prompt(hub: &SessionHub, repo: &Path, id: &str, body: &[u8]) -> Result<Value, SessionError> {
    let input = parse_input(body)?;
    // The session must be this repository's before anything starts.
    journal_for(repo, id)?;
    if let Some((socket, _)) = hub.locate(id) {
        match send_input(hub, id, &socket, &input) {
            // The socket this row names died between the scan and this send: resume, the same
            // way an ended session is.
            Err(SessionError::NotLive(_)) => {}
            other => return other,
        }
    }
    let socket = hub.start_for(repo, id)?;
    send_input(hub, id, &socket, &input)
}

/// `POST …/sessions/<id>/cancel`: forward the port's `cancel`.
///
/// # Errors
///
/// [`SessionError::Unknown`] for a session that is not this repository's,
/// [`SessionError::NotLive`] when no process hosts it, [`SessionError::Port`] when its port
/// fails.
pub fn cancel(hub: &SessionHub, repo: &Path, id: &str) -> Result<(), SessionError> {
    let (socket, _) = live_session(hub, repo, id)?;
    forward(&socket, hub, id, &json!({ "id": 1, "verb": "cancel" }))?;
    Ok(())
}

/// `POST …/sessions/<id>/answer`: forward the port's `answer`. A late answer is
/// [`SessionError::Late`] (`409`) and changes nothing; an id the queue never issued is
/// [`SessionError::Unknown`] (`404`).
///
/// # Errors
///
/// As [`cancel`], plus the body's field checks ([`SessionError::Bad`]) and the port's refusal
/// ([`SessionError::Invalid`]).
pub fn answer(hub: &SessionHub, repo: &Path, id: &str, body: &[u8]) -> Result<(), SessionError> {
    let raw = parse_object(body)?;
    if !raw.get("request_id").is_some_and(Value::is_u64) {
        return Err(SessionError::Bad(
            "`request_id` is required and must be a non-negative integer".to_owned(),
        ));
    }
    if raw.get("response").is_none_or(Value::is_null) {
        return Err(SessionError::Bad("`response` is required".to_owned()));
    }
    let (socket, _) = live_session(hub, repo, id)?;
    let mut request = raw;
    if let Some(object) = request.as_object_mut() {
        object.insert("id".to_owned(), json!(1));
        object.insert("verb".to_owned(), json!("answer"));
    }
    forward(&socket, hub, id, &request)?;
    Ok(())
}

/// `POST …/sessions/<id>/config`: forward the port's `config`; the port validates the keys
/// and queues the change for the next turn boundary (ADR 0004).
///
/// # Errors
///
/// As [`cancel`], plus the body's field checks ([`SessionError::Bad`]) and the port's refusal
/// ([`SessionError::Invalid`]).
pub fn config(hub: &SessionHub, repo: &Path, id: &str, body: &[u8]) -> Result<(), SessionError> {
    let raw = parse_object(body)?;
    if !raw.get("config").is_some_and(Value::is_object) {
        return Err(SessionError::Bad(
            "`config` is required and must be an object".to_owned(),
        ));
    }
    let (socket, _) = live_session(hub, repo, id)?;
    let mut request = raw;
    if let Some(object) = request.as_object_mut() {
        object.insert("id".to_owned(), json!(1));
        object.insert("verb".to_owned(), json!("config"));
    }
    forward(&socket, hub, id, &request)?;
    Ok(())
}

/// `POST …/sessions/<id>/close`: forward the port's `close` (ADR 0043), which answers `ok` before
/// its host cancels the running turn, abandons the queue and exits. A hub-held process installs
/// the hook, and so does the full-screen interface: either leaves its journal for the next
/// `prompt`. A host that installs no hook refuses the verb and the route carries that text.
///
/// # Errors
///
/// As [`cancel`], except that a port that answers with an error is [`SessionError::Port`]
/// (`502`) with the port's own text: `close` is not a client value the route can refuse with
/// `422`, and "this process cannot be closed remotely" is the host's answer, not the client's.
pub fn close(hub: &SessionHub, repo: &Path, id: &str) -> Result<(), SessionError> {
    let (socket, _) = live_session(hub, repo, id)?;
    let token = hub.token();
    match self::request(
        &socket,
        &json!({ "id": 1, "verb": "close" }),
        REQUEST_TIMEOUT,
        token.as_deref(),
    ) {
        Ok(_) => Ok(()),
        // A socket gone under the last scan is the ended session, and the stale row goes, the
        // same as [`cancel`].
        Err(CallError::Gone(_)) => {
            hub.forget_session(id);
            Err(ended(id))
        }
        // Every error the port answers, and every failure of the call, is the port failing:
        // the route carries the text as `502`.
        Err(CallError::Failed(message) | CallError::Refused(message)) => {
            Err(SessionError::Port(message))
        }
    }
}

/// The live socket of a session this repository owns, or the route's error.
fn live_session(
    hub: &SessionHub,
    repo: &Path,
    id: &str,
) -> Result<(PathBuf, LiveSession), SessionError> {
    journal_for(repo, id)?;
    // A fresh probe as well as the scanner's cache: a process started a moment ago is live
    // before the next scan sees it, and its queue may already hold the request being answered.
    hub.locate(id).ok_or_else(|| ended(id))
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
                "live": current.map_or("none", |live| live.holder.as_str()),
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
                "live": current.holder,
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
            holder: "tui".to_owned(),
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
    fn a_gone_socket_is_an_ended_session_not_a_port_failure() {
        let socket = Path::new("/nonexistent/1.sock");
        for kind in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            let error = std::io::Error::new(kind, "gone");
            assert!(
                matches!(connect_error(socket, &error), CallError::Gone(_)),
                "{kind:?} means the hosting process ended"
            );
        }
        for kind in [ErrorKind::TimedOut, ErrorKind::PermissionDenied] {
            let error = std::io::Error::new(kind, "failed");
            assert!(
                matches!(connect_error(socket, &error), CallError::Failed(_)),
                "{kind:?} is the port failing, not the session ending"
            );
        }
    }

    #[test]
    fn a_stale_live_row_with_a_gone_socket_is_the_ended_session() {
        // The scanner still lists the session, but its socket file is gone: the route must
        // answer exactly what it answers for an ended session, not a port failure, and the
        // stale row goes (ADR 0048 §16). No scanner runs here, so the row stays put until the
        // call drops it: this is the deterministic form of the gate's race.
        let dir = crate::test_support::TempDir::new("hub-sessions-stale-row");
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
        // Short enough for `sun_path`, and never created: the connect finds it gone.
        let socket = PathBuf::from("/nonexistent/4242.sock");
        assert!(hub.record(&socket, live_row("s", 0)), "the row is new");
        let error = call_live(&hub, "s", &socket, &json!({ "verb": "cancel", "id": 1 }))
            .expect_err("a gone socket is no answer");
        assert!(
            matches!(error, SessionError::NotLive(_)),
            "a gone socket is the ended session, not the port failing: {error:?}"
        );
        assert!(
            hub.live("s").is_none(),
            "the stale live row is dropped, so the next request sees the session ended"
        );
    }

    #[test]
    fn a_null_steer_is_a_bad_body_not_an_absent_field() {
        // The manual and ADR 0048 §16: a wrong-typed field is `400` and nothing starts. A
        // present `null` is not an absent field.
        let error = match parse_input(br#"{"text":"x","steer":null}"#) {
            Ok(_) => panic!("a null steer is refused"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionError::Bad(_)), "{error:?}");
        assert!(
            parse_input(br#"{"text":"x"}"#).is_ok(),
            "an absent steer is false"
        );
    }

    #[test]
    fn a_null_attachment_is_a_bad_body_not_an_absent_field() {
        // A present `images: null`/`files: null` is wrong-typed, so the routes answer `400`
        // before a process starts; an absent field is the empty list.
        for field in ["images", "files"] {
            let raw = json!({ "text": "x", field: null });
            let error = match parse_input(raw.to_string().as_bytes()) {
                Ok(_) => panic!("a null {field} is refused"),
                Err(error) => error,
            };
            assert!(matches!(error, SessionError::Bad(_)), "{error:?}");
        }
        assert!(
            parse_input(br#"{"text":"x"}"#).is_ok(),
            "an absent attachment list is the empty one"
        );
    }

    #[test]
    fn a_null_model_is_a_bad_body_not_an_absent_field() {
        let raw = json!({ "text": "x", "model": null });
        let error = match parse_model(&raw) {
            Ok(_) => panic!("a null model is refused"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionError::Bad(_)), "{error:?}");
        assert!(
            parse_model(&json!({ "text": "x" }))
                .expect("an absent model is the configured route")
                .is_none()
        );
    }

    #[test]
    fn a_held_process_is_started_with_the_hubs_own_configuration_file() {
        // `radian --config alt.toml hub` must start its sessions on `alt.toml`, not on the home
        // file the child would resolve by itself (ADR 0048 §16).
        let dir = crate::test_support::TempDir::new("hub-sessions-held-config");
        let config = dir.path().join("alt.toml");
        fs::write(&config, "schema_version = 1\n").unwrap();
        let hub = SessionHub::with_hooks(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
            HubHooks::default(),
            Some(config.clone()),
        );
        let args = hub.held_arguments(dir.path(), "s", false, None);
        let position = args
            .iter()
            .position(|arg| arg == "--config")
            .expect("the child is told the hub's configuration file");
        assert_eq!(args.get(position + 1), Some(&config.display().to_string()));

        // A hub built without a file passes none on: this branch is not reached through
        // `serve` (which always resolves the explicit file or the machine's default), only by a
        // direct construction. The child then resolves the machine's configuration itself.
        let plain = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
        assert!(
            !plain
                .held_arguments(dir.path(), "s", false, None)
                .iter()
                .any(|arg| arg == "--config")
        );
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
            holder: "tui".to_owned(),
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
        let hub = SessionHub::new(
            dir.clone(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
        assert!(hub.sockets().is_empty(), "{:?}", hub.sockets());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Mutation: fire on every pending event instead of a new head (drop the `request_id`
    /// comparison). The second call with the same head then returns a head again.
    #[test]
    fn a_repeated_pending_head_is_not_new() {
        let dir = crate::test_support::TempDir::new("hub-sessions-head");
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
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
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
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

    /// Review: the head memory is per socket **file**, not per socket path alone. A pid reused
    /// by a new process binds a new file at the same `<pid>.sock` path; its first head
    /// (`request_id` 1, a new queue's first id) is new even before the next scan prunes the old
    /// process's entry.
    #[test]
    fn a_reused_pid_socket_file_starts_the_head_memory_over() {
        let dir = crate::test_support::TempDir::new("hub-sessions-reused-pid");
        let socket = dir.path().join("run/1.sock");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        fs::write(&socket, b"old").unwrap();
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
        let live = live_row("s", 0);
        let head = json!({
            "head": { "request_id": 1, "kind": "approval", "tool": "shell" },
            "length": 1,
        });
        let journal = dir.path().join("s.jsonl");
        let mut first = Stream::new(Arc::clone(&hub), socket.clone(), &live, journal.clone());
        assert!(
            first.new_pending_head(&head).is_some(),
            "the old process's first head is new"
        );

        // The process dies and the pid is reused: the new process's socket file replaces the
        // old one at the same path. Rename a separate file over it, so the new inode is
        // certainly not the old one.
        let replacement = dir.path().join("run/1.new");
        fs::write(&replacement, b"new").unwrap();
        fs::rename(&replacement, &socket).unwrap();
        let mut reused = Stream::new(Arc::clone(&hub), socket.clone(), &live, journal);
        assert!(
            reused.new_pending_head(&head).is_some(),
            "a reused pid's new socket file starts the head memory over"
        );
        assert!(
            reused.new_pending_head(&head).is_none(),
            "the new process's own repeated head is still not new"
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
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
            HubHooks {
                after_pending: vec![command],
            },
            None,
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
            holder: "tui".to_owned(),
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
    fn a_session_another_process_hosts_is_not_started_again() {
        let dir = crate::test_support::TempDir::new("hub-sessions-hosted");
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
        let socket = hub.run_dir().join("4242.sock");
        let mut live = live_row("s", 0);
        live.workspace = "/work/example".to_owned();
        hub.record(&socket, live);

        // The scanner saw the session and its port answers: it is hosted, so no process is
        // started. A port that no longer answers, or another id, is not a host.
        assert_eq!(hub.hosting("s", |_| true), Some(socket));
        assert_eq!(hub.hosting("s", |_| false), None);
        assert_eq!(hub.hosting("other", |_| true), None);
    }

    #[test]
    fn a_pending_event_becomes_one_pending_frame_and_a_blocked_state() {
        let dir = crate::test_support::TempDir::new("hub-sessions-pending");
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
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
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
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
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
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
        let hub = SessionHub::new(
            dir.path().to_path_buf(),
            radian_host::config::DEFAULT_HUB_SESSION_IDLE_SECS,
        );
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

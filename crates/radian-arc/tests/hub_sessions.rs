//! The hub's session surface (ADR 0048 §16, the read
//! half): discovery of the sessions the run directory's control ports report, the ended sessions
//! the session index lists, the transcript, conversation, models and event-stream routes.
//!
//! The tests that need a control socket bind a fake port that speaks the protocol (`list_sessions`,
//! `subscribe`, `models`) in a short `/tmp` home, because `$TMPDIR` inside the workspace is too
//! long for a Unix socket path; the sandbox nextest profile excludes them by name.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Repo, radian_bin, write};
use radian_arc::hub::Registry;
use radian_arc::store::{Store, config};
use radian_host::transcript::{AcpFold, AttachFold};
use serde_json::{Value, json};

// ---- a hub process -------------------------------------------------------------------------

/// A running `radian hub`, its home, and the session directory it reads.
struct Hub {
    child: Child,
    /// The hub's stdout, kept open: it announces the port on its first line and then only
    /// carries status lines, which a closed pipe would swallow.
    _stdout: BufReader<std::process::ChildStdout>,
    port: u16,
    home: PathBuf,
    sessions: PathBuf,
}

impl Hub {
    /// A hub over `repo`'s own scratch home (`$TMPDIR`), the shape the other hub tests use: no
    /// session socket is involved, so the long path does not matter.
    fn over(repo: &Repo) -> Self {
        Self::start(&repo.home, &repo.sessions, &repo.clone, &[])
    }

    /// A hub in a short `/tmp` home, so a control socket fits `sun_path`; `repo`'s clone is the
    /// working directory and the sessions live under the short home.
    fn in_short_home(short: &Path, repo: &Repo, envs: &[(&str, &str)]) -> Self {
        let home = short.join("home");
        let sessions = home.join("sessions");
        Self::start(&home, &sessions, &repo.clone, envs)
    }

    fn start(home: &Path, sessions: &Path, cwd: &Path, envs: &[(&str, &str)]) -> Self {
        fs::create_dir_all(home).expect("the hub home is creatable");
        fs::create_dir_all(sessions).expect("the session directory is creatable");
        let stderr = fs::File::create(home.join("hub.stderr.log"))
            .expect("the hub stderr file is creatable");
        let mut command = Command::new(radian_bin());
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("RADIAN_HOME", home)
            .env("RADIAN_SESSION_DIR", sessions)
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .current_dir(cwd)
            .args(["hub", "--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("radian hub starts");
        let stdout = child.stdout.take().expect("the hub has stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the hub's first stdout line reads");
        let first = line.trim_end();
        let port: u16 = first
            .strip_prefix("hub: listening on 127.0.0.1:")
            .unwrap_or_else(|| panic!("the first stdout line names the listener: {first:?}"))
            .parse()
            .expect("the first stdout line names a port");
        Self {
            child,
            _stdout: reader,
            port,
            home: home.to_path_buf(),
            sessions: sessions.to_path_buf(),
        }
    }

    fn token(&self) -> String {
        fs::read_to_string(self.home.join("hub/token"))
            .expect("the token file exists")
            .trim()
            .to_owned()
    }

    fn get(&self, path: &str) -> Response {
        http(
            self.port,
            "GET",
            path,
            Some(&format!("Bearer {}", self.token())),
        )
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One HTTP/1.1 response, split at the first blank line.
struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("the body is JSON: {error}: {:?}", self.body))
    }

    fn error(&self) -> String {
        self.json()
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("the body has an error: {:?}", self.body))
            .to_owned()
    }
}

fn http(port: u16, method: &str, path: &str, header: Option<&str>) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the hub accepts a connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout is settable");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(header) = header {
        request.push_str(&format!("Authorization: {header}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .expect("the response reads to its end");
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Response {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("the response has a header block: {raw:?}"));
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("the status line is HTTP: {head:?}"));
    Response {
        status,
        body: raw[split + 4..].to_vec(),
    }
}

// ---- a fake control port -------------------------------------------------------------------

/// What a fake control port reports and where its event lines go.
struct FakeInner {
    id: String,
    workspace: String,
    state: String,
    pending: u64,
    provider: String,
    model: String,
    /// One sender per live subscription: a line written here reaches the hub.
    lines: Mutex<Vec<std::sync::mpsc::Sender<String>>>,
    /// How many `subscribe` connections were accepted.
    subscriptions: AtomicUsize,
    /// Close the next subscription right after its acknowledgement, so the hub has to reconnect.
    drop_next: AtomicBool,
    /// How many of the next `list_sessions` probes wait past the hub's probe timeout before they
    /// answer, the shape of a process too busy to answer in time.
    silent_probes: AtomicUsize,
}

/// A control socket at `<run dir>/<pid>.sock` that speaks the protocol the hub uses.
struct FakePort {
    path: PathBuf,
    inner: Arc<FakeInner>,
}

impl FakePort {
    /// Binds the socket and answers every connection on a thread of its own.
    fn bind(run_dir: &Path, pid: u32, inner: FakeInner) -> Self {
        fs::create_dir_all(run_dir).expect("the run directory is creatable");
        let path = run_dir.join(format!("{pid}.sock"));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("the fake control socket binds");
        listener
            .set_nonblocking(true)
            .expect("the fake listener is non-blocking");
        let inner = Arc::new(inner);
        let shared = Arc::clone(&inner);
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let inner = Arc::clone(&shared);
                        std::thread::spawn(move || serve(stream, inner));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });
        Self { path, inner }
    }

    /// Pushes one event line to every subscription still connected.
    fn push(&self, event: Value) {
        let line = json!({ "event": event }).to_string();
        let senders = self.inner.lines.lock().unwrap_or_else(|e| e.into_inner());
        for sender in senders.iter() {
            let _ = sender.send(line.clone());
        }
    }

    /// Drops every subscription's sender, so the fake closes the connection and the hub must
    /// reconnect. The next `wait_for_subscription` waits for that new connection.
    fn disconnect(&self) {
        self.inner
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn subscriptions(&self) -> usize {
        self.inner.subscriptions.load(Ordering::SeqCst)
    }

    /// Makes the next `count` `list_sessions` probes wait past the hub's `PROBE_TIMEOUT` before
    /// they answer, so the scanner counts them as failed probes.
    fn silent_probes(&self, count: usize) {
        self.inner.silent_probes.store(count, Ordering::SeqCst);
    }

    /// Waits for the hub's subscription, so a pushed line is not lost before the hub follows.
    fn wait_for_subscription(&self, deadline: Instant) {
        while self.subscriptions() == 0 {
            assert!(
                Instant::now() < deadline,
                "the hub never subscribed to {}",
                self.path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // The acknowledgement is written before the handler registers the sender; one short
        // pause keeps a push from racing the registration.
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One connection to the fake: read a request, answer it, and (for `subscribe`) stream the lines
/// the test pushes.
fn serve(mut stream: UnixStream, inner: Arc<FakeInner>) {
    let mut reader = BufReader::new(stream.try_clone().expect("the fake connection clones"));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let request: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let mut answer = |value: Value| {
        let mut bytes = value.to_string().into_bytes();
        bytes.push(b'\n');
        let _ = stream.write_all(&bytes);
        let _ = stream.flush();
    };
    match request.get("verb").and_then(Value::as_str) {
        Some("list_sessions") => {
            if inner.silent_probes.load(Ordering::SeqCst) > 0 {
                inner.silent_probes.fetch_sub(1, Ordering::SeqCst);
                // The hub's probe read timeout is one second; answer after it has given up.
                std::thread::sleep(Duration::from_millis(1500));
            }
            answer(json!({
                "id": 1,
                "ok": true,
                "sessions": [{
                    "id": inner.id,
                    "kind": "main",
                    "workspace": inner.workspace,
                    "model": { "provider": inner.provider, "model": inner.model },
                    "state": inner.state,
                    "pending": inner.pending,
                }],
            }))
        }
        Some("models") => answer(json!({
            "id": 1,
            "ok": true,
            "models": [{
                "provider": inner.provider,
                "models": [{ "id": inner.model, "current": true }],
            }],
        })),
        Some("subscribe") => {
            answer(json!({ "id": 1, "ok": true }));
            let (sender, receiver) = std::sync::mpsc::channel::<String>();
            inner
                .lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(sender);
            inner.subscriptions.fetch_add(1, Ordering::SeqCst);
            if inner.drop_next.swap(false, Ordering::SeqCst) {
                return;
            }
            for line in receiver {
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                if stream.write_all(&bytes).is_err() {
                    return;
                }
                let _ = stream.flush();
            }
        }
        _ => answer(json!({ "id": 1, "ok": false, "error": "unknown verb" })),
    }
}

/// Holds a journal's writer lock the way a live session does, so the hub's fold follows the
/// journal incrementally (`radian_session::session_is_live` probes the same flock). Released on
/// drop.
struct JournalLock(fs::File);

impl JournalLock {
    fn take(path: &Path) -> Self {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("the journal opens");
        fs2::FileExt::lock_exclusive(&file).expect("the journal's writer lock is free");
        Self(file)
    }
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// A pid that stays alive for the test (`kill(pid, 0)` must see it), killed on drop.
struct LivePid {
    child: Child,
}

impl LivePid {
    fn new() -> Self {
        let child = Command::new("sleep")
            .arg("120")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("a sleep process starts");
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for LivePid {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A short `/tmp` home: a Unix socket path has a hard length cap (`sun_path`), and `$TMPDIR`
/// inside the workspace is already too long for `<home>/run/<pid>.sock`.
struct ShortHome(PathBuf);

impl ShortHome {
    fn new(label: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path =
            PathBuf::from("/tmp").join(format!("radian-hub-{}-{label}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the short home is creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ShortHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- the fixtures --------------------------------------------------------------------------

/// An initialized arc store in the repository's clone.
fn arc_store(repo: &Repo) -> Store {
    let root = repo.arc();
    fs::create_dir_all(&root).expect("the arc directory is creatable");
    fs::write(root.join("config.toml"), config::default_text("main")).expect("the config writes");
    Store::at(repo.clone.clone())
}

fn register(home: &Path, name: &str, repo: &Path) {
    Registry::at(home.join("hub"))
        .add(repo, Some(name))
        .expect("the repository registers");
}

/// One journal envelope (ADR 0006 §1).
fn envelope(id: &str, seq: u64, event: &str, data: Value) -> String {
    json!({
        "schema_version": 1,
        "session_id": id,
        "seq": seq,
        "ts": "2026-11-02T00:00:00.000000Z",
        "event": event,
        "critical": true,
        "data": data,
    })
    .to_string()
}

/// Writes `<id>.jsonl` in `sessions`: a `SessionCreated` for `workspace`, then one
/// `MessageAppended` per message.
fn write_journal(sessions: &Path, id: &str, workspace: &str, messages: &[Value]) -> PathBuf {
    fs::create_dir_all(sessions).expect("the session directory is creatable");
    let mut lines = vec![envelope(
        id,
        0,
        "SessionCreated",
        json!({"workspace": workspace, "config_snapshot": {}}),
    )];
    for (index, message) in messages.iter().enumerate() {
        lines.push(envelope(
            id,
            index as u64 + 1,
            "MessageAppended",
            json!({ "index": index, "message": message }),
        ));
    }
    let path = sessions.join(format!("{id}.jsonl"));
    fs::write(&path, format!("{}\n", lines.join("\n"))).expect("the journal writes");
    path
}

/// Appends one committed message to a journal the way a live session would.
fn append_message(path: &Path, index: u64, message: &Value) {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("the journal file is named after the session");
    let line = envelope(
        id,
        index + 1,
        "MessageAppended",
        json!({ "index": index, "message": message }),
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("the journal opens");
    file.write_all(format!("{line}\n").as_bytes())
        .expect("the journal appends");
    file.flush().expect("the journal flushes");
}

fn user_message(text: &str) -> Value {
    json!({ "user": { "content": [{ "text": { "text": text } }] } })
}

/// The route's entries, as the fold computes them: the equality every transcript and conversation
/// test asserts.
fn fold_entries(path: &Path) -> Vec<Value> {
    let fold = AttachFold::open(path, None).expect("the fold opens");
    fold.entries()
        .iter()
        .map(|entry| serde_json::to_value(entry).expect("an entry is JSON"))
        .collect()
}

fn acp_entries(lines: &[String]) -> Vec<Value> {
    let mut fold = AcpFold::new();
    for line in lines {
        fold.push_line(line);
    }
    fold.entries()
        .iter()
        .map(|entry| serde_json::to_value(entry).expect("an entry is JSON"))
        .collect()
}

/// A real attempt's `result.jsonl`, shrunk to the lines that shape the conversation.
fn result_jsonl() -> Vec<String> {
    let update = |session_update: Value| {
        json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "s", "update": session_update},
            },
        })
        .to_string()
    };
    let chunk = |kind: &str, text: &str| {
        update(json!({
            "sessionUpdate": kind,
            "content": {"type": "text", "text": text},
        }))
    };
    vec![
        chunk("agent_thought_chunk", "Looking at the file."),
        update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "c1",
            "title": "Read main.rs",
            "kind": "read",
            "status": "pending",
        })),
        update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "1: fn main() {}"}}],
        })),
        chunk("agent_message_chunk", "Done."),
        json!({"type": "Result", "outcome": "completed", "text": "finished"}).to_string(),
    ]
}

/// The event stream as a client sees it: the status and headers are checked when it opens, then
/// each line of the chunked body is decoded as it arrives.
struct EventStream {
    reader: BufReader<TcpStream>,
    pending: Vec<u8>,
    done: bool,
}

impl EventStream {
    fn open(hub: &Hub, path: &str) -> Self {
        let mut stream =
            TcpStream::connect(("127.0.0.1", hub.port)).expect("the hub accepts a connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("a read timeout is settable");
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
            hub.port,
            hub.token()
        );
        stream
            .write_all(request.as_bytes())
            .expect("the request is written");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .expect("the event response's status line reads");
        assert!(status.contains(" 200 "), "the stream opens: {status:?}");
        let mut event_stream = false;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("the event response's header block reads");
            if line.is_empty() || line == "\r\n" || line == "\n" {
                break;
            }
            let header = line.to_ascii_lowercase();
            if header.starts_with("content-type:") {
                event_stream = header.contains("text/event-stream");
            }
        }
        assert!(event_stream, "the content type is text/event-stream");
        Self {
            reader,
            pending: Vec::new(),
            done: false,
        }
    }

    /// Reads one transfer-coding chunk into `pending`; `false` at the end of the body or when
    /// nothing arrives within the read timeout.
    fn fill(&mut self) -> bool {
        if self.done {
            return false;
        }
        loop {
            let mut size_line = String::new();
            match self.reader.read_line(&mut size_line) {
                Ok(0) => {
                    self.done = true;
                    return false;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
            let text = size_line.trim();
            if text.is_empty() {
                continue;
            }
            let Ok(size) = usize::from_str_radix(text, 16) else {
                return false;
            };
            if size == 0 {
                self.done = true;
                return false;
            }
            let mut chunk = vec![0u8; size];
            if self.reader.read_exact(&mut chunk).is_err() {
                return false;
            }
            let mut ending = [0u8; 2];
            if self.reader.read_exact(&mut ending).is_err() {
                return false;
            }
            self.pending.extend(chunk);
            return true;
        }
    }

    /// The next body line without its ending, or `None` at the end of the body or on a timeout.
    fn line(&mut self) -> Option<String> {
        loop {
            if let Some(at) = self.pending.iter().position(|byte| *byte == b'\n') {
                let bytes: Vec<u8> = self.pending.drain(..=at).collect();
                let mut text = String::from_utf8(bytes).expect("the stream is UTF-8");
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
                return Some(text);
            }
            if !self.fill() {
                if self.pending.is_empty() {
                    return None;
                }
                let bytes = std::mem::take(&mut self.pending);
                let mut text = String::from_utf8(bytes).expect("the stream is UTF-8");
                if text.ends_with('\r') {
                    text.pop();
                }
                return Some(text);
            }
        }
    }

    /// The next event's name and data, skipping `: keep-alive` comments; `None` when nothing
    /// arrives within `within`.
    fn next(&mut self, within: Duration) -> Option<(String, Value)> {
        let _ = self.reader.get_ref().set_read_timeout(Some(within));
        loop {
            let line = self.line()?;
            let Some(name) = line.strip_prefix("event: ") else {
                continue;
            };
            let data = self
                .line()
                .expect("an event has a data line")
                .strip_prefix("data: ")
                .expect("the data line starts with `data: `")
                .to_owned();
            assert_eq!(
                self.line().as_deref(),
                Some(""),
                "an event ends with a blank line"
            );
            let value = serde_json::from_str(&data)
                .unwrap_or_else(|error| panic!("the event data is JSON: {error}: {data:?}"));
            return Some((name.to_owned(), value));
        }
    }

    /// The next event whose name is `name`, skipping the others; `None` on the timeout.
    fn next_named(&mut self, name: &str, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            let (seen, value) = self.next(left)?;
            if seen == name {
                return Some(value);
            }
        }
        None
    }
}

/// Polls `check` until it returns `Some`, or panics after `within`.
fn eventually<T>(within: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "the condition never held");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---- edge 4: the transcript is the moved attach fold ---------------------------------------

#[test]
fn a_transcript_is_the_moved_attach_folds_entries() {
    let repo = Repo::new("hub-sessions-transcript");
    arc_store(&repo);
    register(&repo.home, "repo", &repo.canonical_clone());
    let id = "session-transcript";
    let path = write_journal(
        &repo.sessions,
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[
            user_message("what does the runner do?"),
            json!({ "assistant": {
                "content": [{ "text": { "text": "It runs the loop." } }],
                "tool_calls": [{ "id": "c1", "name": "shell", "arguments": {"command": "ls"} }],
            }}),
            json!({ "tool": { "result": {
                "call_id": "c1",
                "content": [{ "text": { "text": "file.txt" } }],
                "is_error": false,
            }}}),
        ],
    );
    let hub = Hub::over(&repo);

    let response = hub.get(&format!("/v1/repos/repo/sessions/{id}/transcript"));
    assert_eq!(response.status, 200, "{}", response.error());
    let body = response.json();
    assert_eq!(body["session"], json!(id));
    assert_eq!(
        body["entries"],
        json!(fold_entries(&path)),
        "the route is exactly the walk `radian attach` makes over the same journal"
    );

    // The list route carries the same session: ended, idle, with the first user message as the
    // title and no model route (no process reports one).
    let listed = hub.get("/v1/repos/repo/sessions");
    assert_eq!(listed.status, 200, "{}", listed.error());
    let sessions = listed.json()["sessions"].clone();
    let row = sessions
        .as_array()
        .expect("the list is an array")
        .iter()
        .find(|row| row["id"] == json!(id))
        .unwrap_or_else(|| panic!("the session is listed: {sessions}"))
        .clone();
    assert_eq!(row["live"], json!("none"));
    assert_eq!(row["state"], json!("idle"));
    assert_eq!(row["pending"], json!(0));
    assert_eq!(row["title"], json!("what does the runner do?"));
    assert_eq!(row["model"], Value::Null);
    assert_eq!(row["created"], json!("2026-11-02T00:00:00.000000Z"));

    // An unknown session, and a session id that cannot be one, are the hub's 404.
    let unknown = hub.get("/v1/repos/repo/sessions/no-such-session/transcript");
    assert_eq!(unknown.status, 404);
    assert!(
        unknown.error().contains("no session"),
        "{}",
        unknown.error()
    );
    let bad = hub.get("/v1/repos/repo/sessions/..%2Fescape/transcript");
    assert_eq!(bad.status, 404);

    // An unknown repository is the CLI's own message.
    let unknown_repo = hub.get("/v1/repos/nope/sessions");
    assert_eq!(unknown_repo.status, 404);
    assert!(
        unknown_repo.error().contains("no repository named `nope`"),
        "{}",
        unknown_repo.error()
    );
}

// ---- edge 8: the conversation is the ACP fold of the attempt ------------------------------

#[test]
fn a_conversation_is_the_acp_fold_of_the_attempts_result_jsonl() {
    let repo = Repo::new("hub-sessions-conversation");
    let store = arc_store(&repo);
    register(&repo.home, "repo", &repo.canonical_clone());
    let plan = store
        .add_plan("hub", "the hub plan", "a request\n")
        .expect("the plan adds");
    let task = store
        .add_task(
            &plan,
            "one",
            "the one task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("the task adds");
    let run = radian_arc::store::attempt_ops::run_dir(&store, &task, 1);
    fs::create_dir_all(&run).expect("the attempt's run directory is creatable");
    let lines = result_jsonl();
    write(
        &run.join("result.jsonl"),
        &format!("{}\n", lines.join("\n")),
    );
    let hub = Hub::over(&repo);

    let response = hub.get(&format!("/v1/repos/repo/items/{task}/conversation"));
    assert_eq!(response.status, 200, "{}", response.error());
    let body = response.json();
    assert_eq!(body["task"], json!(task));
    assert_eq!(body["attempt"], json!(1));
    assert_eq!(
        body["entries"],
        json!(acp_entries(&lines)),
        "the route folds the attempt's `result.jsonl` exactly as the arc TUI's page does"
    );

    // `?attempt=<n>` picks an attempt; the only one there is.
    let picked = hub.get(&format!(
        "/v1/repos/repo/items/{task}/conversation?attempt=1"
    ));
    assert_eq!(picked.status, 200, "{}", picked.error());
    assert_eq!(picked.json(), body);

    // An unknown attempt number and an unknown task are 404; a bad parameter is 400.
    let missing = hub.get(&format!(
        "/v1/repos/repo/items/{task}/conversation?attempt=7"
    ));
    assert_eq!(missing.status, 404);
    assert!(
        missing.error().contains("no attempt 7"),
        "{}",
        missing.error()
    );
    let unknown_task = hub.get("/v1/repos/repo/items/TK9999-nope/conversation");
    assert_eq!(unknown_task.status, 404);
    let bad = hub.get(&format!(
        "/v1/repos/repo/items/{task}/conversation?attempt=x"
    ));
    assert_eq!(bad.status, 400);
    assert!(bad.error().contains("attempt"), "{}", bad.error());
}

// ---- edge 8: a task with no attempt is an empty conversation ------------------------------

#[test]
fn a_task_without_an_attempt_is_an_empty_conversation() {
    let repo = Repo::new("hub-sessions-no-attempt");
    let store = arc_store(&repo);
    register(&repo.home, "repo", &repo.canonical_clone());
    let plan = store
        .add_plan("hub", "the hub plan", "a request\n")
        .expect("the plan adds");
    let task = store
        .add_task(
            &plan,
            "one",
            "the one task",
            Vec::new(),
            false,
            Vec::new(),
            "b\n",
        )
        .expect("the task adds");
    let hub = Hub::over(&repo);
    let response = hub.get(&format!("/v1/repos/repo/items/{task}/conversation"));
    assert_eq!(response.status, 200, "{}", response.error());
    assert_eq!(
        response.json(),
        json!({ "task": task, "attempt": Value::Null, "entries": [] })
    );
}

// ---- edge 7: a dead socket is not a session -----------------------------------------------

#[test]
fn a_stale_socket_file_of_a_dead_pid_is_never_listed() {
    let repo = Repo::new("hub-sessions-dead-socket");
    arc_store(&repo);
    register(&repo.home, "repo", &repo.canonical_clone());
    // A stale socket file of a pid that no longer exists, and a file that only looks like one.
    let run = repo.home.join("run");
    fs::create_dir_all(&run).expect("the run directory is creatable");
    fs::write(run.join("2147483647.sock"), b"").expect("the stale socket file writes");
    fs::write(run.join("notes.sock"), b"").expect("the other file writes");
    let hub = Hub::over(&repo);

    // The list answers at once with no session at all: no socket was probed, because neither
    // file names a live pid.
    let listed = hub.get("/v1/repos/repo/sessions");
    assert_eq!(listed.status, 200, "{}", listed.error());
    assert_eq!(listed.json(), json!({ "sessions": [] }));
}

/// Edge 7 on the socket side: a session that was listed leaves the list on the next scan once
/// its process is gone.
#[test]
fn a_socket_whose_process_died_is_dropped_on_the_next_scan() {
    let repo = Repo::new("hub-sessions-died");
    arc_store(&repo);
    let short = ShortHome::new("died");
    register(&short.path().join("home"), "repo", &repo.canonical_clone());
    let id = "session-died";
    write_journal(
        &short.path().join("home/sessions"),
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[user_message("about to die")],
    );
    let hub = Hub::in_short_home(short.path(), &repo, &[]);
    let pid = LivePid::new();
    let number = pid.pid();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        number,
        FakeInner {
            id: id.to_owned(),
            workspace: repo.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    eventually(Duration::from_secs(30), || {
        let body = hub.get("/v1/repos/repo/sessions").json();
        body["sessions"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["live"] == json!("tui")))
            .then_some(())
    });

    // The process the socket is named after dies; the file it left behind names no live pid.
    drop(pid);
    eventually(Duration::from_secs(30), || {
        let body = hub.get("/v1/repos/repo/sessions").json();
        let rows = body["sessions"].as_array().cloned().unwrap_or_default();
        rows.iter()
            .find(|row| row["id"] == json!(id))
            .is_some_and(|row| row["live"] == json!("none"))
            .then_some(())
    });
    assert!(
        fake.path.exists(),
        "the stale socket file is still there: the list dropped it, the scanner did not need to\n         remove it"
    );
}

// ---- edge 9: a version 1 client still reads every version 1 route the same way -------------

#[test]
fn every_version_one_route_still_answers_and_the_version_is_two() {
    let repo = Repo::new("hub-sessions-v1");
    let store = arc_store(&repo);
    register(&repo.home, "repo", &repo.canonical_clone());
    let plan = store
        .add_plan("hub", "the hub plan", "a request\n")
        .expect("the plan adds");
    let _task = store
        .add_task(
            &plan,
            "one",
            "the one task",
            Vec::new(),
            false,
            Vec::new(),
            "b\n",
        )
        .expect("the task adds");
    let hub = Hub::over(&repo);

    let version = hub.get("/v1/version");
    assert_eq!(version.status, 200);
    assert_eq!(
        version.json(),
        json!({ "api_version": 2, "schema_version": 1 })
    );

    // The version 1 read routes answer exactly what they answered before the session surface
    // existed: the CLI's bytes, the registry array, and the event stream's content type.
    let items = hub.get("/v1/repos/repo/items");
    assert_eq!(items.status, 200);
    let cli = repo.radian(&["arc", "list", "--json"]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(items.body, cli.stdout.as_bytes());
    let repos = hub.get("/v1/repos");
    assert_eq!(repos.status, 200);
    assert_eq!(repos.json()[0]["name"], json!("repo"));
    // The event stream still opens as `text/event-stream`; its frames are unchanged (the other
    // hub tests compare them).
    let _events = EventStream::open(&hub, "/v1/events");
}

// ---- the router's method and unknown-route answers ---------------------------------------

#[test]
fn the_session_read_routes_take_get_the_write_routes_post_and_unknown_paths_are_404() {
    let repo = Repo::new("hub-sessions-router");
    arc_store(&repo);
    register(&repo.home, "repo", &repo.canonical_clone());
    let hub = Hub::over(&repo);
    let token = format!("Bearer {}", hub.token());
    // A read route, or the conversation: `GET` only.
    for path in [
        "/v1/repos/repo/sessions/x/transcript",
        "/v1/repos/repo/sessions/x/events",
        "/v1/repos/repo/sessions/x/models",
        "/v1/repos/repo/items/TK0001-x/conversation",
    ] {
        let post = http(hub.port, "POST", path, Some(&token));
        assert_eq!(post.status, 405, "{path}");
        let no_token = http(hub.port, "GET", path, None);
        assert_eq!(no_token.status, 401, "{path}");
    }
    // `/sessions` takes both: a `GET` lists, a `POST` starts one. A `POST` with no body is a
    // body error (`400`), never `405`, and it starts no process.
    assert_eq!(
        http(hub.port, "GET", "/v1/repos/repo/sessions", Some(&token)).status,
        200
    );
    let empty_create = http(hub.port, "POST", "/v1/repos/repo/sessions", Some(&token));
    assert_eq!(empty_create.status, 400, "{:?}", empty_create.body);
    // The four forwarded control verbs are `POST` only.
    for suffix in ["prompt", "cancel", "answer", "config"] {
        let path = format!("/v1/repos/repo/sessions/x/{suffix}");
        let get = http(hub.port, "GET", &path, Some(&token));
        assert_eq!(get.status, 405, "{path}");
        let no_token = http(hub.port, "POST", &path, None);
        assert_eq!(no_token.status, 401, "{path}");
    }
    let unknown = hub.get("/v1/repos/repo/sessions/x/nope");
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.error(), "no such route");
}

// ---- edge 1: each repository lists only its own live session ------------------------------

#[test]
fn each_repository_lists_only_its_own_live_session() {
    let a = Repo::new("hub-sessions-live-a");
    let b = Repo::new("hub-sessions-live-b");
    arc_store(&a);
    arc_store(&b);
    let short = ShortHome::new("two-repos");
    register(&short.path().join("home"), "a", &a.canonical_clone());
    register(&short.path().join("home"), "b", &b.canonical_clone());
    let pid_a = LivePid::new();
    let pid_b = LivePid::new();
    let hub = Hub::in_short_home(short.path(), &a, &[]);
    let run = short.path().join("home/run");
    let id_a = "session-live-a";
    let id_b = "session-live-b";
    write_journal(
        &hub.sessions,
        id_a,
        &a.canonical_clone().to_string_lossy(),
        &[user_message("from a")],
    );
    write_journal(
        &hub.sessions,
        id_b,
        &b.canonical_clone().to_string_lossy(),
        &[user_message("from b")],
    );
    let fake_a = FakePort::bind(
        &run,
        pid_a.pid(),
        FakeInner {
            id: id_a.to_owned(),
            workspace: a.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 1,
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-4".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    let _fake_b = FakePort::bind(
        &run,
        pid_b.pid(),
        FakeInner {
            id: id_b.to_owned(),
            workspace: b.canonical_clone().to_string_lossy().into_owned(),
            state: "idle".to_owned(),
            pending: 0,
            provider: "opencode-go".to_owned(),
            model: "deepseek-v4.1-flash".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );

    let listed = eventually(Duration::from_secs(30), || {
        let body = hub.get("/v1/repos/a/sessions").json();
        let rows = body["sessions"].as_array().cloned().unwrap_or_default();
        rows.iter()
            .any(|row| row["id"] == json!(id_a) && row["live"] == json!("tui"))
            .then_some(body)
    });
    let rows = listed["sessions"].as_array().expect("the list is an array");
    assert_eq!(rows.len(), 1, "a lists only its own session: {listed}");
    assert_eq!(rows[0]["id"], json!(id_a));
    assert_eq!(rows[0]["live"], json!("tui"));
    assert_eq!(rows[0]["state"], json!("working"));
    assert_eq!(rows[0]["pending"], json!(1));
    assert_eq!(
        rows[0]["model"],
        json!({ "provider": "anthropic", "model": "claude-sonnet-4" })
    );
    assert_eq!(rows[0]["title"], json!("from a"));

    let other = hub.get("/v1/repos/b/sessions").json();
    let rows = other["sessions"].as_array().expect("the list is an array");
    assert_eq!(rows.len(), 1, "b lists only its own session: {other}");
    assert_eq!(rows[0]["id"], json!(id_b));
    assert_eq!(rows[0]["live"], json!("tui"));
    assert_eq!(rows[0]["state"], json!("idle"));
    assert_eq!(rows[0]["pending"], json!(0));
    assert_eq!(
        rows[0]["model"],
        json!({ "provider": "opencode-go", "model": "deepseek-v4.1-flash" })
    );
    // The hub subscribed to each live session, with the token (the fake answers any subscribe).
    fake_a.wait_for_subscription(Instant::now() + Duration::from_secs(30));
}

// ---- edge 2: a live session in an unregistered workspace is not listed --------------------

#[test]
fn a_live_session_in_an_unregistered_workspace_is_not_listed_anywhere() {
    let registered = Repo::new("hub-sessions-registered");
    let stray = Repo::new("hub-sessions-stray");
    arc_store(&registered);
    arc_store(&stray);
    let short = ShortHome::new("stray");
    register(
        &short.path().join("home"),
        "repo",
        &registered.canonical_clone(),
    );
    let id = "session-stray";
    write_journal(
        &short.path().join("home/sessions"),
        id,
        &stray.canonical_clone().to_string_lossy(),
        &[user_message("outside the registry")],
    );
    let hub = Hub::in_short_home(short.path(), &registered, &[]);
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: stray.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));

    // The one registered repository lists nothing at all: the stray session's journal names
    // another workspace, and its socket's workspace is not registered either.
    let listed = hub.get("/v1/repos/repo/sessions").json();
    assert_eq!(listed, json!({ "sessions": [] }), "nothing is shown");
    // Its routes are not reachable through the registered repository's name either.
    let transcript = hub.get(&format!("/v1/repos/repo/sessions/{id}/transcript"));
    assert_eq!(transcript.status, 404);
}

// ---- edge 3: a symlinked workspace spelling still matches its repository -------------------

#[test]
fn a_workspace_spelled_through_a_symlink_matches_its_repository() {
    let repo = Repo::new("hub-sessions-symlink");
    arc_store(&repo);
    let short = ShortHome::new("symlink");
    register(&short.path().join("home"), "repo", &repo.canonical_clone());
    // The repository is registered under its real path; the session was opened under a
    // symlinked spelling of it, which is what a pre-canonicalisation build or a symlinked
    // project directory records.
    let link = repo.root.join("link");
    std::os::unix::fs::symlink(&repo.clone, &link).expect("the symlink is creatable");
    let id = "session-link";
    write_journal(
        &short.path().join("home/sessions"),
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[user_message("through the link")],
    );
    let hub = Hub::in_short_home(short.path(), &repo, &[]);
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: link.to_string_lossy().into_owned(),
            state: "idle".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));

    let listed = hub.get("/v1/repos/repo/sessions").json();
    let rows = listed["sessions"].as_array().expect("the list is an array");
    let row = rows
        .iter()
        .find(|row| row["id"] == json!(id))
        .unwrap_or_else(|| panic!("the linked session is listed: {listed}"));
    assert_eq!(
        row["live"],
        json!("tui"),
        "the symlink is the same directory"
    );
}

// ---- edge 5: a committed message arrives as one entry event -------------------------------

#[test]
fn a_message_committed_in_the_session_arrives_as_one_entry_event() {
    let repo = Repo::new("hub-sessions-event");
    arc_store(&repo);
    let short = ShortHome::new("event");
    register(&short.path().join("home"), "repo", &repo.canonical_clone());
    let id = "session-event";
    let journal = write_journal(
        &short.path().join("home/sessions"),
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[user_message("first")],
    );
    // The session is live: its process holds the journal's writer lock.
    let _writer = JournalLock::take(&journal);
    let hub = Hub::in_short_home(short.path(), &repo, &[]);
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: repo.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    let mut stream = EventStream::open(&hub, &format!("/v1/repos/repo/sessions/{id}/events"));

    // A committed message: the journal gains it, and the port pushes the event that says so.
    append_message(&journal, 1, &user_message("second"));
    fake.push(json!({
        "source": "agent",
        "name": "transcript_appended",
        "data": { "index": 1, "message": user_message("second") },
    }));

    let event = stream
        .next_named("entry", Duration::from_secs(20))
        .expect("the stream carries the committed entry");
    let entry = &event["entry"];
    assert_eq!(entry["kind"], json!("user"));
    assert_eq!(entry["text"], json!("second"));
    assert_eq!(event["index"], json!(1));
    assert_eq!(event["reset"], json!(false));

    // The transcript route already has it too: the stream and the route fold one journal.
    let transcript = hub
        .get(&format!("/v1/repos/repo/sessions/{id}/transcript"))
        .json();
    assert_eq!(transcript["entries"][1], json!(entry.clone()));
}

// ---- edge 6: a message committed while disconnected is in the re-read transcript ----------

#[test]
fn a_message_committed_while_disconnected_is_in_the_reread_transcript() {
    let repo = Repo::new("hub-sessions-reconnect");
    arc_store(&repo);
    let short = ShortHome::new("reconnect");
    register(&short.path().join("home"), "repo", &repo.canonical_clone());
    let id = "session-reconnect";
    let journal = write_journal(
        &short.path().join("home/sessions"),
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[user_message("before")],
    );
    let _writer = JournalLock::take(&journal);
    let hub = Hub::in_short_home(short.path(), &repo, &[]);
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: repo.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    let path = format!("/v1/repos/repo/sessions/{id}/events");
    let first = EventStream::open(&hub, &path);
    // The client goes away before the message is committed.
    drop(first);
    // A moment for the hub to see the closed connection, so the message really is committed
    // while no client is reading; this waits on no result.
    std::thread::sleep(Duration::from_millis(200));
    append_message(&journal, 1, &user_message("meanwhile"));
    fake.push(json!({
        "source": "agent",
        "name": "transcript_appended",
        "data": { "index": 1, "message": user_message("meanwhile") },
    }));

    // The reconnected stream does not replay: the rule is to re-read the transcript first.
    let mut second = EventStream::open(&hub, &path);
    assert!(
        second
            .next_named("entry", Duration::from_millis(500))
            .is_none(),
        "the stream never replays an entry"
    );
    let transcript = hub
        .get(&format!("/v1/repos/repo/sessions/{id}/transcript"))
        .json();
    let entries = transcript["entries"]
        .as_array()
        .expect("entries are an array");
    assert!(
        entries
            .iter()
            .any(|entry| entry["text"] == json!("meanwhile")),
        "the re-read transcript has the message: {transcript}"
    );
}

// ---- the hub's subscription reconnects when its connection drops --------------------------

#[test]
fn a_dropped_subscription_is_reconnected_and_its_events_still_arrive() {
    let repo = Repo::new("hub-sessions-resubscribe");
    arc_store(&repo);
    let short = ShortHome::new("resubscribe");
    register(&short.path().join("home"), "repo", &repo.canonical_clone());
    let id = "session-resubscribe";
    let journal = write_journal(
        &short.path().join("home/sessions"),
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[user_message("first")],
    );
    let _writer = JournalLock::take(&journal);
    // The fake closes the first subscription right after its acknowledgement.
    let hub = Hub::in_short_home(short.path(), &repo, &[]);
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: repo.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(true),
            silent_probes: AtomicUsize::new(0),
        },
    );
    // The hub's first subscription is dropped by the fake; only the second one is live.
    eventually(Duration::from_secs(30), || {
        (fake.subscriptions() >= 2).then_some(())
    });
    std::thread::sleep(Duration::from_millis(100));
    let mut stream = EventStream::open(&hub, &format!("/v1/repos/repo/sessions/{id}/events"));
    append_message(&journal, 1, &user_message("after the reconnect"));
    fake.push(json!({
        "source": "agent",
        "name": "transcript_appended",
        "data": { "index": 1, "message": user_message("after the reconnect") },
    }));
    let event = stream
        .next_named("entry", Duration::from_secs(20))
        .unwrap_or_else(|| panic!("the reconnected subscription carries the event"));
    assert_eq!(event["entry"]["text"], json!("after the reconnect"));
}

// ---- a committed ConfigChanged record arrives as one `config` frame -----------------------

#[test]
fn a_config_change_in_the_session_arrives_as_one_config_frame() {
    let repo = Repo::new("hub-sessions-config");
    arc_store(&repo);
    let short = ShortHome::new("config");
    register(&short.path().join("home"), "repo", &repo.canonical_clone());
    let id = "session-config";
    let journal = write_journal(
        &short.path().join("home/sessions"),
        id,
        &repo.canonical_clone().to_string_lossy(),
        &[user_message("first")],
    );
    let _writer = JournalLock::take(&journal);
    let hub = Hub::in_short_home(short.path(), &repo, &[]);
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &short.path().join("home/run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: repo.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    let mut stream = EventStream::open(&hub, &format!("/v1/repos/repo/sessions/{id}/events"));

    // The line the control port fans out for a `/model` or `config` change: a `RuntimeEvent`
    // carries its v1 disk name (`ConfigChanged`), never a lowercase spelling.
    fake.push(json!({
        "source": "runtime",
        "name": "ConfigChanged",
        "data": { "revision": 2, "change": { "model": { "provider": "p", "model": "m" } } },
    }));

    let event = stream
        .next_named("config", Duration::from_secs(20))
        .expect("a committed ConfigChanged record is one config frame");
    assert_eq!(event["revision"], json!(2));
    assert_eq!(event["change"]["model"]["provider"], json!("p"));
    assert_eq!(event["change"]["model"]["model"], json!("m"));
}

// ---- edge 10: a slow socket delays no request --------------------------------------------

#[test]
fn a_slow_socket_delays_no_version_and_no_other_repositorys_listing() {
    let quiet = Repo::new("hub-sessions-slow-quiet");
    let busy = Repo::new("hub-sessions-slow-busy");
    arc_store(&quiet);
    arc_store(&busy);
    let short = ShortHome::new("slow");
    register(
        &short.path().join("home"),
        "quiet",
        &quiet.canonical_clone(),
    );
    register(&short.path().join("home"), "busy", &busy.canonical_clone());
    let id = "session-busy";
    write_journal(
        &short.path().join("home/sessions"),
        id,
        &busy.canonical_clone().to_string_lossy(),
        &[user_message("busy")],
    );
    let run = short.path().join("home/run");
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &run,
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: busy.canonical_clone().to_string_lossy().into_owned(),
            state: "idle".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    // The scanner lives in the hub, so the subscription can only appear once the hub is up.
    let hub = Hub::in_short_home(short.path(), &quiet, &[]);
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    // A socket that accepts and never answers, named after a live pid so the scanner probes it.
    let slow_pid = LivePid::new();
    let slow = bind_silent_socket(&run, slow_pid.pid());
    // The scanner finds the silent socket on a later scan, so wait for that scan to reach the
    // probe: only then is the scanner really blocked while the requests are timed.
    slow.wait_for_probe(Instant::now() + Duration::from_secs(30));

    // The scanner is blocked on the silent socket's probe; the routes answer anyway.
    let started = Instant::now();
    let version = hub.get("/v1/version");
    assert_eq!(version.status, 200);
    assert_eq!(version.json()["api_version"], json!(2));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "the version route does not wait for a session's port: {elapsed:?}"
    );
    let started = Instant::now();
    let listed = hub.get("/v1/repos/quiet/sessions");
    assert_eq!(listed.status, 200, "{}", listed.error());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a listing does not wait for another session's port"
    );
    // The busy repository's session is still discovered, after the silent socket's timeout.
    eventually(Duration::from_secs(30), || {
        let body = hub.get("/v1/repos/busy/sessions").json();
        body["sessions"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["live"] == json!("tui")))
            .then_some(())
    });
}

/// A Unix socket at `<run dir>/<pid>.sock` that accepts connections and never answers, so the
/// scanner's probe runs into its timeout.
struct SilentSocket {
    path: PathBuf,
    /// How many connections were accepted: the scanner is inside its probe from the first one
    /// until the probe's timeout expires.
    accepted: Arc<AtomicUsize>,
}

impl SilentSocket {
    /// Waits until the scanner has connected, so the requests timed afterwards run while the
    /// scanner is blocked on the silent socket's answer.
    fn wait_for_probe(&self, deadline: Instant) {
        while self.accepted.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "the scanner never probed {}",
                self.path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for SilentSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn bind_silent_socket(run_dir: &Path, pid: u32) -> SilentSocket {
    fs::create_dir_all(run_dir).expect("the run directory is creatable");
    let path = run_dir.join(format!("{pid}.sock"));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("the silent socket binds");
    let accepted = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&accepted);
    std::thread::spawn(move || {
        let mut held: Vec<UnixStream> = Vec::new();
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            seen.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    SilentSocket { path, accepted }
}

// ---- `[hub.hooks] after_pending` (ADR 0048 §16; backlog decision 5) ------------------------

/// Writes `[hub.hooks] after_pending` into the hub's user configuration under `home`
/// (`$RADIAN_HOME/config/config.toml`).
fn write_hub_hooks(home: &Path, commands: &[String]) {
    let dir = home.join("config");
    fs::create_dir_all(&dir).expect("the config directory is creatable");
    let mut text = String::from("schema_version = 1\n[hub.hooks]\nafter_pending = [");
    for (index, command) in commands.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        // Rust's debug form of a string is a TOML basic string for the characters these
        // tests use (quotes and backslashes escaped; no raw newline).
        text.push_str(&format!("{command:?}"));
    }
    text.push_str("]\n");
    fs::write(dir.join("config.toml"), text).expect("the hub configuration writes");
}

/// One live session on its own fake control port, in this hub's run directory.
fn live_session(home: &Path, repo: &Repo, id: &str) -> (LivePid, FakePort) {
    let pid = LivePid::new();
    let fake = FakePort::bind(
        &home.join("run"),
        pid.pid(),
        FakeInner {
            id: id.to_owned(),
            workspace: repo.canonical_clone().to_string_lossy().into_owned(),
            state: "working".to_owned(),
            pending: 0,
            provider: "p".to_owned(),
            model: "m".to_owned(),
            lines: Mutex::new(Vec::new()),
            subscriptions: AtomicUsize::new(0),
            drop_next: AtomicBool::new(false),
            silent_probes: AtomicUsize::new(0),
        },
    );
    (pid, fake)
}

/// One `pending` event as the control port pushes it to an owner subscriber.
fn pending(head: Value, length: u64) -> Value {
    json!({ "source": "control", "name": "pending", "data": { "head": head, "length": length } })
}

/// The hub's hook log.
fn hub_log(home: &Path) -> String {
    fs::read_to_string(home.join("hub/hub.log")).unwrap_or_default()
}

#[test]
fn a_new_approval_head_runs_the_hook_once_with_only_the_four_variables() {
    let repo = Repo::new("hub-hooks-approval");
    arc_store(&repo);
    let short = ShortHome::new("hook-approval");
    let home = short.path().join("home");
    register(&home, "repo", &repo.canonical_clone());
    let runs = repo.clone.join("hook-runs.txt");
    let env_file = repo.clone.join("hook-env.txt");
    write_hub_hooks(
        &home,
        &[format!(
            "printf 'run\\n' >> '{}'; env > '{}'",
            runs.display(),
            env_file.display()
        )],
    );
    let _hub = Hub::in_short_home(short.path(), &repo, &[]);
    let (_pid, fake) = live_session(&home, &repo, "session-approval");
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    fake.push(pending(
        json!({
            "request_id": 1,
            "kind": "approval",
            "tool": "shell",
            "arguments": { "command": "printf secret-argument" },
            "resource": "/secret-path/file",
            "resolved_resource": "/secret-path/file",
            "reason": "secret-reason",
        }),
        1,
    ));
    eventually(Duration::from_secs(30), || {
        fs::read_to_string(&runs)
            .ok()
            .filter(|text| !text.is_empty())
    });
    // The same head is not sent again, but give a wrong double fire time to show up.
    std::thread::sleep(Duration::from_millis(500));
    let count = fs::read_to_string(&runs)
        .expect("the hook wrote its run record")
        .lines()
        .count();
    assert_eq!(count, 1, "the hook ran once for one new head");

    let text = fs::read_to_string(&env_file).expect("the hook wrote its environment");
    for line in [
        "RADIAN_HUB_REPO=repo",
        "RADIAN_HUB_SESSION=session-approval",
        "RADIAN_HUB_PENDING_KIND=approval",
        "RADIAN_HUB_TOOL=shell",
    ] {
        assert!(
            text.lines().any(|candidate| candidate == line),
            "the environment lacks {line}: {text}"
        );
    }
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("RADIAN_HUB_"))
            .count(),
        radian_arc::hub::hooks::HOOK_ENV.len(),
        "the child sees exactly the four hook variables: {text}"
    );
    for secret in ["secret-argument", "secret-path", "secret-reason"] {
        assert!(
            !text.contains(secret),
            "the environment carries {secret} from the request: {text}"
        );
    }
}

/// Edge 2: the same head seen again after a subscription reconnect does not run twice.
#[test]
fn a_repeated_head_after_a_reconnect_runs_the_hook_once() {
    let repo = Repo::new("hub-hooks-reconnect");
    arc_store(&repo);
    let short = ShortHome::new("hook-reconnect");
    let home = short.path().join("home");
    register(&home, "repo", &repo.canonical_clone());
    let runs = repo.clone.join("hook-runs.txt");
    write_hub_hooks(&home, &[format!("printf 'run\\n' >> '{}'", runs.display())]);
    let _hub = Hub::in_short_home(short.path(), &repo, &[]);
    let (_pid, fake) = live_session(&home, &repo, "session-reconnect");
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    let head = pending(
        json!({ "request_id": 2, "kind": "approval", "tool": "shell" }),
        1,
    );
    fake.push(head.clone());
    eventually(Duration::from_secs(30), || {
        fs::read_to_string(&runs)
            .ok()
            .filter(|text| !text.is_empty())
    });

    // The connection drops and the hub subscribes again; the port repeats the same head.
    let before = fake.subscriptions();
    fake.disconnect();
    eventually(Duration::from_secs(30), || {
        (fake.subscriptions() > before).then_some(())
    });
    // The acknowledgement is written before the handler registers its sender.
    std::thread::sleep(Duration::from_millis(100));
    fake.push(head);
    std::thread::sleep(Duration::from_millis(500));

    let count = fs::read_to_string(&runs)
        .expect("the hook wrote its run record")
        .lines()
        .count();
    assert_eq!(count, 1, "the same request id is not a new head");
}

/// The review's trigger: a port that misses two `list_sessions` probes makes the scanner forget
/// its session; when it answers again the hub subscribes afresh, and the head it already ran for
/// is still not new.
#[test]
fn a_recovered_socket_does_not_run_the_hook_for_the_same_head_again() {
    let repo = Repo::new("hub-hooks-recovered");
    arc_store(&repo);
    let short = ShortHome::new("hook-recovered");
    let home = short.path().join("home");
    register(&home, "repo", &repo.canonical_clone());
    let runs = repo.clone.join("hook-runs.txt");
    write_hub_hooks(&home, &[format!("printf 'run\\n' >> '{}'", runs.display())]);
    let _hub = Hub::in_short_home(short.path(), &repo, &[]);
    let (_pid, fake) = live_session(&home, &repo, "session-recovered");
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    let head = pending(
        json!({ "request_id": 1, "kind": "approval", "tool": "shell" }),
        1,
    );
    fake.push(head.clone());
    eventually(Duration::from_secs(30), || {
        fs::read_to_string(&runs)
            .ok()
            .filter(|text| !text.is_empty())
    });

    // The port misses its next two probes, each one past `PROBE_TIMEOUT`: the scanner stops the
    // subscription and forgets the session. Then it answers again, the scanner records it as new
    // and subscribes afresh, and the port re-sends the head it already pushed.
    let before = fake.subscriptions();
    fake.silent_probes(2);
    eventually(Duration::from_secs(60), || {
        (fake.subscriptions() > before).then_some(())
    });
    // The acknowledgement is written before the handler registers its sender.
    std::thread::sleep(Duration::from_millis(100));
    fake.push(head);
    std::thread::sleep(Duration::from_millis(500));

    let count = fs::read_to_string(&runs)
        .expect("the hook wrote its run record")
        .lines()
        .count();
    assert_eq!(
        count, 1,
        "the head the hook already ran for is not new on a recovered socket"
    );
}

/// Edge 3: a question's head sets kind `question` and an empty tool.
#[test]
fn a_question_head_runs_the_hook_with_kind_question_and_an_empty_tool() {
    let repo = Repo::new("hub-hooks-question");
    arc_store(&repo);
    let short = ShortHome::new("hook-question");
    let home = short.path().join("home");
    register(&home, "repo", &repo.canonical_clone());
    let env_file = repo.clone.join("hook-env.txt");
    write_hub_hooks(&home, &[format!("env > '{}'", env_file.display())]);
    let _hub = Hub::in_short_home(short.path(), &repo, &[]);
    let (_pid, fake) = live_session(&home, &repo, "session-question");
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    fake.push(pending(
        json!({
            "request_id": 7,
            "kind": "question",
            "question": { "questions": [{ "question": "secret-question-text" }] },
        }),
        1,
    ));
    let text = eventually(Duration::from_secs(30), || {
        fs::read_to_string(&env_file)
            .ok()
            .filter(|text| text.contains("RADIAN_HUB_PENDING_KIND"))
    });
    assert!(
        text.lines()
            .any(|line| line == "RADIAN_HUB_PENDING_KIND=question"),
        "{text}"
    );
    assert!(
        text.lines().any(|line| line == "RADIAN_HUB_TOOL="),
        "a question carries an empty tool: {text}"
    );
    assert!(
        !text.contains("secret-question-text"),
        "the environment carries the question text: {text}"
    );
}

/// Edge 4: a hook past its timeout is killed and logged; another session's hook still runs.
#[test]
fn a_slow_hook_is_killed_at_the_timeout_and_another_sessions_hook_still_runs() {
    let repo = Repo::new("hub-hooks-timeout");
    // The timeout is the repository's arc `[gate] command_timeout_secs`, the rule arc's hooks
    // follow. Five seconds leaves the other session room to finish first.
    arc_store_with_timeout(&repo, 5);
    let short = ShortHome::new("hook-timeout");
    let home = short.path().join("home");
    register(&home, "repo", &repo.canonical_clone());
    let slept = repo.clone.join("slept.txt");
    let woke = repo.clone.join("woke.txt");
    write_hub_hooks(
        &home,
        &[format!(
            "if [ \"$RADIAN_HUB_SESSION\" = \"session-sleepy\" ]; then sleep 30; printf slept > '{}'; else printf woke > '{}'; fi",
            slept.display(),
            woke.display()
        )],
    );
    let _hub = Hub::in_short_home(short.path(), &repo, &[]);
    let (_sleepy_pid, sleepy) = live_session(&home, &repo, "session-sleepy");
    let (_brisk_pid, brisk) = live_session(&home, &repo, "session-brisk");
    sleepy.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    brisk.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    let head = || {
        pending(
            json!({ "request_id": 1, "kind": "approval", "tool": "shell" }),
            1,
        )
    };
    sleepy.push(head());
    brisk.push(head());

    // The slow session's command is on its own thread: the other one completes while it runs.
    eventually(Duration::from_secs(3), || woke.exists().then_some(()));
    eventually(Duration::from_secs(15), || {
        hub_log(&home).contains("timed out after 5 s").then_some(())
    });
    assert!(
        !slept.exists(),
        "the timed-out command was killed before its second step"
    );
    let log = hub_log(&home);
    assert!(
        log.contains("session-sleepy"),
        "the timeout is logged with the session: {log}"
    );
}

/// Edge 5: an emptied queue runs nothing.
#[test]
fn an_emptied_queue_runs_no_hook() {
    let repo = Repo::new("hub-hooks-empty");
    arc_store(&repo);
    let short = ShortHome::new("hook-empty");
    let home = short.path().join("home");
    register(&home, "repo", &repo.canonical_clone());
    let runs = repo.clone.join("hook-runs.txt");
    write_hub_hooks(&home, &[format!("printf 'run\\n' >> '{}'", runs.display())]);
    let _hub = Hub::in_short_home(short.path(), &repo, &[]);
    let (_pid, fake) = live_session(&home, &repo, "session-empty");
    fake.wait_for_subscription(Instant::now() + Duration::from_secs(30));
    fake.push(pending(
        json!({ "request_id": 3, "kind": "approval", "tool": "shell" }),
        1,
    ));
    eventually(Duration::from_secs(30), || {
        fs::read_to_string(&runs)
            .ok()
            .filter(|text| !text.is_empty())
    });
    fake.push(pending(Value::Null, 0));
    std::thread::sleep(Duration::from_millis(500));
    let count = fs::read_to_string(&runs)
        .expect("the hook wrote its run record")
        .lines()
        .count();
    assert_eq!(count, 1, "an emptied queue is not a new head");
}

/// An initialized arc store with a chosen `[gate] command_timeout_secs`.
fn arc_store_with_timeout(repo: &Repo, secs: u64) -> Store {
    let root = repo.arc();
    fs::create_dir_all(&root).expect("the arc directory is creatable");
    let text = config::default_text("main").replace(
        "command_timeout_secs = 3600",
        &format!("command_timeout_secs = {secs}"),
    );
    fs::write(root.join("config.toml"), text).expect("the config writes");
    Store::at(repo.clone.clone())
}

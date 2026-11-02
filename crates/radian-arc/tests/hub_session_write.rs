//! The hub's session write routes and its hub-held processes (ADR 0048 §16;
//! `docs/backlog/hub-sessions.md` decisions 3 and 4, the write half): starting a session,
//! prompting (live and ended), cancelling, answering the pending queue, configuration, the
//! one-process-per-journal rule, idle reclaim and hub shutdown.
//!
//! Every test runs a real `radian hub` and real `radian exec --hub-held` children, so the
//! home must be short enough for `run/<pid>.sock` (`sun_path` is 104 bytes on macOS) and
//! writable inside the workspace sandbox: it is a short directory beside the worktree root,
//! not under `/tmp` and not under `$TMPDIR` (whose sandbox spelling is far too long). The
//! repository clone itself stays in `$TMPDIR`; only the home's path length matters.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{Reply, chunk};
use common::{Mock, Repo, fast_catalog_url, radian_bin};
use radian_arc::hub::Registry;
use serde_json::{Value, json};

/// The longest any wait in this suite is allowed to take.
const WAIT: Duration = Duration::from_secs(30);

// ---- a short home inside the workspace ------------------------------------------------

/// A home directory whose path is short enough for a control socket. It sits beside the
/// worktree root (the only writable tree in the `workspace` sandbox) and is removed on drop.
struct ShortHome(PathBuf);

impl ShortHome {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        // `h<pid><n>`: five to seven digits of pid and one test counter, so two test processes
        // never share a name and `<home>/run/<pid>.sock` stays under `sun_path`.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the crate sits inside the worktree");
        let path = root.join(format!("h{}{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("config")).expect("the home's config directory is creatable");
        fs::create_dir_all(path.join("sessions"))
            .expect("the home's session directory is creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn sessions(&self) -> PathBuf {
        self.0.join("sessions")
    }

    fn run(&self) -> PathBuf {
        self.0.join("run")
    }

    fn journal(&self, id: &str) -> PathBuf {
        self.sessions().join(format!("{id}.jsonl"))
    }

    fn write_config(&self, base_url: &str, extra: &str) {
        let text = format!(
            "schema_version = 1\n\
             [model]\n\
             provider = \"openai-compatible\"\n\
             base_url = \"{base_url}\"\n\
             model = \"m\"\n\
             {extra}"
        );
        fs::write(self.0.join("config").join("config.toml"), text)
            .expect("the home's configuration writes");
    }
}

impl Drop for ShortHome {
    fn drop(&mut self) {
        for _ in 0..100 {
            match fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

// ---- the hub under test ----------------------------------------------------------------

struct Hub {
    child: Child,
    _stdout: BufReader<std::process::ChildStdout>,
    port: u16,
    home: PathBuf,
}

impl Hub {
    fn start(repo: &Repo, home: &ShortHome, envs: &[(&str, &str)]) -> Self {
        let stderr = fs::File::create(home.path().join("hub.stderr.log"))
            .expect("the hub stderr file is creatable");
        let mut command = Command::new(radian_bin());
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .env("RADIAN_HOME", home.path())
            .env("RADIAN_SESSION_DIR", home.sessions())
            .env("RADIAN_CATALOG_URL", fast_catalog_url())
            .current_dir(&repo.clone)
            .args(["hub", "--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmp);
        }
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
            home: home.path().to_path_buf(),
        }
    }

    fn token(&self) -> String {
        fs::read_to_string(self.home.join("hub/token"))
            .expect("the token file exists")
            .trim()
            .to_owned()
    }

    fn post(&self, path: &str, body: &Value) -> Response {
        self.request("POST", path, body.to_string().as_bytes())
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[])
    }

    fn request(&self, method: &str, path: &str, body: &[u8]) -> Response {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("the hub accepts a connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("a read timeout is settable");
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.port,
            self.token(),
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        stream.write_all(&request).expect("the request is written");
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .expect("the response reads to its end");
        parse_response(&raw)
    }

    /// The repository's sessions as the route lists them.
    fn sessions(&self, repo: &str) -> Vec<Value> {
        self.get(&format!("/v1/repos/{repo}/sessions"))
            .json()
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // SIGTERM, not SIGKILL: the hub must stop its held sessions cleanly, exactly as on a
        // real stop. Then the sockets that are left name processes to kill, so a failed
        // shutdown cannot leak a child into the next test.
        let _ = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        for socket in sockets(&self.home.join("run")) {
            if let Some(pid) = socket
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u32>().ok())
            {
                let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
            let _ = fs::remove_file(socket);
        }
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

// ---- the fixture -----------------------------------------------------------------------

/// A repository, a short home with the mock model's route, the mock and a running hub.
struct Fixture {
    /// Dropped first: its `Drop` stops the hub and every held session before the home and the
    /// repository clone go away.
    hub: Hub,
    home: ShortHome,
    repo: Repo,
    _mock: Mock,
    name: &'static str,
}

impl Fixture {
    fn new(
        label: &'static str,
        replies: Vec<Reply>,
        extra_config: &str,
        idle: Option<u64>,
    ) -> Self {
        let mock = Mock::start(replies);
        let repo = Repo::new(label);
        // A registered repository must be an arc repository; the scheduling loop schedules it
        // (with nothing to do) and the session routes only need its path.
        let arc = repo.arc();
        fs::create_dir_all(&arc).expect("the arc directory is creatable");
        fs::write(
            arc.join("config.toml"),
            radian_arc::store::config::default_text("main"),
        )
        .expect("the arc configuration writes");
        let home = ShortHome::new();
        home.write_config(mock.base_url(), extra_config);
        // The registered path is the canonical clone: arc and the hub compare workspaces after
        // canonicalizing both sides (macOS's /var is /private/var).
        Registry::at(home.path().join("hub"))
            .add(&repo.canonical_clone(), Some(label))
            .expect("the repository registers");
        let idle = idle.map(|secs| secs.to_string());
        let envs: Vec<(&str, &str)> = idle
            .as_deref()
            .map(|secs| vec![("RADIAN_HUB_SESSION_IDLE_SECS", secs)])
            .unwrap_or_default();
        let hub = Hub::start(&repo, &home, &envs);
        Self {
            hub,
            home,
            repo,
            _mock: mock,
            name: label,
        }
    }

    fn create(&self, body: Value) -> String {
        let response = self
            .hub
            .post(&format!("/v1/repos/{}/sessions", self.name), &body);
        assert_eq!(
            response.status,
            200,
            "{:?}\n--- hub stderr ---\n{}",
            String::from_utf8_lossy(&response.body),
            fs::read_to_string(self.home.path().join("hub.stderr.log")).unwrap_or_default()
        );
        response
            .json()
            .get("session")
            .and_then(Value::as_str)
            .expect("the create route returns a session id")
            .to_owned()
    }

    fn prompt(&self, id: &str, body: Value) -> Response {
        self.hub.post(
            &format!("/v1/repos/{}/sessions/{id}/prompt", self.name),
            &body,
        )
    }

    fn journal(&self, id: &str) -> PathBuf {
        self.home.journal(id)
    }

    /// Waits until the journal has a committed line the predicate accepts.
    fn wait_journal(&self, id: &str, what: &str, mut found: impl FnMut(&Value) -> bool) {
        let path = self.journal(id);
        wait_until(what, || {
            let Ok(text) = fs::read_to_string(&path) else {
                return false;
            };
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| found(&event))
        });
    }

    /// Waits until the run directory has no control socket, i.e. the process exited.
    fn wait_no_socket(&self) {
        wait_until("the session process exits", || {
            sockets(&self.home.run()).is_empty()
        });
    }

    /// Stops every hosted session cleanly (SIGTERM, which the process handles) and waits for
    /// the sockets to go, so a session is ended without waiting out its idle window.
    fn stop_sessions(&self) {
        for socket in sockets(&self.home.run()) {
            if let Some(pid) = socket
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u32>().ok())
            {
                let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }
        }
        self.wait_no_socket();
    }
}

fn sockets(run: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(run) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sock"))
        .collect()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- the mock model's replies ----------------------------------------------------------

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: &Value) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

// ---- the control port, as a client -----------------------------------------------------

/// One request on a fresh connection to a session's control port, with the owner token.
fn control(socket: &Path, token: &str, mut request: Value) -> Value {
    if let Some(object) = request.as_object_mut() {
        object.insert("token".to_owned(), json!(token));
    }
    let mut stream = UnixStream::connect(socket).expect("the control socket connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("a read timeout is settable");
    let mut line = request.to_string().into_bytes();
    line.push(b'\n');
    stream.write_all(&line).expect("the request is written");
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .expect("the answer line reads");
    serde_json::from_str(answer.trim()).expect("the answer is JSON")
}

/// The queue head of a live session, or `None` while nothing waits.
fn pending_head(socket: &Path, token: &str) -> Option<Value> {
    let answer = control(socket, token, json!({"id": 1, "verb": "pending"}));
    assert_eq!(answer.get("ok"), Some(&Value::Bool(true)), "{answer}");
    answer.get("head").cloned().filter(|head| !head.is_null())
}

/// Waits for a live session's queue to hold a request, returning its `request_id`.
fn wait_pending(socket: &Path, token: &str) -> u64 {
    let mut id = 0;
    wait_until("the session queues an approval", || {
        match pending_head(socket, token) {
            Some(head) => {
                id = head
                    .get("request_id")
                    .and_then(Value::as_u64)
                    .expect("a pending head carries its request id");
                true
            }
            None => false,
        }
    });
    id
}

/// Waits until the run directory has one socket and returns it.
fn wait_socket(run: &Path) -> PathBuf {
    let mut found = None;
    wait_until("a control socket appears", || {
        let list = sockets(run);
        found = list.first().cloned();
        !list.is_empty()
    });
    found.expect("the socket exists")
}

/// The journal's events, parsed.
fn journal_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

// ---- edge 1 ----------------------------------------------------------------------------

/// `POST sessions` starts a hub-held process, the first message is in its journal with source
/// `User` and `via: hub`, and `sessions` reports `live: hub`.
#[test]
fn a_created_session_sends_its_first_message_as_the_owner_with_the_hub_origin() {
    let fixture = Fixture::new("create", vec![text_reply("hello back")], "", Some(60));
    let id = fixture.create(json!({"text": "hello from the phone"}));

    let journal = fixture.journal(&id);
    wait_until("the journal exists", || journal.is_file());
    fixture.wait_journal(&id, "the first message is enqueued", |event| {
        event.get("event").and_then(Value::as_str) == Some("InboxEnqueued")
            && event.pointer("/data/source") == Some(&json!("user"))
            && event.pointer("/data/message/user/meta/origin/via") == Some(&json!("hub"))
            && event.pointer("/data/message/user/content/0/text/text")
                == Some(&json!("hello from the phone"))
    });

    // The scanner's two-second pass turns the port's `holder: hub` into `live: hub`.
    wait_until(
        "the sessions route reports the session live and hub-held",
        || {
            fixture.hub.sessions(fixture.name).iter().any(|row| {
                row.get("id").and_then(Value::as_str) == Some(id.as_str())
                    && row.get("live").and_then(Value::as_str) == Some("hub")
            })
        },
    );

    let transcript = fixture
        .hub
        .get(&format!(
            "/v1/repos/{}/sessions/{id}/transcript",
            fixture.name
        ))
        .json();
    let entries = transcript
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        entries.iter().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("user")
                && entry
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("hello from the phone"))
        }),
        "the transcript carries the first message: {transcript}"
    );
}

// ---- edge 2 ----------------------------------------------------------------------------

/// A `prompt` to an ended session resumes the same journal: same id, history kept, and the
/// second message appended to the file the first process wrote.
#[test]
fn a_prompt_to_an_ended_session_resumes_the_same_journal() {
    let fixture = Fixture::new(
        "resume",
        vec![text_reply("first answer"), text_reply("second answer")],
        "",
        Some(1),
    );
    let id = fixture.create(json!({"text": "first question"}));
    fixture.wait_journal(&id, "the first turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });
    fixture.wait_no_socket();

    let before = journal_events(&fixture.journal(&id));
    assert_eq!(
        before
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some("SessionCreated"))
            .count(),
        1,
        "one session was created"
    );

    let response = fixture.prompt(&id, json!({"text": "second question"}));
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(
        response.json().get("session").and_then(Value::as_str),
        Some(id.as_str()),
        "the prompt answers the same session id"
    );
    fixture.wait_journal(
        &id,
        "the resumed turn enqueues the second message",
        |event| {
            event.pointer("/data/message/user/content/0/text/text")
                == Some(&json!("second question"))
        },
    );
    fixture.wait_journal(&id, "the resumed turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });

    let after = journal_events(&fixture.journal(&id));
    assert!(
        after.len() > before.len(),
        "the same journal grew: {} then {}",
        before.len(),
        after.len()
    );
    assert_eq!(
        after
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some("SessionCreated"))
            .count(),
        1,
        "a resume never creates a second session"
    );
    assert!(
        after.iter().any(
            |event| event.pointer("/data/message/user/content/0/text/text")
                == Some(&json!("first question"))
        ),
        "the history of the first process is still there"
    );
}

// ---- edge 3 ----------------------------------------------------------------------------

/// A `prompt` to a live session started outside the hub goes through its socket; the hub
/// starts no second process.
#[test]
fn a_prompt_to_a_live_process_outside_the_hub_uses_its_socket() {
    let fixture = Fixture::new("outside", vec![text_reply("answered")], "", Some(60));
    let id = "outside-one";
    // A process the hub did not start: the hub can only reach it through its socket.
    let stderr = fs::File::create(fixture.home.path().join("outside.log"))
        .expect("the log file is creatable");
    let mut child = KillOnDrop::new(
        Command::new(radian_bin())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", fixture.home.path())
            .env("RADIAN_HOME", fixture.home.path())
            .env("RADIAN_SESSION_DIR", fixture.home.sessions())
            .env("RADIAN_CATALOG_URL", fast_catalog_url())
            .current_dir(&fixture.repo.clone)
            .args([
                "exec",
                "--hub-held",
                "--hub-idle-secs",
                "60",
                "--workspace",
                fixture
                    .repo
                    .canonical_clone()
                    .to_str()
                    .expect("a UTF-8 path"),
                "--session-id",
                id,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("the outside session starts"),
    );
    let socket = fixture.home.run().join(format!("{}.sock", child.pid()));
    wait_until("the outside session's socket answers", || {
        fixture.journal(id).is_file() && UnixStream::connect(&socket).is_ok()
    });

    let response = fixture.prompt(id, json!({"text": "from the hub"}));
    assert_eq!(response.status, 200, "{:?}", response.body);
    fixture.wait_journal(id, "the message reaches the outside process", |event| {
        event.pointer("/data/message/user/content/0/text/text") == Some(&json!("from the hub"))
    });
    assert_eq!(
        sockets(&fixture.home.run()).len(),
        1,
        "the hub did not start a second process"
    );

    let _ = unsafe { libc::kill(child.pid() as libc::pid_t, libc::SIGTERM) };
    child.wait();
}

/// A child process killed on drop, so a panicking test cannot leak a session process into the
/// next test's home.
struct KillOnDrop(Child);

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn wait(&mut self) {
        let _ = self.0.wait();
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---- edge 4 ----------------------------------------------------------------------------

/// Two concurrent prompts to the same ended session start one process only.
#[test]
fn two_concurrent_prompts_to_an_ended_session_start_one_process() {
    let fixture = Fixture::new(
        "concurrent",
        vec![
            text_reply("created"),
            text_reply("first prompt"),
            text_reply("second prompt"),
        ],
        "",
        Some(60),
    );
    let id = fixture.create(json!({"text": "create it"}));
    fixture.wait_journal(&id, "the creating turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });
    // End the session now rather than waiting out its 60-second idle window: the prompts below
    // must find it ended, and the process they start must stay alive past them.
    fixture.stop_sessions();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for text in ["from thread one", "from thread two"] {
        let barrier = Arc::clone(&barrier);
        let endpoint = format!("/v1/repos/{}/sessions/{id}/prompt", fixture.name);
        let port = fixture.hub.port;
        let token = fixture.hub.token();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            raw_request(port, &token, &endpoint, &json!({"text": text}).to_string())
        }));
    }
    for handle in handles {
        let response = handle.join().expect("the prompt thread returns");
        assert_eq!(response.status, 200, "{:?}", response.body);
    }

    wait_until("both prompts are in the one journal", || {
        let events = journal_events(&fixture.journal(&id));
        ["from thread one", "from thread two"].iter().all(|text| {
            events.iter().any(|event| {
                event.pointer("/data/message/user/content/0/text/text") == Some(&json!(text))
            })
        })
    });
    assert_eq!(
        sockets(&fixture.home.run()).len(),
        1,
        "only one process hosts the journal"
    );
    assert_eq!(
        journal_events(&fixture.journal(&id))
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some("SessionCreated"))
            .count(),
        1,
        "no second session was created"
    );
}

/// A POST from a bare thread (the concurrent test's client).
fn raw_request(port: u16, token: &str, path: &str, body: &str) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the hub accepts a connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("a read timeout is settable");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .expect("the response reads to its end");
    parse_response(&raw)
}

// ---- edges 5 and 6 ---------------------------------------------------------------------

/// An approval in a hub-held process waits in the queue (not on stdin) and proceeds after
/// `answer`.
#[test]
fn an_approval_waits_in_the_queue_and_proceeds_after_the_hub_answers() {
    let fixture = Fixture::new(
        "approval",
        vec![
            tool_reply("c1", "read", &json!({"path": "README.md"})),
            text_reply("read it"),
        ],
        "[permissions]\napproval = \"always\"\n",
        Some(60),
    );
    let id = fixture.create(json!({"text": "read the readme"}));
    let socket = wait_socket(&fixture.home.run());
    let request_id = wait_pending(&socket, &fixture.hub.token());

    // The approval is in the queue and the session reports blocked; nothing has run yet.
    wait_until("the sessions route reports the blocked session", || {
        fixture.hub.sessions(fixture.name).iter().any(|row| {
            row.get("id").and_then(Value::as_str) == Some(id.as_str())
                && row.get("pending").and_then(Value::as_u64) == Some(1)
        })
    });
    assert!(
        !journal_events(&fixture.journal(&id)).iter().any(|event| {
            event.get("event").and_then(Value::as_str) == Some("ToolExecutionSettled")
        }),
        "the tool has not run while the approval waits"
    );

    let response = fixture.hub.post(
        &format!("/v1/repos/{}/sessions/{id}/answer", fixture.name),
        &json!({"request_id": request_id, "response": "allow_once"}),
    );
    assert_eq!(response.status, 200, "{:?}", response.body);

    fixture.wait_journal(&id, "the approved tool runs", |event| {
        event.get("event").and_then(Value::as_str) == Some("ToolExecutionSettled")
    });
    fixture.wait_journal(&id, "the turn finishes after the tool", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });
    let reply = fixture
        .hub
        .get(&format!(
            "/v1/repos/{}/sessions/{id}/transcript",
            fixture.name
        ))
        .json();
    assert!(
        serde_json::to_string(&reply)
            .expect("the transcript is JSON")
            .contains("README"),
        "the approved read produced its result: {reply}"
    );
}

/// A late `answer` is `409` and changes nothing: the once-answered approval stays answered and
/// the tool result stays the one the first answer produced.
#[test]
fn a_late_answer_is_conflict_and_nothing_changes() {
    let fixture = Fixture::new(
        "late-answer",
        vec![
            tool_reply("c1", "read", &json!({"path": "README.md"})),
            text_reply("read it"),
        ],
        "[permissions]\napproval = \"always\"\n",
        Some(60),
    );
    let id = fixture.create(json!({"text": "read the readme"}));
    let socket = wait_socket(&fixture.home.run());
    let token = fixture.hub.token();
    let request_id = wait_pending(&socket, &token);

    let first = fixture.hub.post(
        &format!("/v1/repos/{}/sessions/{id}/answer", fixture.name),
        &json!({"request_id": request_id, "response": "allow_once"}),
    );
    assert_eq!(first.status, 200, "{:?}", first.body);
    fixture.wait_journal(&id, "the approved tool finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("ToolExecutionSettled")
    });
    let before = journal_events(&fixture.journal(&id));

    let second = fixture.hub.post(
        &format!("/v1/repos/{}/sessions/{id}/answer", fixture.name),
        &json!({"request_id": request_id, "response": "reject_once"}),
    );
    assert_eq!(second.status, 409, "{:?}", second.body);
    let after = journal_events(&fixture.journal(&id));
    assert_eq!(
        before
            .iter()
            .filter(
                |event| event.get("event").and_then(Value::as_str) == Some("ToolExecutionSettled")
            )
            .count(),
        after
            .iter()
            .filter(
                |event| event.get("event").and_then(Value::as_str) == Some("ToolExecutionSettled")
            )
            .count(),
        "a late answer appends nothing"
    );
    let result = after
        .iter()
        .find(|event| event.get("event").and_then(Value::as_str) == Some("ToolExecutionSettled"))
        .expect("the approved call finished");
    assert_ne!(
        result.pointer("/data/result/is_error"),
        Some(&json!(true)),
        "the first answer's allow stands: {result}"
    );
}

// ---- edge 7 ----------------------------------------------------------------------------

/// An idle hub-held process exits, the journal stays complete, and the next prompt resumes it.
#[test]
fn an_idle_process_exits_and_the_next_prompt_resumes_it() {
    let fixture = Fixture::new(
        "idle",
        vec![text_reply("done"), text_reply("back again")],
        "",
        Some(1),
    );
    let id = fixture.create(json!({"text": "hello"}));
    fixture.wait_journal(&id, "the turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });
    fixture.wait_no_socket();

    // The journal is complete: every line parses and the turn's finish is recorded, so the
    // shutdown appended nothing torn and lost nothing.
    let events = journal_events(&fixture.journal(&id));
    assert!(
        events
            .iter()
            .any(|event| event.get("event").and_then(Value::as_str) == Some("TurnFinished")),
        "the turn is recorded"
    );

    let response = fixture.prompt(&id, json!({"text": "again"}));
    assert_eq!(response.status, 200, "{:?}", response.body);
    fixture.wait_journal(&id, "the resumed turn takes the message", |event| {
        event.pointer("/data/message/user/content/0/text/text") == Some(&json!("again"))
    });
    assert_eq!(
        journal_events(&fixture.journal(&id))
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some("SessionCreated"))
            .count(),
        1,
        "the resume reuses the journal"
    );
}

// ---- edge 8 ----------------------------------------------------------------------------

/// A process with a running background job is not reclaimed, however long it sits without a
/// turn.
#[test]
fn a_process_with_a_running_background_job_is_not_reclaimed() {
    let fixture = Fixture::new(
        "background",
        vec![
            tool_reply(
                "c1",
                "shell",
                &json!({"command": "sleep 30", "background": true}),
            ),
            text_reply("the job is running"),
        ],
        "",
        Some(1),
    );
    let id = fixture.create(json!({"text": "start a long job"}));
    fixture.wait_journal(&id, "the turn finishes with the job running", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });

    // Well past the one-second idle window: the job keeps the process alive.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        sockets(&fixture.home.run()).len(),
        1,
        "the running job keeps the process from reclaiming itself"
    );
    wait_until("the port reports the job as working", || {
        fixture.hub.sessions(fixture.name).iter().any(|row| {
            row.get("id").and_then(Value::as_str) == Some(id.as_str())
                && row.get("live").and_then(Value::as_str) == Some("hub")
                && row.get("state").and_then(Value::as_str) == Some("working")
        })
    });
}

// ---- edge 10 ---------------------------------------------------------------------------

/// A body without `text` is `400` and starts nothing.
#[test]
fn a_bad_body_is_bad_request_and_starts_no_process() {
    let fixture = Fixture::new(
        "bad-body",
        vec![text_reply("first"), text_reply("second")],
        "",
        Some(60),
    );

    let response = fixture
        .hub
        .post(&format!("/v1/repos/{}/sessions", fixture.name), &json!({}));
    assert_eq!(response.status, 400, "{:?}", response.body);
    assert!(
        response.error().contains("text"),
        "the error names the missing field: {response:?}",
        response = response.body
    );
    assert!(
        sockets(&fixture.home.run()).is_empty(),
        "no process was started"
    );
    let sessions = fixture.hub.sessions(fixture.name);
    assert!(sessions.is_empty(), "no session exists: {sessions:?}");

    // The same for a prompt: a bad body reaches no process, and an ended session stays ended.
    let id = fixture.create(json!({"text": "the one session"}));
    fixture.wait_journal(&id, "the turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });
    fixture.stop_sessions();
    let response = fixture.prompt(&id, json!({"steer": true}));
    assert_eq!(response.status, 400, "{:?}", response.body);
    assert!(
        sockets(&fixture.home.run()).is_empty(),
        "the bad prompt started no process"
    );
}

// ---- the request bodies a process refuses ----------------------------------------------

/// A bad attachment and a bad model object are refused before anything starts.
#[test]
fn a_bad_attachment_or_model_is_unprocessable_and_starts_no_process() {
    let fixture = Fixture::new("bad-values", vec![text_reply("unused")], "", Some(60));

    let bad_file = fixture.hub.post(
        &format!("/v1/repos/{}/sessions", fixture.name),
        &json!({"text": "x", "files": [{"name": "a.txt", "text": "y".repeat(200 * 1024)}]}),
    );
    assert_eq!(bad_file.status, 422, "{:?}", bad_file.body);
    assert!(bad_file.error().contains("a.txt"), "{:?}", bad_file.body);

    let bad_model = fixture.hub.post(
        &format!("/v1/repos/{}/sessions", fixture.name),
        &json!({"text": "x", "model": {"provider": "no-such-provider", "model": "m", "extra": 1}}),
    );
    assert_eq!(bad_model.status, 400, "{:?}", bad_model.body);
    assert!(sockets(&fixture.home.run()).is_empty(), "nothing started");
}

// ---- the routes on an ended session ----------------------------------------------------

/// `cancel`, `answer` and `config` on a session no process hosts answer `409` and change
/// nothing.
#[test]
fn the_live_only_routes_refuse_an_ended_session() {
    let fixture = Fixture::new("ended-routes", vec![text_reply("done")], "", Some(1));
    let id = fixture.create(json!({"text": "hello"}));
    fixture.wait_journal(&id, "the turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });
    fixture.wait_no_socket();
    let before = fs::read(fixture.journal(&id)).expect("the journal reads");

    for (path, body) in [
        ("cancel", json!({})),
        ("answer", json!({"request_id": 1, "response": "allow_once"})),
        ("config", json!({"config": {"thinking": "low"}})),
    ] {
        let response = fixture.hub.post(
            &format!("/v1/repos/{}/sessions/{id}/{path}", fixture.name),
            &body,
        );
        assert_eq!(response.status, 409, "{path}: {:?}", response.body);
    }
    assert_eq!(
        fs::read(fixture.journal(&id)).expect("the journal reads"),
        before,
        "a refused route changes no journal byte"
    );
}

// ---- the configuration route -----------------------------------------------------------

/// `config` on a live session queues the turn-boundary change; the next prompt's turn journals
/// it (ADR 0004).
#[test]
fn a_config_route_queues_the_next_turns_route() {
    let fixture = Fixture::new(
        "config-route",
        vec![text_reply("first"), text_reply("second")],
        "",
        Some(60),
    );
    let id = fixture.create(json!({"text": "first"}));
    fixture.wait_journal(&id, "the first turn finishes", |event| {
        event.get("event").and_then(Value::as_str) == Some("TurnFinished")
    });

    let response = fixture.hub.post(
        &format!("/v1/repos/{}/sessions/{id}/config", fixture.name),
        &json!({"config": {"thinking": "low"}}),
    );
    assert_eq!(response.status, 200, "{:?}", response.body);

    let response = fixture.prompt(&id, json!({"text": "second"}));
    assert_eq!(response.status, 200, "{:?}", response.body);
    fixture.wait_journal(
        &id,
        "the queued change is applied at the turn boundary",
        |event| event.get("event").and_then(Value::as_str) == Some("ConfigChanged"),
    );

    let bad = fixture.hub.post(
        &format!("/v1/repos/{}/sessions/{id}/config", fixture.name),
        &json!({"config": {"nonsense": true}}),
    );
    assert_eq!(bad.status, 422, "{:?}", bad.body);
}

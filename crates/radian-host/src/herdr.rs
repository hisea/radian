//! Agent state reporting to herdr (ADR 0034): the environment check, the pure state function,
//! the report JSON, the monotonic `seq`, the resume-argv rules, and the latest-only sender.
//!
//! The socket protocol (herdr 0.9.3, protocol 22; `docs/socket-api.mdx` v0.9.5): newline-
//! delimited JSON over a Unix socket, one request per line, and the response echoes the
//! request's `id`. We use three methods - `pane.report_agent`, `pane.report_agent_session`
//! (unused for now: the identity rides on the first state report) and `pane.release_agent` -
//! and never parse the response beyond "a line arrived".

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::projection::TurnState;

/// The integration id. `source` must not start with `herdr:` (that is reserved for herdr's own
/// integrations); `agent` is the name the user sees in herdr.
pub const SOURCE: &str = "radian";
pub const AGENT: &str = "radian";

/// The first connect/write/read timeout, and the one retry's (ADR H4).
const TIMEOUT: Duration = Duration::from_millis(500);
const RETRY: Duration = Duration::from_millis(1500);

/// The `resume_argv` limits (ADR H3 / herdr's `invalid_resume_argv` rule).
const MAX_ARGV: usize = 64;
const MAX_ARGV_BYTES: usize = 8 * 1024;

/// Where to report, when the environment says we are inside a herdr pane (ADR H1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrEnv {
    pub pane_id: String,
    pub socket: PathBuf,
}

/// The environment check (ADR H1): `HERDR_ENV=1`, a non-empty pane id and socket path, and not
/// turned off by the config or `RADIAN_HERDR=0`. `get` is the environment (testable).
pub fn env_from(
    config_on: bool,
    radian_herdr: Option<&str>,
    get: &dyn Fn(&str) -> Option<String>,
) -> Option<HerdrEnv> {
    if !config_on || radian_herdr == Some("0") {
        return None;
    }
    if get("HERDR_ENV").as_deref() != Some("1") {
        return None;
    }
    let pane_id = get("HERDR_PANE_ID").filter(|value| !value.is_empty())?;
    let socket = get("HERDR_SOCKET_PATH").filter(|value| !value.is_empty())?;
    Some(HerdrEnv {
        pane_id,
        socket: PathBuf::from(socket),
    })
}

/// [`env_from`] against the process environment.
pub fn detect(config_on: bool) -> Option<HerdrEnv> {
    env_from(
        config_on,
        std::env::var("RADIAN_HERDR").ok().as_deref(),
        &|name| std::env::var(name).ok(),
    )
}

/// The three states radian reports (herdr also has `unknown`, which radian never sends: it
/// always knows which of the three it is in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// What is blocking, in the shape the message needs (ADR H2: the category only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// `approval: <tool>`; the tool name, never the command.
    Approval(String),
    Question,
}

/// The pure state function (ADR H2): `blocked` wins over `working`.
pub fn state(turn: TurnState, pending: usize, trust_modal: bool) -> State {
    if trust_modal || pending > 0 {
        State::Blocked
    } else if turn == TurnState::Running {
        State::Working
    } else {
        State::Idle
    }
}

/// The `blocked` message: the category only, never a command, path or question body (ADR H2).
pub fn blocked_message(blocked: Option<&Blocked>, trust_modal: bool) -> String {
    if trust_modal {
        return "trust".to_owned();
    }
    match blocked {
        Some(Blocked::Approval(tool)) => format!("approval: {tool}"),
        Some(Blocked::Question) => "question".to_owned(),
        None => "blocked".to_owned(),
    }
}

/// The session identity the first report carries (ADR H3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    pub id: String,
    pub path: PathBuf,
}

/// One report to herdr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub state: State,
    pub message: Option<String>,
    pub session: Option<SessionIdentity>,
    pub resume_argv: Option<Vec<String>>,
}

impl Report {
    pub fn new(state: State) -> Self {
        Self {
            state,
            message: None,
            session: None,
            resume_argv: None,
        }
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn session(mut self, id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.session = Some(SessionIdentity {
            id: id.into(),
            path: path.into(),
        });
        self
    }

    pub fn resume(mut self, argv: Option<Vec<String>>) -> Self {
        self.resume_argv = argv;
        self
    }
}

/// The `pane.report_agent` request line (newline-terminated).
pub fn report_line(env: &HerdrEnv, report: &Report, seq: u64, id: &str) -> String {
    let mut params = serde_json::json!({
        "pane_id": env.pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "state": report.state.as_str(),
        "seq": seq,
    });
    let object = params.as_object_mut().expect("an object");
    if let Some(message) = &report.message {
        object.insert("message".into(), serde_json::Value::String(message.clone()));
    }
    if let Some(session) = &report.session {
        object.insert(
            "agent_session_id".into(),
            serde_json::Value::String(session.id.clone()),
        );
        object.insert(
            "agent_session_path".into(),
            serde_json::Value::String(session.path.to_string_lossy().into_owned()),
        );
    }
    if let Some(argv) = &report.resume_argv {
        object.insert(
            "resume_argv".into(),
            serde_json::Value::Array(
                argv.iter()
                    .map(|arg| serde_json::Value::String(arg.clone()))
                    .collect(),
            ),
        );
    }
    line(&serde_json::json!({ "id": id, "method": "pane.report_agent", "params": params }))
}

/// The `pane.release_agent` request line (newline-terminated).
pub fn release_line(env: &HerdrEnv, seq: u64, id: &str) -> String {
    line(&serde_json::json!({
        "id": id,
        "method": "pane.release_agent",
        "params": {
            "pane_id": env.pane_id,
            "source": SOURCE,
            "agent": AGENT,
            "seq": seq,
        },
    }))
}

fn line(value: &serde_json::Value) -> String {
    let mut text = value.to_string();
    text.push('\n');
    text
}

/// The resume command (ADR H3): `["radian", "--resume", <id>, ...]`. The first word is the bare
/// `radian` on `PATH`, never `argv[0]` (a user may have started `./target/debug/radian`).
pub fn resume_argv(id: &str, extra: &[String]) -> Result<Vec<String>, String> {
    let mut argv = vec!["radian".to_owned(), "--resume".to_owned(), id.to_owned()];
    argv.extend(extra.iter().cloned());
    validate_resume_argv(&argv)?;
    Ok(argv)
}

/// herdr's `resume_argv` rules (ADR H3): a plain first word, no apostrophe or control
/// character in any element, at most 64 elements and 8 KiB in total. `Err` is the reason, so
/// the startup line can name it.
pub fn validate_resume_argv(argv: &[String]) -> Result<(), String> {
    let Some(first) = argv.first() else {
        return Err("empty command".to_owned());
    };
    if first.is_empty() || first.contains('/') || first.contains('\\') {
        return Err("the first word must be a plain command name".to_owned());
    }
    if argv.len() > MAX_ARGV {
        return Err(format!("more than {MAX_ARGV} arguments"));
    }
    let bytes: usize = argv.iter().map(String::len).sum();
    if bytes > MAX_ARGV_BYTES {
        return Err(format!("more than {MAX_ARGV_BYTES} bytes"));
    }
    for arg in argv {
        if arg.contains('\'') {
            return Err("an argument contains an apostrophe".to_owned());
        }
        if arg.chars().any(char::is_control) {
            return Err("an argument contains a control character".to_owned());
        }
    }
    Ok(())
}

/// The monotonic `seq` (ADR H3): `max(now_ns, last + 1)`.
#[derive(Debug, Default)]
pub struct Seq(AtomicU64);

impl Seq {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        let last = self.0.load(Ordering::SeqCst);
        let next = now.max(last.saturating_add(1));
        self.0.store(next, Ordering::SeqCst);
        next
    }
}

/// One queued request.
struct Request {
    line: String,
    /// A release is sent once with the short timeout; a report retries once.
    retry: bool,
}

struct Shared {
    slot: Mutex<Option<Request>>,
    wake: Condvar,
    stop: AtomicBool,
    counter: AtomicU64,
    /// The sender's connect/write/read timeouts: the production values, or the test seam's.
    timeout: Duration,
    retry: Duration,
}

/// The latest-only sender (ADR H4): one dedicated OS thread, a slot that keeps only the newest
/// request, and a synchronous release. The UI thread only ever locks the slot.
pub struct Reporter {
    shared: Arc<Shared>,
    env: HerdrEnv,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Reporter {
    /// Starts the sender thread.
    pub fn spawn(env: HerdrEnv) -> Self {
        Self::with_timeouts(env, TIMEOUT, RETRY)
    }

    /// The crate-private test seam: the same sender with longer timeouts, so a loaded machine
    /// cannot turn a reply the test is deliberately holding into a retry. Production always
    /// uses [`TIMEOUT`]/[`RETRY`].
    pub(crate) fn with_timeouts(env: HerdrEnv, timeout: Duration, retry: Duration) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(None),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            counter: AtomicU64::new(0),
            timeout,
            retry,
        });
        let thread_env = env.clone();
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("radian-herdr".to_owned())
            .spawn(move || sender_loop(thread_env, thread_shared))
            .ok();
        Self {
            shared,
            env,
            thread: Mutex::new(handle),
        }
    }

    fn id(&self) -> String {
        format!(
            "radian-{}",
            self.shared.counter.fetch_add(1, Ordering::SeqCst) + 1
        )
    }

    /// Queues a report, overwriting any report not yet sent (ADR H4). Never blocks on I/O.
    pub fn report(&self, report: &Report, seq: u64) {
        let request = Request {
            line: report_line(&self.env, report, seq, &self.id()),
            retry: true,
        };
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(request);
        self.shared.wake.notify_one();
    }

    /// Sends a release synchronously with the short timeout (ADR H5). Failures are ignored.
    pub fn release(&self, seq: u64) {
        let request = Request {
            line: release_line(&self.env, seq, &self.id()),
            retry: false,
        };
        send(&self.env, &request, TIMEOUT, RETRY);
    }

    /// Stops the sender thread (best effort; a blocked write times out on its own).
    ///
    /// `stop` is set while holding the slot mutex. Setting it without the lock loses a race
    /// with the sender: the sender can check `stop` (false), then call `wake.wait`, and the
    /// `notify_all` lands before it is registered as a waiter. It then waits forever and the
    /// `join` below hangs (the herdr-report hang).
    pub fn shutdown(&self) {
        self.stop_and_wake();
        if let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }

    /// Signals the sender under the slot mutex, so a sender between its `stop` check and its
    /// `wait` cannot miss the wakeup.
    fn stop_and_wake(&self) {
        let _slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
    }
}

impl Drop for Reporter {
    /// Signals the sender thread and returns without joining: a panic must not wait on an
    /// in-flight send (ADR 0034 H5, the panic path does no I/O). The thread exits on its own
    /// after its current timeout. The lock taken here is the slot mutex, which the sender never
    /// holds during a send, so this still does not wait on I/O.
    fn drop(&mut self) {
        self.stop_and_wake();
    }
}

fn sender_loop(env: HerdrEnv, shared: Arc<Shared>) {
    loop {
        let request = {
            let mut slot = shared.slot.lock().unwrap_or_else(|e| e.into_inner());
            while slot.is_none() && !shared.stop.load(Ordering::SeqCst) {
                slot = shared.wake.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
            slot.take()
        };
        match request {
            Some(request) => send(&env, &request, shared.timeout, shared.retry),
            None if shared.stop.load(Ordering::SeqCst) => return,
            None => {}
        }
    }
}

/// One request over the socket: connect, write the line, wait for a response line. A report
/// retries once with the longer timeout; the response is only read to confirm delivery.
fn send(env: &HerdrEnv, request: &Request, timeout: Duration, retry: Duration) {
    if send_once(env, request, timeout).is_ok() {
        return;
    }
    // One retry with the longer timeout; a release is sent once.
    if request.retry {
        let _ = send_once(env, request, retry);
    }
}

fn send_once(env: &HerdrEnv, request: &Request, timeout: Duration) -> std::io::Result<()> {
    let stream = UnixStream::connect(&env.socket)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    let mut writer = stream.try_clone()?;
    writer.write_all(request.line.as_bytes())?;
    writer.flush()?;
    // Read one response line; its content is not used.
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HerdrEnv {
        HerdrEnv {
            pane_id: "w1:p1".into(),
            socket: PathBuf::from("/tmp/fake.sock"),
        }
    }

    #[test]
    fn the_environment_check_needs_all_three_and_the_switches() {
        let get = |name: &str| match name {
            "HERDR_ENV" => Some("1".to_owned()),
            "HERDR_PANE_ID" => Some("w1:p1".to_owned()),
            "HERDR_SOCKET_PATH" => Some("/tmp/h.sock".to_owned()),
            _ => None,
        };
        assert_eq!(
            env_from(true, None, &get),
            Some(HerdrEnv {
                pane_id: "w1:p1".into(),
                socket: "/tmp/h.sock".into()
            })
        );
        assert!(env_from(false, None, &get).is_none(), "config off");
        assert!(env_from(true, Some("0"), &get).is_none(), "RADIAN_HERDR=0");
        let no_env = |name: &str| {
            if name == "HERDR_PANE_ID" {
                Some("w1:p1".to_owned())
            } else {
                None
            }
        };
        assert!(env_from(true, None, &no_env).is_none(), "no HERDR_ENV");
    }

    #[test]
    fn blocked_wins_over_working() {
        assert_eq!(state(TurnState::Running, 1, false), State::Blocked);
        assert_eq!(state(TurnState::Running, 0, true), State::Blocked);
        assert_eq!(state(TurnState::Running, 0, false), State::Working);
        assert_eq!(state(TurnState::Idle, 0, false), State::Idle);
        assert_eq!(state(TurnState::Cancelled, 0, false), State::Idle);
        assert_eq!(
            blocked_message(Some(&Blocked::Approval("shell".into())), false),
            "approval: shell"
        );
        assert_eq!(blocked_message(None, true), "trust");
        assert_eq!(blocked_message(Some(&Blocked::Question), false), "question");
    }

    #[test]
    fn the_report_line_has_the_schema_shape() {
        let report = Report::new(State::Blocked)
            .message("approval: shell")
            .session("sid-1", "/tmp/s.jsonl")
            .resume(Some(vec![
                "radian".into(),
                "--resume".into(),
                "sid-1".into(),
            ]));
        let text = report_line(&env(), &report, 42, "req-1");
        assert!(text.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["method"], "pane.report_agent");
        assert_eq!(value["params"]["pane_id"], "w1:p1");
        assert_eq!(value["params"]["source"], "radian");
        assert_eq!(value["params"]["agent"], "radian");
        assert_eq!(value["params"]["state"], "blocked");
        assert_eq!(value["params"]["message"], "approval: shell");
        assert_eq!(value["params"]["seq"], 42);
        assert_eq!(value["params"]["agent_session_id"], "sid-1");
        assert_eq!(value["params"]["agent_session_path"], "/tmp/s.jsonl");
        assert_eq!(value["params"]["resume_argv"][0], "radian");
    }

    #[test]
    fn the_release_line_has_the_schema_shape() {
        let text = release_line(&env(), 5, "r");
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["method"], "pane.release_agent");
        assert_eq!(value["params"]["pane_id"], "w1:p1");
        assert_eq!(value["params"]["source"], "radian");
        assert_eq!(value["params"]["agent"], "radian");
        assert_eq!(value["params"]["seq"], 5);
    }

    #[test]
    fn the_resume_argv_rules_reject_a_bad_element() {
        assert!(resume_argv("sid", &[]).is_ok());
        // An apostrophe in a --workspace path makes the whole command invalid.
        let bad = resume_argv("sid", &["--workspace".into(), "/tmp/it's".into()]);
        assert!(bad.unwrap_err().contains("apostrophe"));
        assert!(validate_resume_argv(&["radian".into(), "a\nb".into()]).is_err());
        assert!(validate_resume_argv(&["/usr/bin/radian".into()]).is_err());
        assert!(validate_resume_argv(&[]).is_err());
        let long: Vec<String> = (0..70).map(|i| format!("a{i}")).collect();
        assert!(validate_resume_argv(&long).is_err());
    }

    use std::os::unix::net::UnixListener;

    /// A short `/tmp` socket path (the AF_UNIX limit) and its listener.
    fn listener(label: &str) -> (PathBuf, UnixListener) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "radian-herdr-{}-{label}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        (path, listener)
    }

    /// The test-side deadlines. `UnixListener::accept` and a socket read have no timeout, so a
    /// test that waits for a connection or a line the sender never sends must fail here, not
    /// hang. (The herdr-report hang was a lost wakeup in `shutdown`; a future one must not turn
    /// into a stuck test binary.)
    const ACCEPT_DEADLINE: Duration = Duration::from_secs(5);
    const READ_DEADLINE: Duration = Duration::from_secs(5);
    /// The tests hold a reply on purpose; a loaded machine must not turn that into a retry, so
    /// the sender gets these timeouts instead of the production 500 ms / 1500 ms.
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Accepts one connection, or fails after [`ACCEPT_DEADLINE`]. `accept` runs on a helper
    /// thread, so the deadline is a channel receive, not a sleep.
    fn accept(listener: &UnixListener) -> UnixStream {
        let listener = listener.try_clone().expect("clone listener");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(listener.accept());
        });
        match rx.recv_timeout(ACCEPT_DEADLINE) {
            Ok(Ok((conn, _))) => conn,
            Ok(Err(e)) => panic!("accept failed: {e}"),
            Err(_) => panic!("no connection arrived within {ACCEPT_DEADLINE:?}"),
        }
    }

    fn read_line(conn: &mut UnixStream) -> String {
        conn.set_read_timeout(Some(READ_DEADLINE))
            .expect("set read timeout");
        let mut line = String::new();
        match BufReader::new(conn.try_clone().unwrap()).read_line(&mut line) {
            Ok(0) => panic!("the connection closed before a line arrived"),
            Ok(_) => line,
            Err(e) => panic!("no line arrived within {READ_DEADLINE:?}: {e}"),
        }
    }

    fn reply(conn: &mut UnixStream) {
        let _ = conn.write_all(b"{\"id\":\"x\",\"result\":{}}\n");
        let _ = conn.flush();
    }

    #[test]
    fn the_reporter_sends_the_report_line() {
        let (socket, listener) = listener("send");
        let env = HerdrEnv {
            pane_id: "w1:p1".into(),
            socket: socket.clone(),
        };
        let reporter = Reporter::with_timeouts(env, TEST_TIMEOUT, TEST_TIMEOUT);
        reporter.report(&Report::new(State::Working), 7);
        let mut conn = accept(&listener);
        let line = read_line(&mut conn);
        reply(&mut conn);
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["method"], "pane.report_agent");
        assert_eq!(value["params"]["pane_id"], "w1:p1");
        assert_eq!(value["params"]["source"], "radian");
        assert_eq!(value["params"]["agent"], "radian");
        assert_eq!(value["params"]["state"], "working");
        assert_eq!(value["params"]["seq"], 7);
        reporter.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    /// A listener that never replies: the caller returns immediately and the sender gives up
    /// after its timeouts.
    #[test]
    fn a_silent_listener_does_not_block_the_caller() {
        let (socket, listener) = listener("silent");
        let env = HerdrEnv {
            pane_id: "w1:p1".into(),
            socket: socket.clone(),
        };
        let reporter = Reporter::spawn(env);
        let start = std::time::Instant::now();
        reporter.report(&Report::new(State::Working), 1);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "report must not block: {:?}",
            start.elapsed()
        );
        // The connection is accepted but never answered; the sender's read times out.
        let _conn = accept(&listener);
        reporter.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    /// While the sender is busy, a newer report replaces the queued one (ADR H4).
    #[test]
    fn the_slot_keeps_only_the_latest() {
        let (socket, listener) = listener("latest");
        let env = HerdrEnv {
            pane_id: "w1:p1".into(),
            socket: socket.clone(),
        };
        let reporter = Reporter::with_timeouts(env, TEST_TIMEOUT, TEST_TIMEOUT);
        // The first report connects; the listener accepts but holds the reply.
        reporter.report(&Report::new(State::Idle), 1);
        let mut first = accept(&listener);
        let first_line = read_line(&mut first);
        // Queue two more while the first send is waiting for its reply.
        reporter.report(&Report::new(State::Working), 2);
        reporter.report(&Report::new(State::Blocked), 3);
        // Release the first; the sender then takes the slot (the latest, `blocked`).
        reply(&mut first);
        let mut second = accept(&listener);
        let second_line = read_line(&mut second);
        reply(&mut second);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(first_line.trim()).unwrap()["params"]["state"],
            "idle"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(second_line.trim()).unwrap()["params"]["state"],
            "blocked",
            "the working report must have been replaced"
        );
        reporter.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    /// An apostrophe makes the resume command invalid; the report still goes, without it.
    #[test]
    fn an_invalid_resume_omits_the_command_but_still_reports() {
        let argv = resume_argv("sid", &["--workspace".into(), "/tmp/it's".into()]);
        assert!(argv.is_err());
        let report = Report::new(State::Idle).resume(argv.ok());
        let line = report_line(&env(), &report, 1, "r");
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["params"]["state"], "idle");
        assert!(value["params"].get("resume_argv").is_none());
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let seq = Seq::new();
        let mut last = 0;
        for _ in 0..1000 {
            let next = seq.next();
            assert!(next > last, "{next} <= {last}");
            last = next;
        }
    }
}

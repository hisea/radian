//! End-to-end through a pty: the bin is started under `portable-pty`, driven by bytes, and
//! its rendering is read from a `vt100` screen model. **The full-screen program is never run
//! in the test runner's own terminal.**

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_provider_anthropic::testing as anth;
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::json;
use vt100::Parser;

/// A unique temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-tui-pty-{}-{label}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const BIN: &str = env!("CARGO_BIN_EXE_radian-tui");

/// The pty tests spawn real processes and mock servers; run them one at a time so the
/// whole workspace staying busy cannot make one of them flake.
static PTY_LOCK: Mutex<()> = Mutex::new(());

fn pty_guard() -> std::sync::MutexGuard<'static, ()> {
    PTY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Pty {
    writer: Box<dyn Write + Send>,
    bytes: Arc<Mutex<Vec<u8>>>,
    parser: Parser,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    processed: usize,
}

impl Pty {
    /// Spawns the bin under `sh -c '"$@"; echo EXITED:$?; stty -a'` so the same pty reports
    /// the exit code and the tty state after the app exits.
    fn spawn(args: &[&str], rows: u16, cols: u16) -> Self {
        Self::spawn_with_env(args, &[], rows, cols)
    }

    /// Like [`Pty::spawn`], with extra environment for the app (e.g. the sandbox probe seam).
    fn spawn_with_env(args: &[&str], envs: &[(&str, &str)], rows: u16, cols: u16) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(r#""$@"; echo "EXITED:$?"; stty -a"#);
        cmd.arg("sh");
        cmd.arg(BIN);
        for arg in args {
            cmd.arg(arg);
        }
        // Keep the environment small and deterministic; the mock server needs no key.
        cmd.env_clear();
        for name in ["PATH", "HOME", "TMPDIR", "LANG", "TERM"] {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        for (name, value) in envs {
            cmd.env(name, value);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&bytes);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            writer,
            bytes,
            parser: Parser::new(rows, cols, 5000),
            child,
            processed: 0,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn pump(&mut self) {
        let snapshot = self.bytes.lock().unwrap().clone();
        if snapshot.len() > self.processed {
            self.parser.process(&snapshot[self.processed..]);
            self.processed = snapshot.len();
        }
    }

    /// Waits until `needle` shows on the screen. `pump` is a state check, not a sleep used
    /// for synchronization; the deadline only bounds a failure.
    fn wait_for_screen(&mut self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.pump();
            if self.parser.screen().contents().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits without killing: on timeout the caller decides (the `Drop` impl still kills).
    fn wait_for_exit(&mut self, secs: u64) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.pump();
            if let Ok(Some(status)) = self.child.try_wait() {
                // Let the shell's trailing `stty -a` output arrive.
                std::thread::sleep(Duration::from_millis(150));
                self.pump();
                return Some(status.exit_code());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ctrl-C cancels a running turn and quits when idle, so press it until the app exits.
    fn quit(&mut self) {
        for _ in 0..4 {
            self.send(&[0x03]);
            if self.wait_for_exit(3).is_some() {
                return;
            }
        }
        panic!("the app did not exit after repeated Ctrl-C");
    }

    fn raw(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// The app's exit code, parsed from the shell wrapper's `EXITED:<n>` (the shell's own
    /// status is `stty`'s, not the app's).
    fn app_exit_code(&self) -> Option<i32> {
        let text = String::from_utf8_lossy(&self.raw()).into_owned();
        let rest = text.rsplit_once("EXITED:")?.1;
        rest.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }

    fn screen(&mut self) -> String {
        self.pump();
        self.parser.screen().contents()
    }
}

/// One tool call, then (for the next request) the given text.
fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

/// The MCP fixture, built next to the TUI binary. `cargo test -p radian-tui` alone does not
/// necessarily build it, so the failure says what to run.
fn fixture_bin() -> PathBuf {
    let path = Path::new(BIN).parent().unwrap().join("mcp-fixture");
    assert!(
        path.exists(),
        "mcp-fixture is not built; run: cargo build -p agent-extensions --bin mcp-fixture"
    );
    path
}

/// An `--mcp-config` pointing at the fixture, logging every received method to `log`.
fn mcp_config(dir: &Path, log: &Path) -> PathBuf {
    let server = json!({
        "id": "fx",
        "command": fixture_bin(),
        "env": {"FIXTURE_LOG": log},
        "call_timeout_secs": 30,
    });
    let path = dir.join("mcp.json");
    std::fs::write(&path, json!({"servers": [server]}).to_string()).unwrap();
    path
}

/// One turn: send `hello`, wait for the mock reply, then quit with Ctrl-C.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the four pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_end_to_end_conversation_and_clean_exit() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("ASSISTANT-REPLY-OK")]).await;
    let dir = TempDir::new("e2e");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "e2e",
        ],
        24,
        100,
    );

    assert!(
        pty.wait_for_screen("message", 10),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"hello from the test\r");
    assert!(
        pty.wait_for_screen("ASSISTANT-REPLY-OK", 15),
        "reply not rendered:\n{}",
        pty.screen()
    );

    // Ctrl-C cancels if the turn is somehow still running, then quits.
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit code");

    let raw = pty.raw();
    let raw_text = String::from_utf8_lossy(&raw);
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(
        raw_text.contains("\u{1b}[?2004l"),
        "bracketed paste disabled"
    );
    assert!(raw_text.contains("\u{1b}[?25h"), "cursor restored");
    // The fix this slice makes: the app's last output is a newline after leaving the
    // alternate screen, so the shell does not print its `%` marker.
    // The app writes `\r\n` after leaving the alternate screen; with the pty back in
    // cooked mode ONLCR may turn the `\n` into `\r\n`, so a carriage return right after
    // the leave sequence is the stable marker.
    assert!(
        raw_text.contains("\u{1b}[?1049l\r"),
        "no newline written after restoring the screen"
    );
    // Same pty, after exit: the shell reports cooked mode.
    assert!(
        raw_text.contains("icanon"),
        "stty output missing:\n{raw_text}"
    );
    assert!(
        !raw_text.contains("-icanon"),
        "raw mode was not restored:\n{raw_text}"
    );
    // The session file was written under the temp dir, named by the session id.
    assert!(dir.path().join("e2e.jsonl").exists());
}

/// The panic hook restores the terminal before the panic message is printed.
#[test]
fn pty_panic_path_restores_the_terminal() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--panic-after-enter",
        ],
        20,
        80,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after panic"
    );
    assert_eq!(pty.app_exit_code(), Some(101), "panic must exit 101");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(
        raw_text.contains("\u{1b}[?2004l"),
        "bracketed paste disabled"
    );
    assert!(raw_text.contains("panicked"), "panic message printed");
    assert!(raw_text.contains("icanon"), "cooked mode restored");
    assert!(!raw_text.contains("-icanon"), "raw mode not restored");
}

/// Waits until `predicate` holds, bounded. The sleep is a poll interval, not the
/// synchronization itself.
fn gone_within(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group has a member.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

/// The panic path must not orphan a child the turn had started. The test child is put in its
/// own process group, so "no orphan" is asserted on the group as well as the pid.
#[test]
fn pty_panic_path_does_not_orphan_a_child() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-child");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--panic-with-child",
            "600",
        ],
        20,
        80,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after the panic"
    );
    assert_eq!(pty.app_exit_code(), Some(101), "panic must exit 101");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(raw_text.contains("icanon"), "cooked mode restored");

    let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
        .expect("the app wrote child.pid before panicking")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        gone_within(pid, Duration::from_secs(10)),
        "the child {pid} outlived the panic"
    );
    assert!(
        !group_alive(pid),
        "the child's process group {pid} still has members"
    );
}

/// `--resume` renders the stored history before the first frame. The session file is written
/// with the same storage API the app uses, so the test also pins the on-disk convention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_resume_renders_the_stored_history() {
    let _serial = pty_guard();
    let dir = TempDir::new("resume");
    let path = dir.path().join("resume-me.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "resume-me",
        radian_session::SessionCreatedData::new("/tmp", json!({})),
    )
    .expect("create the session file");
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("OLD-USER-TURN"),
        })
        .await
        .unwrap();
    journal
        .append(agent_core::JournalRecord::Message {
            index: 1,
            message: agent_core::Message::assistant("OLD-ASSISTANT-REPLY"),
        })
        .await
        .unwrap();
    drop(journal);

    let server = MockServer::start(vec![text_reply("new reply")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "resume-me",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("OLD-USER-TURN", 10),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("OLD-ASSISTANT-REPLY", 10),
        "the assistant reply is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("resumed session resume-me", 10),
        "the resume notice is missing:\n{}",
        pty.screen()
    );

    // The resumed session keeps appending to the same file.
    pty.send(b"continue\r");
    assert!(
        pty.wait_for_screen("new reply", 15),
        "the resumed session did not run a turn:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The approval dialog end to end: the tool runs only after the human answers, the prompt
/// shows the **resolved** path, and the decision is written to the session file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_allow_once_runs_the_tool() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let server = MockServer::start(vec![
        // A relative path: the prompt must show the resolved absolute one (the `..`
        // normalization itself is covered by the permission layer's own tests).
        tool_reply(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"approved"}"#,
        ),
        text_reply("WROTE-IT"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "approve",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        // Wide enough that a long macOS temp path is not wrapped in the approval box;
        // the resolved-path assertion below needs it on one line.
        140,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"please write\r");

    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval prompt:\n{}",
        pty.screen()
    );
    // The prompt shows the resolved path: the model asked for `out.txt`, the prompt names
    // the absolute file the write will touch.
    let prompt_screen = pty.screen();
    assert!(prompt_screen.contains("tool: write"), "{prompt_screen}");
    let resolved = std::fs::canonicalize(&ws).unwrap().join("out.txt");
    assert!(
        prompt_screen.contains(&resolved.display().to_string()),
        "the resolved path is missing from the prompt:\n{prompt_screen}"
    );
    // Nothing ran yet.
    assert!(!ws.join("out.txt").exists(), "the tool ran before approval");

    pty.send(b"y");
    assert!(
        pty.wait_for_screen("WROTE-IT", 20),
        "the approved call did not complete:\n{}",
        pty.screen()
    );
    assert!(
        ws.join("out.txt").exists(),
        "the approved write did not land"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    // Every decision is durable in the session file (ADR 0006 §5.9).
    let session = std::fs::read_to_string(dir.path().join("approve.jsonl")).unwrap();
    assert!(session.contains("PermissionResolved"), "{session}");
    assert!(session.contains("\"approver\":\"user\""), "{session}");
    assert!(session.contains("\"decision\":\"allow\""), "{session}");
}

/// Denying feeds a denial result back to the model and never runs the tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_deny_is_fed_back_to_the_model() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve-deny");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"denied.txt","content":"no"}"#),
        text_reply("SAW-THE-DENIAL"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "approve-deny",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no prompt:\n{}",
        pty.screen()
    );
    pty.send(b"n");
    assert!(
        pty.wait_for_screen("SAW-THE-DENIAL", 20),
        "the turn did not continue after the denial:\n{}",
        pty.screen()
    );
    let screen = pty.screen();
    assert!(
        screen.contains("denied"),
        "the denial is not shown: {screen}"
    );
    assert!(!ws.join("denied.txt").exists(), "the denied tool ran");

    pty.quit();
    // The decision is durable and attributed to the user.
    let session = std::fs::read_to_string(dir.path().join("approve-deny.jsonl")).unwrap();
    assert!(session.contains("\"decision\":\"deny\""), "{session}");
    assert!(session.contains("\"approver\":\"user\""), "{session}");
}

/// "Always this session" applies to the next matching call without a second prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_always_skips_the_next_prompt() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve-session");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"same.txt","content":"one"}"#),
        tool_reply("call_2", "write", r#"{"path":"same.txt","content":"two"}"#),
        text_reply("DONE-TWICE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "approve-session",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no first prompt:\n{}",
        pty.screen()
    );
    pty.send(b"a");
    // No further key press: if the second call asked again the turn would stall here.
    assert!(
        pty.wait_for_screen("DONE-TWICE", 20),
        "the session grant did not cover the second call:\n{}",
        pty.screen()
    );
    let written = std::fs::read_to_string(ws.join("same.txt")).unwrap();
    assert_eq!(written, "two", "the second write did not run");

    pty.quit();
}

/// Ctrl-C while the prompt is up cancels the turn: the prompt goes away, the tool never runs,
/// and the app is still usable (a second Ctrl-C quits).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_ctrl_c_while_waiting_for_approval_cancels_the_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve-cancel");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![tool_reply(
        "call_1",
        "write",
        r#"{"path":"never.txt","content":"x"}"#,
    )])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "approve-cancel",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no prompt:\n{}",
        pty.screen()
    );

    pty.send(&[0x03]); // Ctrl-C: cancel the turn, not answer the prompt
    assert!(
        pty.wait_for_screen("cancelled", 20),
        "the turn was not cancelled:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "the prompt outlived the cancelled turn:\n{}",
        pty.screen()
    );
    assert!(!ws.join("never.txt").exists(), "the denied tool ran");

    // The app is not hung: Ctrl-C again quits cleanly.
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The context-budget stop: the request is not sent, the reason is shown honestly (estimated,
/// since nothing was reported yet), and "continue" lets exactly one request through before the
/// guard applies again (ADR 0015 C5).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_budget_stop_sends_nothing_and_continue_relaxes_once() {
    let _serial = pty_guard();
    let dir = TempDir::new("budget");
    let server = MockServer::start(vec![text_reply("BUDGET-REPLY")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "budget",
            "--context-window",
            "1",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hi\r");
    assert!(
        pty.wait_for_screen("stopped before the next request", 20),
        "no budget stop:\n{}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "the blocked request must not be sent"
    );
    let screen = pty.screen();
    assert!(
        screen.contains("(estimated)"),
        "the stop must label the projection as estimated: {screen}"
    );
    assert!(
        !screen.contains("(reported"),
        "nothing was reported yet: {screen}"
    );

    // Continue once: that request goes out.
    pty.send(b"c");
    assert!(
        pty.wait_for_screen("BUDGET-REPLY", 20),
        "the relaxed request did not go out:\n{}",
        pty.screen()
    );
    assert_eq!(server.requests().len(), 1);

    // The guard applies again on the next turn: a second stop, and still one request.
    pty.send(b"again\r");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if pty
            .screen()
            .matches("stopped before the next request")
            .count()
            >= 2
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the guard did not resume:\n{}",
            pty.screen()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        server.requests().len(),
        1,
        "a later request was sent although the guard is back"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Without `--context-window` the guard is off, and the status bar says so instead of showing
/// a guessed number (ADR 0015 C1's `Disabled` row).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_without_a_window_the_status_bar_says_the_guard_is_off() {
    let _serial = pty_guard();
    let dir = TempDir::new("budget-off");
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "budget-off",
        ],
        24,
        110,
    );
    assert!(
        pty.wait_for_screen("context guard off", 10),
        "the status bar does not say the guard is off:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("no window configured"),
        "{}",
        pty.screen()
    );
    pty.quit();
}

/// A failed sandbox probe is a refusal, never an unsandboxed run (ADR 0012 decision 2). The
/// seam is `RADIAN_SANDBOX_PROBE_FAIL=1`, which can only cause a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_failed_sandbox_probe_refuses_instead_of_running_unsandboxed() {
    let _serial = pty_guard();
    let dir = TempDir::new("probe-fail");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("SHOULD-NOT-RUN")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "probe-fail",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after the probe failed"
    );
    assert_eq!(pty.app_exit_code(), Some(1), "a refusal is a failure exit");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw_text.contains("sandbox backend is unavailable")
            || raw_text.contains("failed its probe"),
        "the refusal is not explained: {raw_text}"
    );
    assert!(
        !raw_text.contains("\u{1b}[?1049h"),
        "the app entered the alternate screen although it refused to run"
    );
    assert!(
        server.requests().is_empty(),
        "no request may be sent when the sandbox cannot be enforced"
    );
}

/// The shell approval shows the whole command, and a capped result keeps the truncation
/// marker visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_shell_approval_shows_the_command_and_the_truncation_marker() {
    let _serial = pty_guard();
    let dir = TempDir::new("shell");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // ~200 KiB of output, well past the 64 KiB per-stream default: the marker must show.
    let command = "i=0; while [ $i -lt 20000 ]; do echo filler-$i; i=$((i+1)); done";
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", &json!({"command": command}).to_string()),
        text_reply("SHELL-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "shell",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "a shell command must ask:\n{}",
        pty.screen()
    );
    // The prompt shows the whole command, not a truncated preview.
    let prompt_screen = pty.screen();
    assert!(prompt_screen.contains("command:"), "{prompt_screen}");
    assert!(
        prompt_screen.contains("while [ $i -lt 20000 ]"),
        "the full command is not shown:\n{prompt_screen}"
    );

    pty.send(b"y");
    assert!(
        pty.wait_for_screen("SHELL-DONE", 30),
        "the approved shell command did not finish:\n{}",
        pty.screen()
    );
    let screen = pty.screen();
    assert!(
        screen.contains("bytes discarded"),
        "the truncation marker must be visible:\n{screen}"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// A compound command is never matched by an allow rule, so it always asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_compound_shell_command_always_asks() {
    let _serial = pty_guard();
    let dir = TempDir::new("shell-compound");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let compound = "echo ok; echo two";
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "shell",
            &json!({"command": "echo ok"}).to_string(),
        ),
        text_reply("FIRST-DONE"),
        tool_reply("call_2", "shell", &json!({"command": compound}).to_string()),
        text_reply("SECOND-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "shell-compound",
            "--workspace",
            ws.to_str().unwrap(),
            "--allow-shell-command",
            "echo ok",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // The simple command is allowed by the rule: it runs with no key pressed.
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("FIRST-DONE", 20),
        "the allowed command was blocked:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "an allowed simple command must not ask:\n{}",
        pty.screen()
    );

    // The compound one is not a simple command, so the allow rule cannot match it.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "a compound command must ask:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains(compound),
        "the prompt must show the whole compound command:\n{}",
        pty.screen()
    );
    pty.send(b"n");
    assert!(pty.wait_for_screen("SECOND-DONE", 20), "{}", pty.screen());

    pty.quit();
}

/// A panic inside the driver task is caught by the runtime, but the UI must notice that the
/// session is gone: leave in order, restore the terminal, and report the failure where the
/// user can see it (after the alternate screen is gone).
#[test]
fn pty_a_driver_panic_exits_orderly_and_restores_the_terminal() {
    let _serial = pty_guard();
    let dir = TempDir::new("driver-panic");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "driver-panic",
            "--panic-in-driver",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the TUI did not exit after the driver task panicked"
    );
    // Not 101: the process did not panic, its driver did. The app exits 1 with a message.
    assert_eq!(pty.app_exit_code(), Some(1), "an orderly failure exit");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(
        raw_text.contains("\u{1b}[?2004l"),
        "bracketed paste disabled"
    );
    assert!(raw_text.contains("\u{1b}[?25h"), "cursor restored");
    assert!(raw_text.contains("icanon"), "cooked mode restored");
    assert!(!raw_text.contains("-icanon"), "raw mode not restored");
    // The panic is reported after the alternate screen is gone, so it is readable.
    let after_restore = raw_text
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    assert!(
        after_restore.contains("driver task ended unexpectedly")
            || after_restore.contains("driver task panic test"),
        "the failure was not reported on the normal screen: {after_restore}"
    );
}

/// A bracketed paste arrives whole and multi-line in the input box.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the four pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_multiline_paste_reaches_the_input_box() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("after-paste")]).await;
    let dir = TempDir::new("paste");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10));

    pty.send(b"\x1b[200~paste line one\npaste line two\npaste line three\x1b[201~");
    assert!(
        pty.wait_for_screen("paste line one", 10),
        "paste not shown:\n{}",
        pty.screen()
    );
    let screen = pty.screen();
    assert!(
        screen.contains("paste line three"),
        "all paste lines kept:\n{screen}"
    );
    assert!(
        !screen.contains("paste line one\npaste line two"),
        "the paste is in the input box, not the transcript"
    );

    pty.quit();
}

/// The second request must not carry the first response's reasoning (ADR 0014 R4), verified
/// on the real UI path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the four pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_reasoning_is_not_sent_back_to_the_provider() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(json!({"reasoning_content": "SECRET-REASONING-TEXT"}), None),
            chunk(json!({"content": "first answer"}), None),
            chunk(json!({}), Some("stop")),
        ]),
        text_reply("second answer"),
    ])
    .await;
    let dir = TempDir::new("reasoning");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10));

    pty.send(b"first\r");
    assert!(pty.wait_for_screen("first answer", 15), "{}", pty.screen());
    pty.send(b"second\r");
    assert!(pty.wait_for_screen("second answer", 15), "{}", pty.screen());

    let requests = server.requests();
    assert!(requests.len() >= 2, "two requests expected");
    let second = &requests[1].body;
    assert!(
        !second.contains("reasoning"),
        "reasoning leaked into the request: {second}"
    );
    // The assistant message itself is still in history.
    assert!(second.contains("first answer"), "{second}");

    pty.quit();
}

/// The TUI's `--header` is the same parser `agent-cli` uses: `{session_id}` expands to this
/// process's session id, so a routing header can name the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_header_value_expands_the_session_id() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-new");
    let server = MockServer::start(vec![text_reply("HEADER-OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "abc",
            "--header",
            "x-s={session_id}",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hi\r");
    assert!(
        pty.wait_for_screen("HEADER-OK", 15),
        "reply not rendered:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(
        server.requests()[0].header("x-s"),
        Some("abc"),
        "the placeholder must expand to the session id"
    );
}

/// A resumed session expands `{session_id}` to the id being resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_resumed_session_header_uses_the_resumed_id() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-resume");
    let server = MockServer::start(vec![text_reply("FIRST"), text_reply("SECOND")]).await;

    let mut first = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "abc",
        ],
        24,
        100,
    );
    assert!(first.wait_for_screen("message", 10), "{}", first.screen());
    first.send(b"one\r");
    assert!(first.wait_for_screen("FIRST", 15), "{}", first.screen());
    first.quit();
    assert_eq!(first.app_exit_code(), Some(0));

    let mut second = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "abc",
            "--header",
            "x-s={session_id}",
        ],
        24,
        100,
    );
    assert!(second.wait_for_screen("message", 10), "{}", second.screen());
    second.send(b"two\r");
    assert!(second.wait_for_screen("SECOND", 15), "{}", second.screen());
    second.quit();
    assert_eq!(second.app_exit_code(), Some(0));
    assert_eq!(
        server.requests()[1].header("x-s"),
        Some("abc"),
        "the resumed id must be the one expanded"
    );
}

/// `authorization` is refused (the key has one source), and the refusal never echoes the
/// value. The app refuses before entering the alternate screen, so it exits with an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_an_authorization_header_is_refused_without_echoing_its_value() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-auth");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "auth",
            "--header",
            "Authorization=Bearer sk-from-a-header",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(10).is_some(),
        "the app did not refuse:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    assert!(raw.contains("authorization"), "{raw}");
    assert!(
        !raw.contains("sk-from-a-header"),
        "the header value leaked into the error: {raw}"
    );
    assert!(server.requests().is_empty(), "no request may be sent");
    assert_eq!(pty.app_exit_code(), Some(1));
}

/// A `--header` with no `=` is a usage error whose message does not echo the argument: the
/// whole argument may be a pasted credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_malformed_header_error_does_not_echo_its_value() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-bad");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "bad",
            "--header",
            "sk-secret-value",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(10).is_some(),
        "the app did not refuse:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    assert!(raw.contains("must be NAME=VALUE"), "{raw}");
    assert!(
        !raw.contains("sk-secret-value"),
        "the argument leaked into the error: {raw}"
    );
}

/// MCP servers are mounted from the same config file the CLI reads; a model call to one of
/// their tools goes through the approval prompt, returns its result, and the status bar shows
/// the servers and tools that connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_mcp_tools_run_through_approval_and_the_status_bar_shows_them() {
    let _serial = pty_guard();
    let dir = TempDir::new("mcp");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let log = dir.path().join("mcp.log");
    let config = mcp_config(dir.path(), &log);
    let server = MockServer::start(vec![
        tool_reply("call_1", "fx__echo", r#"{"hello": "mcp"}"#),
        text_reply("MCP-DONE"),
    ])
    .await;

    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "mcp",
            "--workspace",
            ws.to_str().unwrap(),
            "--mcp-config",
            config.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // The session is open, so the status bar reports what connected. The fixture exposes 16
    // tools.
    assert!(
        pty.wait_for_screen("mcp 1/16", 20),
        "the status bar does not show the MCP counts:\n{}",
        pty.screen()
    );

    pty.send(b"call the echo tool\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval for the MCP tool:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("fx__echo"),
        "the prompt must name the tool:\n{}",
        pty.screen()
    );
    let before = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !before.contains("tools/call"),
        "the server ran the tool before approval was given: {before}"
    );

    pty.send(b"y");
    assert!(
        pty.wait_for_screen("MCP-DONE", 20),
        "the approved MCP call did not complete:\n{}",
        pty.screen()
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("tools/call"),
        "the fixture never received the call: {logged}"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0012 option B keeps MCP servers running outside the sandbox under `workspace-write`,
/// so the startup notice is the only thing that tells the user. A test holds it in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_the_workspace_write_notice_names_the_mcp_sandbox_gap() {
    let _serial = pty_guard();
    let dir = TempDir::new("notice");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "notice",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // The notice wraps across lines; collapse whitespace before matching the sentence.
    let notice = pty
        .screen()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notice.contains("MCP servers are NOT sandboxed"),
        "the startup notice must name the MCP sandbox gap: {notice}"
    );
    assert!(
        notice.contains("not a complete credential boundary"),
        "the notice must not claim credential isolation: {notice}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `--yes` removes the question, not the rules: a shell command under `workspace-write`
/// (which would ask) runs without a prompt, through the sandbox, and the decision is still
/// written to the audit record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_yes_runs_an_ask_without_a_prompt_and_still_audits_it() {
    let _serial = pty_guard();
    let dir = TempDir::new("yes");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "shell",
            &json!({"command": "echo YES-RAN; echo x | cat"}).to_string(),
        ),
        text_reply("YES-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "yes",
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    assert!(
        pty.screen()
            .contains("--yes: tool calls are approved without asking"),
        "the startup must say approvals are off:\n{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("YES-DONE", 30),
        "the command did not run without a prompt:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "--yes must not show a prompt:\n{}",
        pty.screen()
    );
    assert!(pty.screen().contains("YES-RAN"), "{}", pty.screen());

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    let session = std::fs::read_to_string(dir.path().join("yes.jsonl")).unwrap();
    assert!(
        session.contains("PermissionResolved") && session.contains("\"decision\":\"allow\""),
        "the decision must still be audited: {session}"
    );
}

/// `--max-model-calls` reaches the turn: with a limit of 1, the reply after the tool call is
/// never requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_max_model_calls_limits_the_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("max-calls");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "list", &json!({"path": "."}).to_string()),
        text_reply("SECOND-CALL"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "max-calls",
            "--workspace",
            ws.to_str().unwrap(),
            "--max-model-calls",
            "1",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    let path = dir.path().join("max-calls.jsonl");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&path)
        .map(|text| text.contains("TurnFinished"))
        .unwrap_or(false)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish:\n{}",
            pty.screen()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests().len(), 1, "the limit of 1 was not applied");
    assert!(!pty.screen().contains("SECOND-CALL"), "{}", pty.screen());

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The session directory, created empty by the caller, must stay empty when startup fails.
fn assert_session_dir_empty(dir: &Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.is_empty(),
        "a failed start left files behind: {names:?}"
    );
}

/// Acceptance 1: a `--workspace` that does not exist fails as a **workspace** error, names
/// the path, does not blame the sandbox, does not suggest `full-access`, and creates no
/// session file (dogfooding D1/D2).
#[test]
fn pty_a_missing_workspace_is_a_workspace_error_and_leaves_no_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("missing-workspace");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let missing = dir.path().join("no-such-workspace");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "missing-workspace",
            "--workspace",
            missing.to_str().unwrap(),
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit on a bad workspace"
    );
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw.contains(missing.to_str().unwrap()), "{raw}");
    assert!(raw.contains("workspace"), "{raw}");
    assert!(
        !raw.contains("sandbox backend"),
        "a bad workspace must not blame the sandbox: {raw}"
    );
    assert!(
        !raw.contains("full-access"),
        "a bad workspace must not suggest full-access: {raw}"
    );
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "a refusal must not enter the alternate screen: {raw}"
    );
    assert_session_dir_empty(&session_dir);
}

/// Acceptance 2 and 4: a failed probe still refuses, leaves no session file, and the same
/// `--session-id` with corrected parameters then creates the session (dogfooding D2).
#[test]
fn pty_a_failed_probe_leaves_no_session_and_the_retry_creates_one() {
    let _serial = pty_guard();
    let dir = TempDir::new("probe-no-session");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "retry",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the refusal did not exit");
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains("sandbox backend is unavailable") || raw.contains("failed its probe"),
        "{raw}"
    );
    assert_session_dir_empty(&session_dir);

    // The corrected retry, same id: the session is created. `--panic-after-enter` exits
    // quickly and needs no server; the session is created before the terminal is entered.
    let mut retry = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "retry",
            "--workspace",
            ws.to_str().unwrap(),
            "--panic-after-enter",
        ],
        20,
        100,
    );
    assert!(retry.wait_for_exit(15).is_some(), "the retry did not exit");
    assert!(
        session_dir.join("retry.jsonl").exists(),
        "the corrected retry created the session"
    );
}

/// Acceptance 3: a missing or malformed MCP config file fails before the session exists.
#[test]
fn pty_a_bad_mcp_config_creates_no_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("bad-mcp");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, r#"{"servers": [], "bogus": 1}"#).unwrap();
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "bad-mcp",
            "--workspace",
            ws.to_str().unwrap(),
            // full-access keeps this test independent of the platform's sandbox backend; the
            // bad config is checked before any session file exists either way.
            "--profile",
            "full-access",
            "--mcp-config",
            bad.to_str().unwrap(),
        ],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the refusal did not exit");
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw.contains(bad.to_str().unwrap()), "{raw}");
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "a bad MCP config must not enter the alternate screen: {raw}"
    );
    assert_session_dir_empty(&session_dir);
}

/// Acceptance 5: a clean exit prints one reason line on the normal screen, after the
/// alternate screen is gone, and the line ends with a newline (dogfooding D4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_clean_exit_prints_one_reason_line() {
    let _serial = pty_guard();
    let dir = TempDir::new("exit-reason");
    let server = MockServer::start(vec![text_reply("unused")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "exit-reason",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit");

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let after = raw
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    let reason = "session exit-reason saved; resume with `--resume exit-reason`";
    let start = after
        .find(reason)
        .unwrap_or_else(|| panic!("no exit reason on the normal screen: {after}"));
    let rest = &after[start..];
    let end = rest
        .find('\n')
        .unwrap_or_else(|| panic!("the exit reason has no trailing newline: {rest}"));
    assert!(
        !rest[..end].contains("EXITED:"),
        "the reason line must end before the shell continues: {rest}"
    );
}

/// Acceptance 6: a panic writes `<id>.panic.log` with the time, thread and panic text, and
/// redacts header values and the key even when the panic text mentions them (dogfooding D4).
#[test]
fn pty_a_panic_writes_a_redacted_panic_log() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-log");
    let secret = "SECRET-HEADER-VALUE-xyz";
    let key = "SECRET-KEY-abc";
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "panic-log",
            "--header",
            &format!("X-Test={secret}"),
            "--panic-message",
            &format!("boom {secret} and {key}"),
        ],
        &[("OPENAI_API_KEY", key)],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the app did not exit");
    assert_eq!(pty.app_exit_code(), Some(101), "a panic exits 101");

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let after = raw
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    assert!(
        after.contains("panicked"),
        "the panic reason is on the normal screen: {after}"
    );

    let log_path = dir.path().join("panic-log.panic.log");
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("no panic log at {}: {e}", log_path.display()));
    assert!(log.contains("boom"), "the panic text is in the log: {log}");
    assert!(log.contains("thread="), "the log names the thread: {log}");
    assert!(
        log.contains('<'),
        "the log has a timestamp token before the thread: {log}"
    );
    assert!(
        !log.contains(secret),
        "the header value must be redacted from the log: {log}"
    );
    assert!(
        !log.contains(key),
        "the key must be redacted from the log: {log}"
    );
    assert!(
        log.contains("<redacted>"),
        "redaction marker missing: {log}"
    );
    // Nothing on the terminal either: the hook replaces Rust's default one, which would print
    // the raw panic text to stderr.
    assert!(
        !raw.contains(secret) && !raw.contains(key),
        "the terminal output must not carry the header value or the key: {raw}"
    );
    assert!(
        raw.contains("<redacted>"),
        "the redacted report reaches stderr: {raw}"
    );
}

/// The pid of the app the pty's `sh -c` wrapper started.
fn app_pid(pty: &Pty) -> String {
    let shell = pty.child.process_id().expect("the wrapper shell has a pid");
    let out = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(shell.to_string())
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .expect("the app runs under the wrapper shell")
        .trim()
        .to_string()
}

/// Dogfooding D7: SIGTERM and SIGHUP leave like a quit. The terminal is restored, one reason
/// line names the signal and how to resume, and the exit code is 128 + the signal.
#[test]
fn pty_sigterm_and_sighup_restore_the_terminal_and_say_why() {
    for (signal, name, code) in [("TERM", "SIGTERM", 143), ("HUP", "SIGHUP", 129)] {
        let _serial = pty_guard();
        let dir = TempDir::new("signal");
        let id = format!("signal-{}", name.to_lowercase());
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                &id,
            ],
            20,
            100,
        );
        assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
        let pid = app_pid(&pty);
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(&pid)
            .status()
            .unwrap();
        assert!(status.success(), "kill -{signal} {pid}");
        assert!(
            pty.wait_for_exit(15).is_some(),
            "{name}: the app did not exit"
        );
        assert_eq!(pty.app_exit_code(), Some(code), "{name}: 128 + signal");
        let raw = String::from_utf8_lossy(&pty.raw()).to_string();
        let (_, after) = raw
            .rsplit_once("\u{1b}[?1049l")
            .unwrap_or_else(|| panic!("{name}: the alternate screen was not left: {raw}"));
        assert!(
            after.contains(&format!("received {name}"))
                && after.contains(&format!("--resume {id}")),
            "{name}: the reason line is on the normal screen: {after}"
        );
        assert!(
            raw.contains("icanon") && !raw.contains("-icanon"),
            "{name}: cooked mode restored"
        );
    }
}

// ---------- provider selection: anthropic (Phase 7 / anthost) ----------

/// One Anthropic text reply (the minimal end_turn stream).
fn anth_text_reply(text: &str) -> anth::Reply {
    anth::Reply::sse(&[
        anth::message_start(Some(json!({"input_tokens": 3, "output_tokens": 1}))),
        anth::text_start(0),
        anth::text_delta(0, text),
        anth::block_stop(0),
        anth::message_delta("end_turn", Some(json!({"output_tokens": 2}))),
        anth::message_stop(),
    ])
}

/// `--provider anthropic` in the TUI talks the Messages API and shows the reply; the session
/// snapshot names anthropic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_anthropic_provider_sends_a_messages_request_and_renders_the_reply() {
    let _serial = pty_guard();
    let server = anth::MockServer::start(vec![anth_text_reply("ANTHROPIC-TUI-REPLY")]).await;
    let dir = TempDir::new("anthropic");
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "claude",
            "--provider",
            "anthropic",
            "--base-url",
            &server.base_url,
            "--api-key-env",
            "ANTHROPIC_API_KEY",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "anthro",
        ],
        &[("ANTHROPIC_API_KEY", "sk-ant-tui-key")],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("ANTHROPIC-TUI-REPLY", 20),
        "the reply is not on screen:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some("sk-ant-tui-key"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));

    // The snapshot names the real provider, and never the key.
    let session = std::fs::read_to_string(dir.path().join("anthro.jsonl")).unwrap();
    assert!(session.contains("\"provider\":\"anthropic\""), "{session}");
    assert!(
        !session.contains("sk-ant-tui-key"),
        "the key reached the session file"
    );
}

/// D15: naming an existing session with `--session-id` points at `--resume`, fails before the
/// terminal is entered, and does not touch the file.
#[test]
fn pty_session_id_already_exists_points_at_resume() {
    let _serial = pty_guard();
    let dir = TempDir::new("dup-id");
    // First run: create the session, then leave via the panic hook (quick, no server needed).
    let mut first = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "dup",
            "--panic-after-enter",
        ],
        20,
        80,
    );
    assert!(
        first.wait_for_exit(15).is_some(),
        "the first run did not exit"
    );
    assert!(dir.path().join("dup.jsonl").exists());
    let before = std::fs::read(dir.path().join("dup.jsonl")).unwrap();

    let mut second = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "dup",
        ],
        20,
        80,
    );
    assert!(
        second.wait_for_exit(15).is_some(),
        "the second run did not exit"
    );
    assert_eq!(second.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&second.raw()).to_string();
    assert!(raw.contains("already exists"), "{raw}");
    assert!(raw.contains("--resume dup"), "{raw}");
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "the refusal must not enter the alternate screen: {raw}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("dup.jsonl")).unwrap(),
        before,
        "the existing session must not be touched"
    );
}

/// `--thinking` and `--max-tokens` are Anthropic-only: under the default openai-compatible
/// provider the TUI refuses them before the terminal, and creates no session file.
#[test]
fn pty_anthropic_only_flags_are_refused_under_openai_compatible() {
    let _serial = pty_guard();
    for (label, extra) in [
        ("thinking", ["--thinking", "adaptive"]),
        ("max-tokens", ["--max-tokens", "100"]),
    ] {
        let dir = TempDir::new(&format!("flag-{label}"));
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut args = vec![
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "flagged",
        ];
        args.extend(extra);
        let mut pty = Pty::spawn(&args, 20, 80);
        assert!(
            pty.wait_for_exit(15).is_some(),
            "{label}: the refusal did not exit"
        );
        assert_eq!(pty.app_exit_code(), Some(1), "{label}");
        let raw = String::from_utf8_lossy(&pty.raw()).to_string();
        assert!(raw.contains("anthropic"), "{label}: {raw}");
        assert!(
            !raw.contains("\u{1b}[?1049h"),
            "{label}: must fail before the terminal: {raw}"
        );
        assert!(
            std::fs::read_dir(&session_dir).unwrap().next().is_none(),
            "{label}: a session file was created"
        );
    }
}

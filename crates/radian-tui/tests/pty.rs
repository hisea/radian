//! End-to-end through a pty: the bin is started under `portable-pty`, driven by bytes, and
//! its rendering is read from a `vt100` screen model. **The full-screen program is never run
//! in the test runner's own terminal.**

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
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
    // The session file was written under the temp dir.
    assert!(dir.path().join("session.jsonl").exists());
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

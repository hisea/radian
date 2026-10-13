//! Startup, `--resume`, first-token and tool-round-trip baseline for `radian-tui` (plan
//! Phase 7), measured through a real pty against an in-process mock HTTP server, so the
//! HTTP adapter and the render path are included but no real network is.
//!
//! The full-screen program is **never** run in the test runner's own terminal. This file uses
//! a small `libc::openpty` + `std::process::Command` harness rather than `portable-pty`: on
//! this machine `portable-pty` delayed the child's first byte by ~2 s (its fork of the large
//! test binary; `env_clear` was ruled out), which is a harness cost, not the app's. The same
//! app under a direct pty here starts in ~0.1 s.
//!
//! Measurement, not an assertion: `#[ignore]`d. Run:
//!
//! ```sh
//! cargo test --release -p radian-tui --test perf_startup -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The detection poll is 1 ms, so every number is quantized upward by up to ~1 ms.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_radian-tui");
const POLL: Duration = Duration::from_millis(1);

struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-perf-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A child on a pty: `libc::openpty`, `std::process::Command` (posix_spawn), a reader thread.
struct Pty {
    master: File,
    bytes: Arc<Mutex<Vec<u8>>>,
    first_byte: Arc<Mutex<Option<Instant>>>,
    start: Instant,
    child: Child,
    /// A per-pty `HOME`: `~/.radian/` (ADR 0020) resolves under it, so a manual `--ignored`
    /// run never writes to the real `HOME`.
    _home: TempDir,
}
impl Drop for Pty {
    fn drop(&mut self) {
        // This harness spawns the binary **directly** (`Command::new(BIN)`, no `sh` wrapper),
        // so `child.kill()` is already the app kill and leaves no orphan. (`tests/pty.rs` used
        // a shell wrapper and had to switch to a process-group kill.)
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Pty {
    fn spawn(args: &[&str], rows: u16, cols: u16) -> Self {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        ws.ws_row = rows;
        ws.ws_col = cols;
        // SAFETY: openpty writes the two fds and takes null termios/name pointers.
        // `winp` is `*mut winsize` on macOS but `*const winsize` on Linux/glibc, so pass a raw
        // pointer rather than `&mut ws` (clippy's `unnecessary_mut_passed` fires on Linux).
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(ws),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: the fds are fresh from openpty; `OwnedFd` closes them.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

        let start = Instant::now();
        let mut cmd = Command::new(BIN);
        cmd.args(args);
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()));
        cmd.stdout(Stdio::from(slave.try_clone().unwrap()));
        cmd.stderr(Stdio::from(slave));
        cmd.env_clear();
        // A fresh HOME per pty keeps the harness hermetic (ADR 0013 diagnostics).
        let home = TempDir::new("perf-home");
        for name in ["PATH", "TMPDIR", "LANG", "TERM"] {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("HOME", home.path());
        // `cmd.env_clear()` above is what makes this hermetic: neither `RADIAN_HOME` nor
        // `RADIAN_CONFIG` reaches the child, so a config can only come from this temp `HOME`.
        // No real models.dev from a test (ADR 0018): a closed local port fails fast.
        cmd.env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json");
        let child = cmd.spawn().unwrap();

        let mut master = File::from(master);
        let mut reader = master.try_clone().unwrap();
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let first_byte: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let log = Arc::clone(&bytes);
        let first = Arc::clone(&first_byte);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut slot = first.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(Instant::now());
                        }
                        drop(slot);
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                }
            }
        });
        let _ = master.flush();
        Self {
            master,
            bytes,
            first_byte,
            start,
            child,
            _home: home,
        }
    }
    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }
    fn contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        self.bytes
            .lock()
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
    }
    fn first_byte_ms(&self) -> f64 {
        self.first_byte
            .lock()
            .unwrap()
            .map(|t| (t - self.start).as_secs_f64() * 1000.0)
            .unwrap_or(-1.0)
    }
    /// Returns the elapsed time from `start` once `needle` has arrived, or panics.
    fn wait_for(&self, needle: &str, start: Instant, secs: u64) -> Duration {
        let deadline = start + Duration::from_secs(secs);
        loop {
            if self.contains(needle) {
                return start.elapsed();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; raw so far:\n{}",
                strip_ansi(&String::from_utf8_lossy(&self.bytes.lock().unwrap()))
            );
            std::thread::sleep(POLL);
        }
    }
    /// Ctrl-C, then a kill as a last resort: a measurement must not leave a process behind.
    fn quit(&mut self) {
        for _ in 0..4 {
            let _ = self.master.write_all(&[0x03]);
            let _ = self.master.flush();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(POLL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A minimal CSI/OSC stripper, only for failure messages.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}
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

fn median_p95(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort();
    (v[v.len() / 2], v[(v.len() * 95 / 100).min(v.len() - 1)])
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Writes a valid v1 session file with `n` messages; the last one is a visible marker.
fn write_session(path: &std::path::Path, session_id: &str, n: usize) {
    use agent_core::Message;
    use std::io::BufWriter;
    let mut out = BufWriter::new(File::create(path).unwrap());
    let ts = "2026-10-12T00:00:00.000000Z";
    writeln!(
        out,
        "{}",
        json!({"schema_version":1,"session_id":session_id,"seq":0,"ts":ts,"event":"SessionCreated",
               "critical":true,"data":{"workspace":"/work/example","config_snapshot":{}}})
    )
    .unwrap();
    for i in 0..n {
        let message = if i + 1 == n {
            Message::assistant("PERF-RESUME-END")
        } else if i % 2 == 0 {
            Message::user(format!("resumed user message {i}"))
        } else {
            Message::assistant(format!("resumed assistant reply {i}"))
        };
        writeln!(
            out,
            "{}",
            json!({"schema_version":1,"session_id":session_id,"seq":i + 1,"ts":ts,
                   "event":"MessageAppended","critical":true,
                   "data":{"index":i,"message":serde_json::to_value(&message).unwrap()}})
        )
        .unwrap();
    }
    out.flush().unwrap();
}

static PTY_LOCK: Mutex<()> = Mutex::new(());
fn guard() -> std::sync::MutexGuard<'static, ()> {
    PTY_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The sandbox probe that `workspace-write` startup includes, measured on its own.
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_sandbox_probe() {
    let dir = TempDir::new("probe");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let sandbox =
            radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &ws).unwrap();
        samples.push(start.elapsed());
        std::hint::black_box(sandbox.mode());
    }
    let (m, p) = median_p95(samples);
    println!(
        "sandbox probe (workspace-write): median {:.1} ms  p95 {:.1} ms",
        ms(m),
        ms(p)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
async fn perf_startup_and_resume() {
    let _serial = guard();
    let server = MockServer::start(vec![text_reply("ok")]).await;

    // Warm the page cache / dyld once, so the p95 is not a single cold run.
    {
        let dir = TempDir::new("warmup");
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                &server.base_url,
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                "warmup",
            ],
            24,
            100,
        );
        pty.wait_for("message", pty.start, 30);
        pty.quit();
    }

    // 1. Startup to the input box (a fresh session).
    let mut startup = Vec::new();
    for i in 0..10 {
        let dir = TempDir::new(&format!("startup-{i}"));
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                &server.base_url,
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                &format!("startup-{i}"),
            ],
            24,
            100,
        );
        let elapsed = pty.wait_for("message", pty.start, 20);
        let first = pty.first_byte_ms();
        println!(
            "  startup sample {i}: first byte {first:.1} ms, input box {:.1} ms",
            ms(elapsed)
        );
        startup.push(elapsed);
        pty.quit();
    }
    let (m, p) = median_p95(startup);
    println!(
        "tui startup to input box: median {:.1} ms  p95 {:.1} ms",
        ms(m),
        ms(p)
    );

    // 2. `--resume` of a 1k / 10k message session.
    for n in [1_000usize, 10_000] {
        let dir = TempDir::new(&format!("resume-{n}"));
        write_session(&dir.path().join("resume.jsonl"), "resume", n);
        let mut times = Vec::new();
        for _ in 0..5 {
            let mut pty = Pty::spawn(
                &[
                    "--model",
                    "mock",
                    "--base-url",
                    &server.base_url,
                    "--session-dir",
                    dir.path().to_str().unwrap(),
                    "--resume",
                    "resume",
                ],
                24,
                100,
            );
            let elapsed = pty.wait_for("PERF-RESUME-END", pty.start, 60);
            times.push(elapsed);
            pty.quit();
        }
        let (m, p) = median_p95(times);
        println!(
            "tui --resume {n:>5} msgs to last message: median {:.1} ms  p95 {:.1} ms",
            ms(m),
            ms(p)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
async fn perf_first_token() {
    let _serial = guard();
    let mut first = Vec::new();
    for i in 0..10 {
        let dir = TempDir::new(&format!("first-token-{i}"));
        let server = MockServer::start(vec![text_reply("FIRST-TOKEN-REPLY")]).await;
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                &server.base_url,
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                "first-token",
            ],
            24,
            100,
        );
        pty.wait_for("message", pty.start, 30);
        let start = Instant::now();
        pty.send(b"hi\r");
        first.push(pty.wait_for("FIRST-TOKEN-REPLY", start, 20));
        pty.quit();
    }
    let (m, p) = median_p95(first);
    println!(
        "first token (pty end to end): median {:.1} ms  p95 {:.1} ms",
        ms(m),
        ms(p)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
async fn perf_tool_round_trip() {
    let _serial = guard();
    let mut samples = Vec::new();
    for i in 0..5 {
        let dir = TempDir::new(&format!("tool-rt-{i}"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let server = MockServer::start(vec![
            tool_reply("call_1", "list", r#"{"path":"."}"#),
            text_reply("TOOL-ROUND-TRIP-DONE"),
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
                "tool-rt",
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
            ],
            30,
            110,
        );
        pty.wait_for("message", pty.start, 30);
        let start = Instant::now();
        pty.send(b"go\r");
        samples.push(pty.wait_for("TOOL-ROUND-TRIP-DONE", start, 30));
        pty.quit();
    }
    let (m, p) = median_p95(samples);
    println!(
        "tool round trip (pty end to end, 1 tool + 2 calls): median {:.1} ms  p95 {:.1} ms",
        ms(m),
        ms(p)
    );
}

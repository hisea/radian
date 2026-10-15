//! Long-session `--resume` for the `agent` CLI (Phase 5): time from process start to the
//! **first model request** on the wire.
//!
//! Measurements, not assertions: `#[ignore]`d. Run:
//!
//! ```sh
//! cargo test --release -p agent-cli --test perf_long_resume -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The mock is a bare TCP listener: it accepts one connection, waits for the request headers
//! (`\r\n\r\n`), records the time and closes. The adapter's in-process mock server refuses a
//! body over 16 MiB, and a 50k+ message resume exceeds that, so a plain socket is the only way
//! to see the request at all sizes. The agent then fails on the closed connection; the test
//! kills it.

#[path = "../../radian-session/tests/common/long_session.rs"]
mod long_session;

use std::io::Read;
use std::net::TcpListener;
mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use long_session::write_long_session;

/// The `radian` binary (ADR 0032: the headless command is `radian exec`), refusing a stale
/// build - `cargo test -p agent-cli` does not rebuild `radian-tui`.
fn bin() -> PathBuf {
    common::radian_bin()
}
const POLL: Duration = Duration::from_millis(1);

struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "agent-cli-perf-long-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A bare TCP listener: accepts one connection, waits for the request headers, and reports how
/// long they took. The connection is closed right after, so the body write fails; that is
/// fine, the measurement is done.
fn header_server(start: Instant) -> (u16, mpsc::Receiver<Duration>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut window = [0u8; 1];
        let mut last4 = [0u8; 4];
        loop {
            match stream.read(&mut window) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    last4.rotate_left(1);
                    last4[3] = window[0];
                    if &last4 == b"\r\n\r\n" {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(start.elapsed());
    });
    (port, rx)
}

fn median_p95(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort();
    (v[v.len() / 2], v[(v.len() * 95 / 100).min(v.len() - 1)])
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Spawns `radian exec --resume <id> -p x` against a header server and returns the time to the
/// request headers. `None` if the listener never heard from it.
async fn resume_to_first_request(dir: &Path, session_id: &str) -> Option<Duration> {
    let start = Instant::now();
    let (port, rx) = header_server(start);
    let mut child: Child = Command::new(bin())
        .arg("exec")
        .args([
            "--model",
            "m",
            "--base-url",
            &format!("http://127.0.0.1:{port}/v1"),
            "--session-dir",
            dir.to_str().unwrap(),
            "--offline-catalog",
            "--resume",
            session_id,
            "-p",
            "x",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match rx.try_recv() {
            Ok(elapsed) => {
                let _ = child.kill();
                let _ = child.wait();
                return Some(elapsed);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the request headers"
        );
        tokio::time::sleep(POLL).await;
    }
}

const SIZES: [usize; 4] = [1_000, 10_000, 50_000, 100_000];
const VARIANTS: [usize; 2] = [0, 3];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
async fn perf_long_agent_resume() {
    for n in SIZES {
        for compactions in VARIANTS {
            let dir = TempDir::new(&format!("resume-{n}-{compactions}"));
            let stats =
                write_long_session(&dir.path().join("resume.jsonl"), "resume", n, compactions);
            let mut times = Vec::new();
            for _ in 0..5 {
                if let Some(elapsed) = resume_to_first_request(dir.path(), "resume").await {
                    times.push(elapsed);
                }
            }
            let context = format!(
                "{n:>6} ({compactions} compactions, raw={} live={} bytes={})",
                stats.raw_messages, stats.live_messages, stats.bytes
            );
            if times.len() != 5 {
                println!(
                    "radian exec --resume {context}: only {}/5 runs reached the request",
                    times.len()
                );
            }
            let (median, p95) = median_p95(times);
            println!(
                "radian exec --resume {context} to first request: median {:.1} ms  p95 {:.1} ms",
                ms(median),
                ms(p95)
            );
        }
    }
}

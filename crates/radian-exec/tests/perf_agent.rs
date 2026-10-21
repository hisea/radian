//! Startup and `--resume` baseline for the `agent` CLI (plan Phase 7).
//!
//! `agent` prints its `> ` prompt to stderr and reads stdin through a pipe-friendly line
//! reader, so no pty is needed. stdin is held open so the process reaches the prompt and
//! stays there; `--base-url` points at a dead port so no network is touched (no turn runs).
//!
//! Measurement, not an assertion: `#[ignore]`d. Run:
//!
//! ```sh
//! cargo test --release -p radian-exec --test perf_agent -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The `radian` binary (ADR 0032: the headless command is `radian exec`), refusing a stale
/// build - `cargo test -p radian-exec` does not rebuild `radian-tui`.
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
            "radian-exec-perf-{}-{label}-{n}",
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

/// A running `agent` with its stderr collected by a reader thread.
struct Agent {
    child: Child,
    #[allow(dead_code)]
    stdin: ChildStdin,
    stderr: Arc<Mutex<Vec<u8>>>,
    start: Instant,
}
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Agent {
    fn spawn(args: &[&str]) -> Self {
        let start = Instant::now();
        // Hermetic like the other harnesses: a fresh `~/.radian` root and `HOME`, and no
        // `RADIAN_CONFIG`, so a perf run can never read the user's real config file.
        let home = std::env::temp_dir().join(format!(
            "agent-perf-home-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let mut child = Command::new(bin())
            .arg("exec")
            .args(args)
            .env_remove("RADIAN_CONFIG")
            .env("RADIAN_HOME", &home)
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        let stderr: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr_pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            stdin,
            stderr,
            start,
        }
    }
    fn contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        self.stderr
            .lock()
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
    }
    /// Walks the child along until `needle` has appeared, returning the elapsed time.
    fn wait_for(&mut self, needle: &str, secs: u64) -> Duration {
        let deadline = self.start + Duration::from_secs(secs);
        loop {
            if self.contains(needle) {
                return self.start.elapsed();
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "agent exited early ({status:?}); stderr:\n{}",
                    String::from_utf8_lossy(&self.stderr.lock().unwrap())
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; stderr:\n{}",
                String::from_utf8_lossy(&self.stderr.lock().unwrap())
            );
            std::thread::sleep(POLL);
        }
    }
}

/// Writes a valid v1 session file with `n` messages (the CLI resumes it by id).
fn write_session(path: &Path, session_id: &str, n: usize) {
    use radian_core::Message;
    use std::io::BufWriter;
    let mut out = BufWriter::new(std::fs::File::create(path).unwrap());
    let ts = "2026-10-12T00:00:00.000000Z";
    writeln!(
        out,
        "{}",
        serde_json::json!({"schema_version":1,"session_id":session_id,"seq":0,"ts":ts,
            "event":"SessionCreated","critical":true,
            "data":{"workspace":"/work/example","config_snapshot":{}}})
    )
    .unwrap();
    for i in 0..n {
        let message = if i % 2 == 0 {
            Message::user(format!("resumed user message {i}"))
        } else {
            Message::assistant(format!("resumed assistant reply {i}"))
        };
        writeln!(
            out,
            "{}",
            serde_json::json!({"schema_version":1,"session_id":session_id,"seq":i + 1,"ts":ts,
                "event":"MessageAppended","critical":true,
                "data":{"index":i,"message":serde_json::to_value(&message).unwrap()}})
        )
        .unwrap();
    }
    out.flush().unwrap();
}

fn median_p95(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort();
    (v[v.len() / 2], v[(v.len() * 95 / 100).min(v.len() - 1)])
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_agent_startup_and_resume() {
    // Warm the page cache / dyld once, so the p95 is not a single cold run.
    {
        let dir = TempDir::new("warmup");
        let mut agent = Agent::spawn(&[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "warmup",
        ]);
        agent.wait_for("> ", 30);
    }

    // 1. Cold startup to the `> ` prompt.
    let mut startup = Vec::new();
    for i in 0..10 {
        let dir = TempDir::new(&format!("startup-{i}"));
        let mut agent = Agent::spawn(&[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            &format!("startup-{i}"),
        ]);
        startup.push(agent.wait_for("> ", 30));
    }
    let (m, p) = median_p95(startup);
    println!(
        "agent startup to prompt: median {:.1} ms  p95 {:.1} ms",
        ms(m),
        ms(p)
    );

    // 2. `--resume` of a 1k / 10k message session.
    for n in [1_000usize, 10_000] {
        let dir = TempDir::new(&format!("resume-{n}"));
        write_session(&dir.path().join("resume.jsonl"), "resume", n);
        let mut times = Vec::new();
        for _ in 0..5 {
            let mut agent = Agent::spawn(&[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--resume",
                "resume",
            ]);
            times.push(agent.wait_for("> ", 30));
        }
        let (m, p) = median_p95(times);
        println!(
            "radian exec --resume {n:>5} msgs to prompt: median {:.1} ms  p95 {:.1} ms",
            ms(m),
            ms(p)
        );
    }
}

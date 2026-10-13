//! Long-session measurements (Phase 5 / Phase 7): `Session::open` + `history()` and the
//! SQLite index rebuild over the synthetic sessions from [`common::long_session`].
//!
//! Measurements, not assertions: every test is `#[ignore]`d. Run:
//!
//! ```sh
//! cargo test --release -p radian-session --test perf_long_session -- --ignored --nocapture
//! ```
//!
//! The RSS of `Session::open` + `history()` is the child process's peak: the test re-runs
//! itself (`--exact perf_long_session_open`, one size via `LONGSESS_OPEN_ONE`) under macOS
//! `/usr/bin/time -l`, so one size's peak cannot hide inside another's.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::TempDir;
use common::long_session::write_long_session;
use radian_session::{Index, Session};

fn median_p95(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort();
    (v[v.len() / 2], v[(v.len() * 95 / 100).min(v.len() - 1)])
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The sizes and both variants (plain, and with 3 committed compactions).
const SIZES: [usize; 4] = [1_000, 10_000, 50_000, 100_000];
const VARIANTS: [usize; 2] = [0, 3];

/// macOS `maximum resident set size` (bytes) for running `args` with `env`.
#[cfg(target_os = "macos")]
fn peak_rss_bytes(args: &[&str], env: &[(&str, &str)]) -> (String, String, Option<u64>) {
    let mut command = Command::new("/usr/bin/time");
    command.arg("-l").args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("run /usr/bin/time");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let rss = stderr
        .lines()
        .find(|line| line.contains("maximum resident set size"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse().ok());
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
        rss,
    )
}

#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_long_session_open() {
    // Child mode: one size, measured in this fresh process so its RSS is its own peak.
    if let Ok(spec) = std::env::var("LONGSESS_OPEN_ONE") {
        let (n, compactions) = spec
            .split_once(':')
            .map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()))
            .expect("LONGSESS_OPEN_ONE is <n>:<compactions>");
        let dir = TempDir::new("perf-long-open-one");
        let path = dir.file("s.jsonl");
        let stats = write_long_session(&path, "open", n, compactions);
        let mut samples = Vec::new();
        let mut live = 0;
        for _ in 0..5 {
            let start = Instant::now();
            let session = Session::open(&path).unwrap();
            let history = session.history().unwrap();
            samples.push(start.elapsed());
            live = history.len();
        }
        let (median, p95) = median_p95(samples);
        println!(
            "LONGSESS_ONE n={n} compactions={compactions} raw={} live={} bytes={} median_ms={:.2} p95_ms={:.2}",
            stats.raw_messages,
            live,
            stats.bytes,
            ms(median),
            ms(p95)
        );
        return;
    }

    // Parent mode: one fresh process per size and variant.
    let exe = std::env::current_exe().unwrap();
    for n in SIZES {
        for compactions in VARIANTS {
            let spec = format!("{n}:{compactions}");
            let args = [
                exe.to_str().unwrap(),
                "--exact",
                "perf_long_session_open",
                "--ignored",
                "--nocapture",
            ];
            #[cfg(target_os = "macos")]
            {
                let (stdout, stderr, rss) = peak_rss_bytes(&args, &[("LONGSESS_OPEN_ONE", &spec)]);
                let line = stdout
                    .lines()
                    .find(|l| l.starts_with("LONGSESS_ONE "))
                    .unwrap_or("<no LONGSESS_ONE line>");
                let rss_mb = rss.map_or(f64::NAN, |b| b as f64 / (1024.0 * 1024.0));
                println!("{line} maxrss={rss_mb:.1} MB");
                assert!(rss.is_some(), "/usr/bin/time output:\n{stderr}");
            }
            #[cfg(not(target_os = "macos"))]
            {
                let out = Command::new(args[0])
                    .args(&args[1..])
                    .env("LONGSESS_OPEN_ONE", &spec)
                    .output()
                    .unwrap();
                let line = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find(|l| l.starts_with("LONGSESS_ONE "))
                    .unwrap_or("<no LONGSESS_ONE line>")
                    .to_string();
                println!("{line} maxrss=n/a (non-macOS)");
            }
        }
    }
}

/// SQLite index rebuild from zero over the same synthetic sessions (one file per size).
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_long_index_rebuild() {
    let dir = TempDir::new("perf-long-index");
    for n in SIZES {
        let path: std::path::PathBuf = dir.file(&format!("idx-{n}.jsonl"));
        let stats = write_long_session(&path, &format!("idx-{n}"), n, 0);
        let db = dir.file(&format!("idx-{n}.sqlite"));
        let mut index = Index::open(&db).unwrap();
        let start = Instant::now();
        let warnings = index.rebuild(std::slice::from_ref(&path)).unwrap();
        let elapsed = start.elapsed();
        assert!(warnings.is_empty(), "{warnings:?}");
        println!(
            "Index::rebuild {n:>7} messages ({} raw, {} bytes): {:.1} ms ({:.1} us/message)",
            stats.raw_messages,
            stats.bytes,
            ms(elapsed),
            elapsed.as_secs_f64() * 1e6 / stats.raw_messages as f64
        );
    }
}

/// The generator must produce files `Session::open` accepts at the sizes the report uses.
/// (The small case is covered by the generator's own unit test; this is the large one.)
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn the_synthetic_sessions_parse() {
    let dir = TempDir::new("perf-long-sanity");
    for n in SIZES {
        for compactions in VARIANTS {
            let path: std::path::PathBuf = dir.file(&format!("s-{n}-{compactions}.jsonl"));
            let stats = write_long_session(&path, "sanity", n, compactions);
            let session = Session::open(&path).unwrap();
            let history = session.history().unwrap();
            println!(
                "n={n} compactions={compactions}: raw={} live={} bytes={} parsed_history={}",
                stats.raw_messages,
                stats.live_messages,
                stats.bytes,
                history.len()
            );
            if compactions == 0 {
                assert_eq!(history.len(), stats.live_messages);
            } else {
                assert!(history.len() < stats.raw_messages, "{stats:?}");
            }
        }
    }
}

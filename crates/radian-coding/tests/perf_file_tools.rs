//! One `grep` vs three `grep`s in one batch, on this repo and on one large tree.
//!
//! All `#[ignore]`: `cargo test` never runs them; the medians are printed for
//! `benchmarks/perf-baseline.md`. The batch polls three `session.execute` futures in **one**
//! task (`tokio::join!`), the same shape as Core's `FuturesUnordered` batch: a scan
//! that never yields runs the three one after another.
//!
//! The `read` bench at the bottom is the same shape for one large-file `read` vs three
//! `read`s in one batch (the skip-to-`offset` phase scans bytes, so a deep offset makes
//! one read tens of milliseconds and the serial shape visible).

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use radian_coding::CodingTools;
use radian_core::{NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
use serde_json::json;

/// This workspace's root: two levels above this crate's manifest dir.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate dir has a workspace root two levels up")
        .to_path_buf()
}

/// The largest extracted registry source tree, if one is checked out on this machine.
fn registry_src() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))?;
    let src = home.join("registry/src");
    let mut largest: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&src).ok()? {
        let entry = entry.ok()?;
        let len: u64 = walk_len(&entry.path(), 0);
        if largest.as_ref().is_none_or(|(n, _)| len > *n) {
            largest = Some((len, entry.path()));
        }
    }
    largest.map(|(_, path)| path)
}

/// File count under `dir`, two levels deep: enough to rank the candidates.
fn walk_len(dir: &Path, depth: usize) -> u64 {
    if depth > 2 {
        return 1;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                walk_len(&entry.path(), depth + 1)
            } else {
                1
            }
        })
        .sum()
}

async fn timed_grep(
    tools: &CodingTools,
    pattern: &str,
    path: &str,
) -> (Duration, radian_core::ToolOutput) {
    let session = tools.open_session().await.expect("open session");
    let start = Instant::now();
    let out = call(&session, pattern, path).await;
    (start.elapsed(), out)
}

async fn timed_batch(
    tools: &CodingTools,
    patterns: [&str; 3],
    path: &str,
) -> (Duration, [radian_core::ToolOutput; 3]) {
    let session = tools.open_session().await.expect("open session");
    let start = Instant::now();
    // One task polls all three, like Core's batch: a blocking scan serializes them.
    let (a, b, c) = tokio::join!(
        call(&session, patterns[0], path),
        call(&session, patterns[1], path),
        call(&session, patterns[2], path),
    );
    (start.elapsed(), [a, b, c])
}

async fn call(
    session: &radian_coding::CodingSession,
    pattern: &str,
    path: &str,
) -> radian_core::ToolOutput {
    let call = ToolCall {
        id: "bench".into(),
        name: "grep".into(),
        arguments: json!({"pattern": pattern, "path": path}),
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: radian_core::CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    session
        .execute(&call, cx)
        .await
        .expect("a no-match grep never fails")
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// A pattern that matches nothing: the scan reads every file to the end, so the wall
/// clock is the walk plus the reads, not the output rendering.
fn needle(tag: char) -> String {
    format!("zzz-no-such-needle-{tag}-9f8b7a6d5c4e")
}

fn report(tree: &str, single: Duration, batch: Duration) {
    println!(
        "[perf] grep {tree}: single {single:?}, batch-of-3 {batch:?} \
         (batch/single {ratio:.2}x)",
        ratio = batch.as_secs_f64() / single.as_secs_f64().max(f64::MIN_POSITIVE),
    );
}

/// A workspace holding one large numbered file (`big.txt`, ~55 MB): a `read` with a deep
/// `offset` scans tens of megabytes in the skip phase, so one read is tens of
/// milliseconds and the serial-vs-parallel shape shows above the noise. The batch reads
/// three different deep offsets in one task, like Core's batch.
fn big_workspace() -> (common::TempDir, CodingTools) {
    let dir = common::TempDir::new("perf-read-big");
    let mut body = String::with_capacity(60 << 20);
    for n in 1..=1_000_000 {
        body.push_str(&format!("line {n:07} with some payload to fill bytes\n"));
    }
    dir.write("ws/big.txt", &body);
    let root = dir.file("ws");
    let tools = common::tools(&root);
    (dir, tools)
}

async fn read_call(
    session: &radian_coding::CodingSession,
    path: &str,
    offset: i64,
) -> radian_core::ToolOutput {
    let call = ToolCall {
        id: "bench".into(),
        name: "read".into(),
        arguments: json!({"path": path, "offset": offset, "limit": 50}),
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: radian_core::CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    session
        .execute(&call, cx)
        .await
        .expect("a bounded read never fails")
}

fn report_read(single: Duration, batch: Duration) {
    println!(
        "[perf] read big-file: single {single:?}, batch-of-3 {batch:?} \
         (batch/single {ratio:.2}x)",
        ratio = batch.as_secs_f64() / single.as_secs_f64().max(f64::MIN_POSITIVE),
    );
}

#[tokio::test]
#[ignore]
async fn perf_read_single_vs_batch_big_file() {
    let (_dir, tools) = big_workspace();
    let session = tools.open_session().await.expect("open session");
    // Warmup: page the file in before the timed samples.
    read_call(&session, "big.txt", 900_000).await;

    let mut singles = Vec::new();
    let mut batches = Vec::new();
    for round in 0..5 {
        let start = Instant::now();
        read_call(&session, "big.txt", 900_000).await;
        singles.push(start.elapsed());
        let start = Instant::now();
        // One task polls all three, like Core's batch: a blocking read serializes them.
        let _ = tokio::join!(
            read_call(&session, "big.txt", 800_000 + round),
            read_call(&session, "big.txt", 850_000 + round),
            read_call(&session, "big.txt", 900_000 + round),
        );
        batches.push(start.elapsed());
    }
    report_read(median(singles), median(batches));
}

#[tokio::test]
#[ignore]
async fn perf_grep_single_vs_batch_this_repo() {
    let root = repo_root();
    let tools = CodingTools::new(&root).expect("open the repo workspace");
    // The workspace is the tree: a relative `.` searches the whole repo.
    let path = ".";
    // Warmup: page the tree in before the timed samples.
    timed_grep(&tools, &needle('w'), path).await;

    let mut singles = Vec::new();
    let mut batches = Vec::new();
    for tag in ['a', 'b', 'c', 'd', 'e'] {
        singles.push(timed_grep(&tools, &needle(tag), path).await.0);
        batches.push(
            timed_batch(
                &tools,
                [needle('x'), needle('y'), needle('z')]
                    .each_ref()
                    .map(String::as_str),
                path,
            )
            .await
            .0,
        );
    }
    report("this-repo", median(singles), median(batches));
}

#[tokio::test]
#[ignore]
async fn perf_grep_single_vs_batch_registry() {
    let Some(tree) = registry_src() else {
        println!("[perf] grep registry: skipped (no ~/.cargo/registry/src on this machine)");
        return;
    };
    let path = tree.to_string_lossy().into_owned();
    let root = repo_root();
    // `new` resolves the read-only roots from the environment, which is what admits the
    // absolute registry path as `path`.
    let tools = CodingTools::new(&root).expect("open the repo workspace");
    timed_grep(&tools, &needle('w'), &path).await;

    let mut singles = Vec::new();
    let mut batches = Vec::new();
    for tag in ['a', 'b', 'c'] {
        singles.push(timed_grep(&tools, &needle(tag), &path).await.0);
        batches.push(
            timed_batch(
                &tools,
                [needle('x'), needle('y'), needle('z')]
                    .each_ref()
                    .map(String::as_str),
                &path,
            )
            .await
            .0,
        );
    }
    report("registry", median(singles), median(batches));
}

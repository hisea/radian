//! `grep`/`glob`/`read` overlap in one batch.
//!
//! The rendezvous uses the existing `safe_fs::resolve_hook` seam (feature `testing`): it
//! runs after the lexical check and before the first `openat`, i.e. inside the blocking
//! scan after the `spawn_blocking` change, on an executor thread before it. No new seam
//! was needed: the hook fires exactly once per search call (the base-directory open;
//! the walk itself never goes through it). A `read` of a file fires it twice (the
//! `open_dir` probe, which fails, then the `open_read`); a `read` of a directory fires
//! it once (the `open_dir` succeeds and the fd listing never goes through it). The read
//! tests size their barrier accordingly.
//!
//! The hook is process-global, so the hook tests run one at a time (the same `serial`
//! shape as `tests/safe_fs.rs`: the lock is held in sync code around a current-thread
//! runtime, never across an `.await`). Nothing here
//! asserts a wall-clock ratio: overlap is a barrier both calls must pass, and the
//! without-`spawn_blocking` shape is a hook timeout, not a hang.

mod common;

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use radian_core::{NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
use common::{TempDir, text, tools};
use serde_json::json;

/// The hook is process-global: the hook tests run one at a time.
static HOOK_LOCK: Mutex<()> = Mutex::new(());

fn serial<F: std::future::Future<Output = ()>>(body: F) {
    let _guard = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}

/// How long one hook wait may take before it gives up and fails the test instead of
/// hanging. Without `spawn_blocking` the first call parks the executor here while the
/// second call never starts, so this is the bound the brief asks for.
const HOOK_WAIT: Duration = Duration::from_secs(10);

fn grep_call(id: &str, pattern: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "grep".into(),
        arguments: json!({"pattern": pattern}),
    }
}

fn glob_call(id: &str, pattern: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "glob".into(),
        arguments: json!({"pattern": pattern}),
    }
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({"path": path}),
    }
}

/// Installs a hook that the batched calls must all pass: each of the first `expected`
/// arrivals waits (bounded) for the rest; a wait that times out is recorded. Returns the
/// timeout flag and the arrival counter the test asserts on. Only the batched calls
/// under test may run while the hook is installed.
fn rendezvous_hook(expected: usize) -> (Arc<AtomicBool>, Arc<(Mutex<usize>, Condvar)>) {
    let pair = Arc::new((Mutex::new(0usize), Condvar::new()));
    let timed_out = Arc::new(AtomicBool::new(false));
    let hook_pair = Arc::clone(&pair);
    let hook_timed_out = Arc::clone(&timed_out);
    radian_coding::safe_fs::resolve_hook(Some(Box::new(move || {
        let (count, cvar) = &*hook_pair;
        let mut arrived = count.lock().unwrap_or_else(|e| e.into_inner());
        *arrived += 1;
        if *arrived > expected {
            return;
        }
        cvar.notify_all();
        let (guard, result) = cvar
            .wait_timeout_while(arrived, HOOK_WAIT, |n| *n < expected)
            .unwrap_or_else(|e| e.into_inner());
        drop(guard);
        if result.timed_out() {
            hook_timed_out.store(true, Ordering::SeqCst);
        }
    })));
    (timed_out, pair)
}

fn clear_hook() {
    radian_coding::safe_fs::resolve_hook(None);
}

/// Two `grep` calls in one batch overlap: each blocks in the hook until the other has
/// started. `tokio::join!` polls both futures in one task, the way Core's
/// `FuturesUnordered` batch does, so without `spawn_blocking` the first call never yields
/// and the second never starts: both hook waits time out and the test fails.
#[test]
fn two_greps_in_one_batch_overlap() {
    serial(async {
        let dir = TempDir::new("parallel-two-greps");
        let root = dir.file("ws");
        dir.write("ws/a.txt", "alpha\n");
        dir.write("ws/b.txt", "beta\n");
        let tools = tools(&root);

        let (timed_out, pair) = rendezvous_hook(2);
        let session = tools.open_session().await.unwrap();
        let sink = NoopSink;
        let call_a = grep_call("a", "alpha");
        let call_b = grep_call("b", "beta");
        let cx_a = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        let cx_b = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 1,
        };
        let (a, b) = tokio::join!(
            session.execute(&call_a, cx_a),
            session.execute(&call_b, cx_b),
        );
        clear_hook();

        a.expect("grep a finds alpha");
        b.expect("grep b finds beta");
        assert_eq!(
            *pair.0.lock().unwrap_or_else(|e| e.into_inner()),
            2,
            "exactly the two batched greps reached the scan"
        );
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "both greps were in the scan at once (remove spawn_blocking and this fails)"
        );
    });
}

/// The same shape for `glob`: two batched calls must both be in the walk at once.
#[test]
fn two_globs_in_one_batch_overlap() {
    serial(async {
        let dir = TempDir::new("parallel-two-globs");
        let root = dir.file("ws");
        dir.write("ws/a.txt", "x");
        dir.write("ws/b.txt", "x");
        let tools = tools(&root);

        let (timed_out, pair) = rendezvous_hook(2);
        let session = tools.open_session().await.unwrap();
        let sink = NoopSink;
        let call_a = glob_call("a", "*.txt");
        let call_b = glob_call("b", "a.*");
        let cx_a = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        let cx_b = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 1,
        };
        let (a, b) = tokio::join!(
            session.execute(&call_a, cx_a),
            session.execute(&call_b, cx_b),
        );
        clear_hook();

        assert!(text(&a.expect("glob a")).contains("a.txt"));
        assert!(text(&b.expect("glob b")).contains("a.txt"));
        assert_eq!(
            *pair.0.lock().unwrap_or_else(|e| e.into_inner()),
            2,
            "exactly the two batched globs reached the scan"
        );
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "both globs were in the scan at once (remove spawn_blocking and this fails)"
        );
    });
}

/// Two `read` calls on files in one batch overlap: each file open fires the hook twice
/// (the `open_dir` probe, which fails because it is a file, then the `open_read`), so the
/// barrier is two arrivals — one fire from each call — and the counter assertion pins
/// four total fires. `tokio::join!` polls both futures in one task, the way Core's
/// `FuturesUnordered` batch does, so without `spawn_blocking` the first call never yields
/// and the second never starts: the hook wait times out and the test fails.
#[test]
fn two_reads_in_one_batch_overlap() {
    serial(async {
        let dir = TempDir::new("parallel-two-reads");
        let root = dir.file("ws");
        dir.write("ws/a.txt", "alpha\n");
        dir.write("ws/b.txt", "beta\n");
        let tools = tools(&root);

        let (timed_out, pair) = rendezvous_hook(2);
        let session = tools.open_session().await.unwrap();
        let sink = NoopSink;
        let call_a = read_call("a", "a.txt");
        let call_b = read_call("b", "b.txt");
        let cx_a = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        let cx_b = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 1,
        };
        let (a, b) = tokio::join!(
            session.execute(&call_a, cx_a),
            session.execute(&call_b, cx_b),
        );
        clear_hook();

        assert!(text(&a.expect("read a")).contains("alpha"));
        assert!(text(&b.expect("read b")).contains("beta"));
        assert_eq!(
            *pair.0.lock().unwrap_or_else(|e| e.into_inner()),
            4,
            "each of the two batched reads opened twice (open_dir probe + open_read)"
        );
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "both reads were in the open at once (remove spawn_blocking and this fails)"
        );
    });
}

/// The same shape for `read` on directories: each directory open fires the hook once
/// (the `open_dir` succeeds; the fd listing never goes through the hook).
#[test]
fn two_directory_reads_in_one_batch_overlap() {
    serial(async {
        let dir = TempDir::new("parallel-two-dir-reads");
        let root = dir.file("ws");
        dir.write("ws/sub1/a.txt", "alpha\n");
        dir.write("ws/sub2/b.txt", "beta\n");
        let tools = tools(&root);

        let (timed_out, pair) = rendezvous_hook(2);
        let session = tools.open_session().await.unwrap();
        let sink = NoopSink;
        let call_a = read_call("a", "sub1");
        let call_b = read_call("b", "sub2");
        let cx_a = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        let cx_b = ToolContext {
            cancellation: radian_core::CancellationToken::new(),
            events: &sink,
            index: 1,
        };
        let (a, b) = tokio::join!(
            session.execute(&call_a, cx_a),
            session.execute(&call_b, cx_b),
        );
        clear_hook();

        assert!(text(&a.expect("read sub1")).contains("a.txt"));
        assert!(text(&b.expect("read sub2")).contains("b.txt"));
        assert_eq!(
            *pair.0.lock().unwrap_or_else(|e| e.into_inner()),
            2,
            "exactly the two batched directory reads reached the open"
        );
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "both directory reads were in the open at once (remove spawn_blocking and this fails)"
        );
    });
}

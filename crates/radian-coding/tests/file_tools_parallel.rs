//! `grep`/`glob` overlap in one batch.
//!
//! The rendezvous uses the existing `safe_fs::resolve_hook` seam (feature `testing`): it
//! runs after the lexical check and before the first `openat`, i.e. inside the blocking
//! scan after the `spawn_blocking` change, on an executor thread before it. No new seam
//! was needed: the hook fires exactly once per search call (the base-directory open;
//! the walk itself never goes through it).
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

use agent_core::{NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
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

/// Installs a hook that two calls must both pass: each of the first two arrivals waits
/// (bounded) for the other; a wait that times out is recorded. Returns the timeout flag
/// and the arrival counter the test asserts on. Only two search calls may run while the
/// hook is installed.
fn rendezvous_hook() -> (Arc<AtomicBool>, Arc<(Mutex<usize>, Condvar)>) {
    let pair = Arc::new((Mutex::new(0usize), Condvar::new()));
    let timed_out = Arc::new(AtomicBool::new(false));
    let hook_pair = Arc::clone(&pair);
    let hook_timed_out = Arc::clone(&timed_out);
    radian_coding::safe_fs::resolve_hook(Some(Box::new(move || {
        let (count, cvar) = &*hook_pair;
        let mut arrived = count.lock().unwrap_or_else(|e| e.into_inner());
        *arrived += 1;
        if *arrived > 2 {
            return;
        }
        cvar.notify_all();
        let (guard, result) = cvar
            .wait_timeout_while(arrived, HOOK_WAIT, |n| *n < 2)
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

        let (timed_out, pair) = rendezvous_hook();
        let session = tools.open_session().await.unwrap();
        let sink = NoopSink;
        let call_a = grep_call("a", "alpha");
        let call_b = grep_call("b", "beta");
        let cx_a = ToolContext {
            cancellation: agent_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        let cx_b = ToolContext {
            cancellation: agent_core::CancellationToken::new(),
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

        let (timed_out, pair) = rendezvous_hook();
        let session = tools.open_session().await.unwrap();
        let sink = NoopSink;
        let call_a = glob_call("a", "*.txt");
        let call_b = glob_call("b", "a.*");
        let cx_a = ToolContext {
            cancellation: agent_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        let cx_b = ToolContext {
            cancellation: agent_core::CancellationToken::new(),
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


//! ADR 0043 decision 2 (fix round 1 B): the `subagent` parent stops the child in **two steps** —
//! SIGTERM first, so the child runs the same cleanup a Ctrl-C does, then `kill_session` as the
//! SIGKILL fallback — on both the cancel path and the drop path.
//!
//! Why SIGKILL alone is not enough: the child's `shell` tool starts every command in its own
//! **session** (`setsid`, `crates/radian-coding/src/shell.rs`), so killing the child's session
//! cannot reach them. The stub child here emulates exactly that: it starts a grandchild that
//! leaves the session with its own `setsid` (built with `perl`, as `radian-coding`'s
//! `job_session_kill` test does for `setpgid`), records the pid, and — like a real `radian exec`
//! — handles SIGTERM by killing what it started and exiting. The shape (a different session) is
//! asserted before anything is stopped, so a grandchild that never escaped proves nothing. The
//! mutation (straight `kill_session`, no SIGTERM) leaves the grandchild alive in both tests.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use radian_core::{CancellationToken, NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
use radian_host::{SubagentParts, SubagentTools};
use radian_permissions::{ApprovalPolicy, NoopAudit, Profile};
use radian_sandbox::SandboxSlot;
use serde_json::{Value, json};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-subagent-stop-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `perl`, resolved once from PATH (the same rule `radian-coding`'s `job_session_kill` uses): the
/// grandchild needs exactly one thing perl can do and a shell cannot — `setsid()` for itself. A
/// missing `perl` is a test failure, never a skip.
fn perl() -> &'static str {
    static PERL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PERL.get_or_init(|| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("perl"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                panic!("this test needs `perl` on PATH to build a child that calls `setsid`")
            })
            .display()
            .to_string()
    })
}

/// An executable stub child: it starts a grandchild in **its own session** (the way the real
/// child's `shell` tool starts every command), records both pids, and — as `radian exec` does —
/// kills the grandchild when it is asked to stop.
fn stub(dir: &Path, leader_path: &Path, pid_path: &Path) -> PathBuf {
    let path = dir.join("child.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             {} -e 'use POSIX; setsid(); exec @ARGV' sleep 600 &\n\
             grandchild=$!\n\
             echo $$ > '{leader}'\n\
             echo $grandchild > '{pid}'\n\
             trap 'kill -9 -\"$grandchild\" 2>/dev/null; exit 0' TERM\n\
             wait\n",
            perl(),
            leader = leader_path.display(),
            pid = pid_path.display(),
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn session_of(pid: u32) -> i32 {
    // SAFETY: `getsid` takes a pid and has no memory-safety preconditions.
    unsafe { libc::getsid(pid as libc::pid_t) }
}

/// Waits until the pid file holds a pid, bounded; the sleep is only the poll interval.
async fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the stub never recorded its grandchild's pid"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The grandchild is gone within the bound; the sleep is only the poll interval.
async fn gone_within(what: &str, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if alive(pid) {
        // Clean up the pid this test started and logged before failing.
        // SAFETY: signal delivery to a process this test spawned.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    assert!(!alive(pid), "{what}: {pid} was still alive");
}

/// The session and the tool session the two tests share.
struct Fixture {
    dir: PathBuf,
    /// The stub child's own pid (the leader of the session the tool kills).
    leader: PathBuf,
    /// The grandchild's pid — the process that leaves the session.
    pid: PathBuf,
    session: <SubagentTools as ToolProvider>::Session,
}

async fn fixture(label: &str) -> Fixture {
    let dir = scratch(label);
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let leader = dir.join("leader.pid");
    let pid = dir.join("grandchild.pid");
    let child = stub(&dir, &leader, &pid);
    let permissions = Arc::new(radian_permissions::PermissionHooks::new(
        Profile::workspace_write(&workspace)
            .unwrap()
            .with_approval(ApprovalPolicy::Auto),
        None,
        Arc::new(NoopAudit),
    ));
    let tools = SubagentTools::new(
        SubagentParts {
            executable: Some(child),
            ..SubagentParts::default()
        },
        dir.clone(),
        "parent",
        workspace,
        None,
        SandboxSlot::empty(),
        radian_runtime::ConfigSlot::new(Value::Null),
        permissions,
    );
    let session = tools.open_session().await.expect("open the tool session");
    Fixture {
        dir,
        leader,
        pid,
        session,
    }
}

impl Fixture {
    fn call(&self) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            arguments: json!({"task": "STOP-TASK"}),
        }
    }
}

/// The shape the test depends on: the grandchild is in a session of its own, which is what the
/// child's `shell` tool produces and what a SIGKILL of the child's session cannot reach. The stub
/// records the pid as soon as it forks and `perl` needs a moment to call `setsid`, so the shape is
/// waited for (bounded; the sleep is only the poll interval), never assumed.
async fn wait_for_escape(leader: u32, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if session_of(pid) != session_of(leader) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the grandchild {pid} never left the child's session ({}) — a grandchild that stays \
             would make this test prove nothing",
            session_of(leader)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The cancel path (ADR 0043 1a fix round 1 B): the parent turn's token fires, the tool SIGTERMs
/// the child, the child kills what it started and exits, and the call returns the `cancelled`
/// failure. The mutation (straight `kill_session`) leaves the grandchild alive — it is in its own
/// session, so the SIGKILL of the child's session never reaches it.
#[tokio::test]
async fn cancelling_the_call_stops_what_the_child_started() {
    static SINK: NoopSink = NoopSink;
    let fx = fixture("cancel").await;
    let cancellation = CancellationToken::new();
    let cx = ToolContext {
        cancellation: cancellation.clone(),
        events: &SINK,
        index: 0,
    };
    let call = fx.call();
    let mut call_future = Box::pin(fx.session.execute(&call, cx));

    let pid = tokio::select! {
        pid = wait_for_pid(&fx.pid) => pid,
        result = &mut call_future => panic!("the stub child exited early: {result:?}"),
    };
    let leader: u32 = std::fs::read_to_string(&fx.leader)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_escape(leader, pid).await;
    assert!(
        alive(leader),
        "the stub child {leader} must still be running"
    );
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(30), call_future)
        .await
        .expect("the cancelled call returned");
    let failure = result.expect_err("a cancelled call fails");
    assert_eq!(failure.code, "cancelled", "{failure:?}");

    gone_within("the cancelled child's own command", pid).await;
    let _ = std::fs::remove_dir_all(&fx.dir);
}

/// The drop path (ADR 0043 1a fix round 1 B): the `execute` future is dropped without the token
/// ever firing — the path a driver-task abort takes. The guard takes the same two steps
/// synchronously. The mutation (plain `kill_on_drop`) leaves the grandchild alive.
#[tokio::test]
async fn dropping_the_call_stops_what_the_child_started() {
    static SINK: NoopSink = NoopSink;
    let fx = fixture("drop").await;
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &SINK,
        index: 0,
    };
    let call = fx.call();
    let mut call_future = Box::pin(fx.session.execute(&call, cx));

    let pid = tokio::select! {
        pid = wait_for_pid(&fx.pid) => pid,
        result = &mut call_future => panic!("the stub child exited early: {result:?}"),
    };
    let leader: u32 = std::fs::read_to_string(&fx.leader)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_escape(leader, pid).await;

    drop(call_future);

    gone_within("the dropped child's own command", pid).await;
    let _ = std::fs::remove_dir_all(&fx.dir);
}

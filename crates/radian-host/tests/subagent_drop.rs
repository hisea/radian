//! ADR 0043 decision 1a (gap 3): dropping the call's future kills the child's **whole
//! session**, not just its leader.
//!
//! This is the SIGKILL **fallback** half of the two-step stop (fix round 1 B, the graceful half
//! is `subagent_stop`'s `dropping_the_call_stops_what_the_child_started`): the stub ignores
//! SIGTERM, so the tool's grace expires and `kill_session` takes the child's whole session. The
//! stub is a shell script that starts a long-lived grandchild in that session — the tool `setsid`s
//! the child, so the stub is the session leader — and records the grandchild's pid. The test
//! drives the tool directly and drops the `execute` future without ever cancelling the token: the
//! path a driver-task abort takes. Tokio's `kill_on_drop` signals only the leader's pid, so the
//! mutation (no fallback in the guard) leaves the grandchild alive.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::{CancellationToken, NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
use radian_host::{SubagentParts, SubagentTools};
use radian_permissions::{ApprovalPolicy, NoopAudit, Profile};
use radian_sandbox::SandboxSlot;
use serde_json::{Value, json};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-subagent-drop-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An executable stub child that **ignores SIGTERM** (the fallback's case): it starts a
/// long-lived grandchild in its own session — the tool `setsid`s the stub, so the stub is the
/// session leader and the grandchild stays in that session — records the grandchild's pid, and
/// waits.
fn stub(dir: &Path, pid_path: &Path) -> PathBuf {
    let path = dir.join("child.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ntrap '' TERM\nsleep 300 &\necho $! > '{}'\nwait\n",
            pid_path.display()
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

#[tokio::test]
async fn dropping_the_call_leaves_no_grandchild_alive() {
    let dir = scratch("guard");
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let pid_path = dir.join("grandchild.pid");
    let child = stub(&dir, &pid_path);

    static SINK: NoopSink = NoopSink;
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
    let call = ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        arguments: json!({"task": "DROP-TASK"}),
    };
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &SINK,
        index: 0,
    };
    let mut call_future = Box::pin(session.execute(&call, cx));

    // Poll the future until the grandchild's pid is on disk, then drop it. The token is never
    // cancelled, which is exactly the driver-abort path this gap is about.
    let pid = tokio::select! {
        pid = wait_for_pid(&pid_path) => pid,
        result = &mut call_future => panic!("the stub child exited: {result:?}"),
    };
    drop(call_future);

    // The guard's drop waits the grace for the (SIGTERM-ignoring) stub and then kills its whole
    // session; the poll is the bound on the grandchild going away.
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if alive(pid) {
        // Clean up the pid this test started and logged before failing.
        // SAFETY: signal delivery to a process this test spawned.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    assert!(
        !alive(pid),
        "the grandchild {pid} survived the dropped `subagent` call"
    );
}

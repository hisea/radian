//! Stopping a job (or a foreground call) must reach **every process of its session**, not only
//! its process group (dogfood bug 2026-10-17): a child that calls `setpgid` — GNU `timeout`
//! does, and so does a shell's own job control — leaves the process group but keeps the session,
//! so a group-only kill leaves it running, reparented to pid 1. The test builds that child with
//! `perl` (see [`escaping_child`]) rather than GNU `timeout`, which is coreutils, not a macOS
//! tool.
//!
//! The child is found by the pid the job itself prints (`$!`), and the shape is asserted before
//! anything is killed (different process group, same session): a child that never left the group
//! would make the test prove nothing. `session_members` is the test's own enumeration of a
//! session, written against the kernel APIs rather than against the production helper.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use agent_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::{CodingSession, CodingTools};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::{Value, json};

// ---------- helpers ----------

fn transparent_tools(ws: &Path) -> CodingTools {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full-access needs no backend");
    CodingTools::with_sandbox(ws, sandbox).expect("matching workspaces")
}

async fn exec(
    session: &CodingSession,
    name: &str,
    arguments: Value,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolFailure> {
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments,
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation,
        events: &sink,
        index: 0,
    };
    session.execute(&call, cx).await
}

fn output_text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn structured(output: &ToolOutput) -> Value {
    output.structured.clone().expect("structured output")
}

/// `perl`, resolved once from PATH. The escaping child is built with it because the test needs
/// exactly one thing: a child that calls `setpgid` for itself. GNU `timeout` does that too, but
/// `timeout` is coreutils (a homebrew install), not a macOS tool. A missing `perl` is a
/// **test failure** with this message, never a skip.
fn perl() -> &'static str {
    static PERL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PERL.get_or_init(|| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("perl"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                panic!(
                    "this test needs `perl` on PATH to build a child that calls `setpgid` (GNU \
                     `timeout` is coreutils, not a macOS tool); `perl` ships with a stock macOS, \
                     so its absence means a broken test environment"
                )
            })
            .display()
            .to_string()
    })
}

/// The escaping child: `perl` calls `setpgid(0,0)` — the same call GNU `timeout` makes — and
/// `exec`s `sleep`, so it leaves the job's process group but keeps the job's **session**.
fn escaping_child() -> String {
    format!(
        "{} -e 'use POSIX; setpgid(0,0); exec @ARGV' sleep 600",
        perl()
    )
}

/// The command under test: the leader (`$$`) starts the escaping child, writes both pids, and
/// waits for it.
fn child_leaves_the_group() -> String {
    format!(
        "{} & echo $$ > leader.pid; echo $! > child.pid; wait",
        escaping_child()
    )
}

/// The leader starts the escaping child, then blocks on the test's fifo and exits 0: on this path
/// the supervisor's **own** kill is what has to reach the child (no `job` kill and no timeout
/// fires). The fifo is what makes the shape observable: the leader is still alive when the test
/// reads the pids, and the kill runs only once the test lets it exit.
fn leader_exits_with_a_child() -> String {
    format!(
        "{} & echo $$ > leader.pid; echo $! > child.pid; cat gate > /dev/null; exit 0",
        escaping_child()
    )
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn session_of(pid: i32) -> Option<i32> {
    // SAFETY: `getsid` takes a pid and has no memory-safety preconditions.
    let sid = unsafe { libc::getsid(pid) };
    (sid >= 0).then_some(sid)
}

fn group_of(pid: i32) -> Option<i32> {
    // SAFETY: `getpgid` takes a pid and has no memory-safety preconditions.
    let pgid = unsafe { libc::getpgid(pid) };
    (pgid >= 0).then_some(pgid)
}

/// Every process in session `sid`: the kernel APIs only, no production code.
#[cfg(target_os = "macos")]
fn session_members(sid: i32) -> Vec<i32> {
    use std::mem::size_of;
    let mut out = Vec::new();
    // SAFETY: the size query writes nothing; the fill call is given the buffer and its length.
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return out;
    }
    let mut pids = vec![0 as libc::c_int; needed as usize / size_of::<libc::c_int>() + 64];
    let written = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    if written <= 0 {
        return out;
    }
    for &pid in &pids[..(written as usize).min(pids.len())] {
        if pid > 0 && session_of(pid) == Some(sid) {
            out.push(pid);
        }
    }
    out
}

/// Every process in session `sid`, from `/proc`.
#[cfg(target_os = "linux")]
fn session_members(sid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if session_of(pid) == Some(sid) {
            out.push(pid);
        }
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn session_members(_sid: i32) -> Vec<i32> {
    Vec::new()
}

/// `mkfifo(3)`: the gate a test uses to hold a command open without sleeping.
fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    // SAFETY: mkfifo takes a NUL-terminated path and a mode.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

/// Opens a fifo for writing; blocks until the command's `cat` opens it.
fn release_fifo(path: &Path) {
    use std::io::Write;
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the fifo for writing");
    fifo.write_all(b"go\n").expect("release the fifo");
}

/// Kills only the pids this test started, if a failed assertion left one behind. Never a pid
/// from the environment, never a name or a pattern.
struct Leftovers(Vec<i32>);

impl Drop for Leftovers {
    fn drop(&mut self) {
        for &pid in &self.0 {
            // SAFETY: SIGKILL to a pid this test started and read from the job's own output.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Waits until the pid file holds a complete number (the shell creates it before writing).
async fn wait_for_pid(dir: &Path, name: &str) -> i32 {
    let path = dir.join(name);
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return pid;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {name} (did the job's command start at all?)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A bounded process-state poll, never a sleep to let a race resolve.
async fn gone_within(pid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Waits, bounded, until nothing is left in session `sid`.
async fn session_empties(sid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    loop {
        let members = session_members(sid);
        if members.is_empty() {
            return true;
        }
        if start.elapsed() >= limit {
            eprintln!("the session {sid} still holds {members:?}");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads both pids the job printed and waits (bounded) for the shape: the child is in the job's
/// session but **not** in its process group. Without that, a group-only kill would suffice and
/// the test would prove nothing. The shell writes `$!` before the child runs, and the child calls
/// `setpgid` only once `perl` starts, so the shape is polled rather than sampled once.
async fn escaping_pids(root: &Path) -> (i32, i32) {
    let leader = wait_for_pid(root, "leader.pid").await;
    let child = wait_for_pid(root, "child.pid").await;
    assert!(
        alive(leader) && alive(child),
        "the job must be running (leader {leader}, child {child})"
    );
    let leader_group = group_of(leader).expect("the leader's group");
    let start = Instant::now();
    loop {
        if group_of(child).is_some_and(|child_group| child_group != leader_group) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the child (pid {child}) never left the job's process group {leader_group}, so the \
             test would be vacuous"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        session_of(child),
        session_of(leader),
        "the child (pid {child}) must still be in the job's session"
    );
    (leader, child)
}

// Starts [`child_leaves_the_group`] as a background job and returns
/// `(job_id, leader pid, child pid, guard)`.
async fn start_background(session: &CodingSession, root: &Path) -> (String, i32, i32, Leftovers) {
    let output = exec(
        session,
        "shell",
        json!({"command": child_leaves_the_group(), "background": true}),
        CancellationToken::new(),
    )
    .await
    .expect("start the job");
    let value = structured(&output);
    assert_eq!(value["background"], json!(true), "{value}");
    let id = value["job_id"].as_str().expect("a job id").to_owned();

    let (leader, child) = escaping_pids(root).await;
    (id, leader, child, Leftovers(vec![leader, child]))
}

/// Waits for the job's settle through `job` wait and returns the wait's text.
async fn settle(session: &CodingSession, id: &str) -> String {
    output_text(
        &exec(
            session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 20_000}),
            CancellationToken::new(),
        )
        .await
        .expect("the wait returns"),
    )
}

// ---------- 1. the kill op ----------

/// `job` kill must kill the whole **session**: the child that left the process group is gone
/// after the job settles, and nothing is left in the session.
#[tokio::test]
async fn job_kill_op_reaches_a_child_that_left_the_process_group() {
    let dir = TempDir::new("session-kill-kill");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (id, leader, child, _guard) = start_background(&session, &root).await;
    assert!(
        alive(child),
        "the child (pid {child}) must be running before the kill"
    );

    exec(
        &session,
        "job",
        json!({"id": id, "op": "kill"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let text = settle(&session, &id).await;
    assert!(text.contains("[status: killed]"), "{text}");

    assert!(
        gone_within(child, Duration::from_secs(10)).await,
        "the child (pid {child}) survived the kill"
    );
    assert!(
        session_empties(leader, Duration::from_secs(10)).await,
        "session {leader} still has members after the kill"
    );
}

// ---------- 2. session end ----------

/// Session end (`kill_all`) must kill every process of the job's session too.
#[tokio::test]
async fn a_session_end_cleanup_reaches_a_child_that_left_the_process_group() {
    let dir = TempDir::new("session-kill-cleanup");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (_id, leader, child, _guard) = start_background(&session, &root).await;

    session.shutdown_jobs();
    assert!(
        gone_within(child, Duration::from_secs(10)).await,
        "the child (pid {child}) survived the session-end cleanup"
    );
    assert!(
        session_empties(leader, Duration::from_secs(10)).await,
        "session {leader} still has members after the session-end cleanup"
    );
}

// ---------- 4. the leader exits on its own ----------

/// The supervisor's own kill (the path a `job` kill and the timeout do not take) must reach the
/// child that left the process group: the job still prints `exited 0`, and the child is gone.
#[tokio::test]
async fn a_natural_exit_reaches_a_child_that_left_the_process_group() {
    let dir = TempDir::new("session-kill-exit");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    mkfifo(&root.join("gate"));
    let output = exec(
        &session,
        "shell",
        json!({"command": leader_exits_with_a_child(), "background": true}),
        CancellationToken::new(),
    )
    .await
    .expect("start the job");
    let value = structured(&output);
    let id = value["job_id"].as_str().expect("a job id").to_owned();

    // The leader is blocked on the fifo, so the shape is observable before the kill runs.
    let (leader, child) = escaping_pids(&root).await;
    let _guard = Leftovers(vec![leader, child]);

    release_fifo(&root.join("gate"));
    let text = settle(&session, &id).await;
    assert!(text.contains("[status: exited 0]"), "{text}");
    assert!(
        gone_within(child, Duration::from_secs(10)).await,
        "the child (pid {child}) survived the leader's natural exit"
    );
    assert!(
        session_empties(leader, Duration::from_secs(10)).await,
        "session {leader} still has members after the leader's natural exit"
    );
}

// ---------- 5. a foreground timeout ----------

/// A foreground `shell` call that times out kills the whole session: the escaped child is gone
/// when the call returns, and nothing is left in the session.
#[tokio::test]
async fn a_foreground_timeout_reaches_a_child_that_left_the_process_group() {
    let dir = TempDir::new("session-kill-foreground");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let call = exec(
        &session,
        "shell",
        json!({"command": child_leaves_the_group(), "timeout_secs": 3}),
        CancellationToken::new(),
    );
    let pids = escaping_pids(&root);
    let (result, (leader, child)) = tokio::join!(call, pids);
    let guard = Leftovers(vec![leader, child]);
    let output = result.expect("the call returns");
    let value = structured(&output);
    assert_eq!(value["timed_out"], json!(true), "{value}");

    assert!(
        gone_within(child, Duration::from_secs(10)).await,
        "the child (pid {child}) survived the foreground timeout"
    );
    assert!(
        session_empties(leader, Duration::from_secs(10)).await,
        "session {leader} still has members after the foreground timeout"
    );
    drop(guard);
}

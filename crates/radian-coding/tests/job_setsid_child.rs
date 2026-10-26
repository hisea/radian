//! A stop must also reach a descendant that called `setsid` and so left the job's **session**
//! (ADR 0037 §7). The real case is `apt-get install`: apt runs `dpkg` in its own pty session, so
//! a session-scoped kill leaves `dpkg` holding `/var/lib/dpkg/lock` and every later `apt` call
//! fails. A stop path collects the leader's descendant tree across sessions when it first
//! signals, and the grace paths keep that set for their SIGKILL pass.
//!
//! The `setsid` child is built with `perl` (see [`setsid_child`]) because macOS has no `setsid`
//! command. The child is found by the pid the command itself writes, and the shape is asserted
//! before the stop (its session differs from the job's): a child that never left the session
//! would make the test prove nothing.
//!
//! The last test documents the scope: a process that double-forked away before the stop is a
//! child of pid 1 and no tree walk finds it.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::TempDir;
use radian_coding::{CodingSession, CodingTools};
use radian_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
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

/// `perl`, resolved once from PATH. A missing `perl` is a **test failure** with this message,
/// never a skip (`perl` ships with a stock macOS).
fn perl() -> &'static str {
    static PERL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PERL.get_or_init(|| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("perl"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                panic!(
                    "this test needs `perl` on PATH to build a child that calls `setsid` (macOS \
                     has no `setsid` command); `perl` ships with a stock macOS, so its absence \
                     means a broken test environment"
                )
            })
            .display()
            .to_string()
    })
}

/// The escaping child: `perl` calls `setsid(2)`, so it leaves the job's session as well as its
/// process group, and `exec`s `sleep`, which ends on SIGTERM.
fn setsid_child() -> String {
    format!(
        "{} -MPOSIX -e 'POSIX::setsid(); exec @ARGV' sleep 600",
        perl()
    )
}

/// The child that only SIGKILL ends: it leaves the job's session, then ignores SIGTERM.
fn ignoring_setsid_child() -> String {
    format!(
        "{} -MPOSIX -e 'POSIX::setsid(); $SIG{{TERM}} = \"IGNORE\"; sleep 600'",
        perl()
    )
}

/// The command under test: the leader (`$$`) starts the escaping child, writes both pids, and
/// waits for it.
fn child_leaves_the_session(child: &str) -> String {
    format!(
        "{} & echo $$ > leader.pid; echo $! > child.pid; wait",
        child
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

/// The parent pid, from the kernel APIs only (the test's own check, not the production helper).
#[cfg(target_os = "macos")]
fn parent_of(pid: i32) -> Option<i32> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is zeroed plain data that `proc_pidinfo` fills in, handed its own size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some(info.pbi_ppid as i32)
}

#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the last `)` the fields are `state ppid pgrp session`; `ppid` is the second.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_of(_pid: i32) -> Option<i32> {
    None
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

/// `true` only when `pid` was still alive on every check: a bounded observation, so a signal
/// that arrives late cannot pass for "not signalled".
async fn alive_for(pid: i32, span: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < span {
        if !alive(pid) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    alive(pid)
}

/// Reads both pids the job printed and waits (bounded) for the shape: the child is in a
/// **session of its own**, not the job's. Without that, the session signal alone would reach it
/// and the test would prove nothing.
async fn escaping_pids(root: &Path) -> (i32, i32) {
    let leader = wait_for_pid(root, "leader.pid").await;
    let child = wait_for_pid(root, "child.pid").await;
    assert!(
        alive(leader) && alive(child),
        "the job must be running (leader {leader}, child {child})"
    );
    let leader_session = session_of(leader).expect("the leader's session");
    let start = Instant::now();
    loop {
        if session_of(child) != Some(leader_session) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the child (pid {child}) never left the job's session {leader_session}, so the test \
             would be vacuous"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (leader, child)
}

/// Starts [`child_leaves_the_session`] as a background job and returns
/// `(job_id, leader pid, child pid, guard)`.
async fn start_background(
    child: &str,
    session: &CodingSession,
    root: &Path,
) -> (String, i32, i32, Leftovers) {
    let output = exec(
        session,
        "shell",
        json!({"command": child_leaves_the_session(child), "background": true}),
        CancellationToken::new(),
    )
    .await
    .expect("start the job");
    let value = structured(&output);
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

// ---------- 1. a foreground timeout ----------

/// A foreground `shell` call that times out reaches a child that started its own session: no
/// part of the job's session holds it, so only the descendant walk can signal it.
#[tokio::test]
async fn a_foreground_timeout_reaches_a_child_that_started_its_own_session() {
    let dir = TempDir::new("setsid-timeout");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let call = exec(
        &session,
        "shell",
        json!({"command": child_leaves_the_session(&setsid_child()), "timeout_secs": 3}),
        CancellationToken::new(),
    );
    let pids = escaping_pids(&root);
    let (result, (leader, child)) = tokio::join!(call, pids);
    let _guard = Leftovers(vec![leader, child]);
    let value = structured(&result.expect("the call returns"));
    assert_eq!(value["timed_out"], json!(true), "{value}");

    assert!(
        gone_within(child, Duration::from_secs(10)).await,
        "the child (pid {child}) survived the foreground timeout"
    );
}

// ---------- 2. the collected set on the SIGKILL pass ----------

/// The child ignores SIGTERM and its parent (the job leader) dies on it. By the SIGKILL pass the
/// child is a child of pid 1, so a fresh walk from the roots finds nothing: only the set
/// collected at the SIGTERM moment reaches it.
#[tokio::test]
async fn a_foreground_timeout_reaches_an_ignoring_child_after_its_parent_died() {
    let dir = TempDir::new("setsid-timeout-ignore");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let call = exec(
        &session,
        "shell",
        json!({"command": child_leaves_the_session(&ignoring_setsid_child()), "timeout_secs": 3}),
        CancellationToken::new(),
    );
    let pids = escaping_pids(&root);
    let (result, (leader, child)) = tokio::join!(call, pids);
    let _guard = Leftovers(vec![leader, child]);
    let value = structured(&result.expect("the call returns"));
    assert_eq!(value["timed_out"], json!(true), "{value}");

    assert!(
        gone_within(child, Duration::from_secs(15)).await,
        "the child (pid {child}) outlived the SIGKILL pass: SIGTERM cannot end it, and a fresh \
         walk cannot find it once its parent is gone"
    );
}

// ---------- 3. `job kill` ----------

/// `job kill` sends SIGTERM and then SIGKILL after the grace; the SIGKILL reaches the set the
/// SIGTERM collected, so the child that ignores SIGTERM still ends.
#[tokio::test]
async fn job_kill_reaches_a_child_that_started_its_own_session() {
    let dir = TempDir::new("setsid-job-kill");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (id, _leader, child, _guard) =
        start_background(&ignoring_setsid_child(), &session, &root).await;

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
        gone_within(child, Duration::from_secs(15)).await,
        "the child (pid {child}) survived `job kill`"
    );
}

// ---------- 4. `/jobs kill all` ----------

/// The cleanup a session exit, the panic path and `/jobs kill all` share signals SIGKILL alone;
/// it collects the tree once and so reaches a child in a session of its own.
#[tokio::test]
async fn jobs_kill_all_reaches_a_child_that_started_its_own_session() {
    let dir = TempDir::new("setsid-kill-all");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (_id, _leader, child, _guard) = start_background(&setsid_child(), &session, &root).await;

    // The same `Jobs::kill_all` the `/jobs kill all` command and the session-exit cleanup run.
    session.shutdown_jobs();
    assert!(
        gone_within(child, Duration::from_secs(10)).await,
        "the child (pid {child}) survived the kill-all cleanup"
    );
}

// ---------- 5. the scope ----------

/// A process that double-forked away before the stop is a child of pid 1 by then, in no part of
/// the job's tree: no walk finds it, and the stop leaves it alone (ADR 0037 §7's out-of-scope
/// rule). It is the daemon case, and this test is what pins it.
#[tokio::test]
async fn a_process_that_double_forked_before_the_stop_is_not_signalled() {
    let dir = TempDir::new("setsid-double-fork");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    // The leader starts a `perl` that forks the grandchild, exits, and leaves the grandchild in a
    // session of its own writing its pid; the leader then sleeps so the job stays running.
    let command = format!(
        "{} -MPOSIX -e 'my $pid = fork(); exit 0 if $pid; POSIX::setsid(); open my $fh, \">\", \
         $ARGV[0] or exit 1; print $fh \"$$\\n\"; close $fh; sleep 600' grandchild.pid & echo $$ \
         > leader.pid; sleep 600",
        perl()
    );
    let output = exec(
        &session,
        "shell",
        json!({"command": command, "background": true}),
        CancellationToken::new(),
    )
    .await
    .expect("start the job");
    let id = structured(&output)["job_id"]
        .as_str()
        .expect("a job id")
        .to_owned();

    let leader = wait_for_pid(&root, "leader.pid").await;
    let grandchild = wait_for_pid(&root, "grandchild.pid").await;
    let _guard = Leftovers(vec![leader, grandchild]);
    assert!(
        alive(grandchild),
        "the grandchild (pid {grandchild}) must be running before the stop"
    );
    assert_ne!(
        session_of(grandchild),
        session_of(leader),
        "the grandchild (pid {grandchild}) must be in a session of its own"
    );
    let start = Instant::now();
    while parent_of(grandchild) == Some(leader) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the grandchild (pid {grandchild}) never left the leader's tree, so the test would \
             not document the scope"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

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

    // The stop's SIGKILL pass runs a grace after the SIGTERM, so the observation spans it.
    assert!(
        alive_for(grandchild, Duration::from_secs(5)).await,
        "a process that double-forked before the stop must not be signalled (pid {grandchild})"
    );
}

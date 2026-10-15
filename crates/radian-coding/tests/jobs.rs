//! Background jobs (ADR 0037 slices 1-2): the `shell` tool's `background: true` and the
//! `job_output` / `job_list` / `job_kill` tools.
//!
//! These use a `FullAccess` sandbox (no backend needed), the same workhorse the shell tests
//! use for the tool's own behaviour. The registry is per session, so each test opens **one**
//! session and reuses it across calls.

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
) -> Result<ToolOutput, ToolFailure> {
    exec_cancel(session, name, arguments, CancellationToken::new()).await
}

async fn exec_cancel(
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

/// Starts a background job and returns `(job_id, workspace-relative output)`.
async fn start(
    session: &CodingSession,
    command: &str,
    timeout_secs: Option<u64>,
) -> (String, String) {
    let mut args = json!({"command": command, "background": true});
    if let Some(secs) = timeout_secs {
        args["timeout_secs"] = json!(secs);
    }
    let output = exec(session, "shell", args).await.expect("start the job");
    let value = structured(&output);
    assert_eq!(value["background"], json!(true), "{value}");
    (
        value["job_id"].as_str().expect("a job id").to_owned(),
        value["output"].as_str().expect("an output path").to_owned(),
    )
}

/// Waits until `predicate` holds, bounded. The sleep is the poll interval, never the
/// synchronization itself.
async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_file(path: &Path, needle: &str) {
    wait_until(&format!("{needle:?} in {}", path.display()), || {
        std::fs::read_to_string(path)
            .map(|text| text.contains(needle))
            .unwrap_or(false)
    })
    .await;
}

/// Waits until the pid file holds a complete number. Waiting on existence alone races a
/// half-written file (the shell creates it before writing the number).
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
            "timed out: {name}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group has a member.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

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

/// `mkfifo(3)`: the gate a test uses to hold a command open without sleeping.
fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    // SAFETY: mkfifo takes a NUL-terminated path and a mode.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

/// Opens a fifo for writing; blocks until a reader (the command's `cat`) opens it.
fn release_fifo(path: &Path) {
    use std::io::Write;
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the fifo for writing");
    fifo.write_all(b"go\n").expect("release the fifo");
}

/// The leader writes its pid, then a background child writes its pid, then both sleep.
const TWO_SLEEPS: &str = "echo $$ > leader.pid; sleep 600 & echo $! > bg.pid; wait";

// ---------- 1. start, output, increments ----------

/// `background: true` returns at once while the command is still blocked on a fifo the test
/// holds; the output file fills; `job_output` returns only the increment since the last read.
#[tokio::test]
async fn a_background_job_returns_at_once_and_job_output_returns_increments() {
    let dir = TempDir::new("jobs-start");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    mkfifo(&root.join("gate"));
    let start_time = Instant::now();
    let (id, output) = start(
        &session,
        "echo first; cat gate > /dev/null; echo second",
        None,
    )
    .await;
    assert!(
        start_time.elapsed() < Duration::from_secs(5),
        "start must not wait for the command: {:?}",
        start_time.elapsed()
    );
    let output_path = root.join(&output);

    // The command is blocked on the fifo; "first" is already in the file and the ring.
    wait_for_file(&output_path, "first").await;
    let first = output_text(
        &exec(&session, "job_output", json!({"job_id": id}))
            .await
            .unwrap(),
    );
    assert!(first.contains("first"), "{first}");
    assert!(first.contains("[status: running]"), "{first}");
    assert!(!first.contains("second"), "not released yet: {first}");

    // A second read with nothing new says so: the cursor advanced.
    let again = output_text(
        &exec(&session, "job_output", json!({"job_id": id}))
            .await
            .unwrap(),
    );
    assert!(again.contains("[no new output]"), "{again}");
    assert!(
        !again.contains("first"),
        "the increment must not repeat: {again}"
    );

    // Release the command; `wait: true` returns the final increment and the settle.
    release_fifo(&root.join("gate"));
    let done = output_text(
        &exec(
            &session,
            "job_output",
            json!({"job_id": id, "wait": true, "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(done.contains("second"), "{done}");
    assert!(done.contains("[status: exited 0]"), "{done}");
    assert!(!done.contains("first"), "the increment only: {done}");

    session.shutdown_jobs();
}

// ---------- 2. wait timeout ----------

/// `wait` with a short timeout returns `[status: running]` and does **not** kill the job.
#[tokio::test]
async fn a_wait_timeout_returns_running_and_leaves_the_job_alive() {
    let dir = TempDir::new("jobs-wait");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (id, _) = start(&session, "echo $$ > leader.pid; sleep 600", None).await;
    let leader = wait_for_pid(&root, "leader.pid").await;

    let text = output_text(
        &exec(
            &session,
            "job_output",
            json!({"job_id": id, "wait": true, "timeout_ms": 300}),
        )
        .await
        .unwrap(),
    );
    assert!(text.contains("[status: running]"), "{text}");
    assert!(text.contains("the job was not killed"), "{text}");
    assert!(
        alive(leader),
        "a wait timeout must not kill the job (pid {leader})"
    );

    session.shutdown_jobs();
    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "shutdown must kill the job"
    );
}

// ---------- 3. kill and timeout kill the whole group ----------

/// `job_kill` SIGTERMs the whole group (then SIGKILLs): neither the shell nor its background
/// child survives, and the group is empty.
#[tokio::test]
async fn job_kill_kills_the_whole_process_group() {
    let dir = TempDir::new("jobs-kill");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (id, _) = start(&session, TWO_SLEEPS, None).await;
    let leader = wait_for_pid(&root, "leader.pid").await;
    let background = wait_for_pid(&root, "bg.pid").await;
    assert_ne!(leader, background, "a separate child pid");

    let killed = output_text(
        &exec(&session, "job_kill", json!({"job_id": id}))
            .await
            .unwrap(),
    );
    assert!(killed.contains("stopping"), "{killed}");

    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "the shell (pid {leader}) survived job_kill"
    );
    assert!(
        gone_within(background, Duration::from_secs(10)).await,
        "the child (pid {background}) survived job_kill"
    );
    assert!(!group_alive(leader), "group {leader} still has members");

    // The job settles as killed, and a second kill is a no-op that says so.
    let done = output_text(
        &exec(
            &session,
            "job_output",
            json!({"job_id": id, "wait": true, "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(done.contains("[status: killed]"), "{done}");
    let again = output_text(
        &exec(&session, "job_kill", json!({"job_id": id}))
            .await
            .unwrap(),
    );
    assert!(again.contains("already finished"), "{again}");
}

/// `timeout_secs` on a background job kills the whole group and settles as timed out.
#[tokio::test]
async fn a_background_timeout_kills_the_whole_process_group() {
    let dir = TempDir::new("jobs-timeout");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    // Two seconds: the script writes both pids before the timeout can fire.
    let (id, _) = start(&session, TWO_SLEEPS, Some(2)).await;
    let leader = wait_for_pid(&root, "leader.pid").await;
    let background = wait_for_pid(&root, "bg.pid").await;

    let text = output_text(
        &exec(
            &session,
            "job_output",
            json!({"job_id": id, "wait": true, "timeout_ms": 15_000}),
        )
        .await
        .unwrap(),
    );
    assert!(text.contains("[status: timed out]"), "{text}");
    assert!(
        gone_within(background, Duration::from_secs(10)).await,
        "the child (pid {background}) survived the timeout"
    );
    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "the shell (pid {leader}) survived the timeout"
    );
    assert!(!group_alive(leader), "group {leader} still has members");
}

// ---------- 4. the concurrency limit ----------

/// The fifth running job is refused with the limit and the three tools named; after one
/// settles a new one starts.
#[tokio::test]
async fn the_fifth_running_job_is_refused() {
    let dir = TempDir::new("jobs-limit");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let mut ids = Vec::new();
    for _ in 0..4 {
        let (id, _) = start(&session, "sleep 600", None).await;
        ids.push(id);
    }
    let failure = exec(
        &session,
        "shell",
        json!({"command": "sleep 600", "background": true}),
    )
    .await
    .expect_err("the fifth job must be refused");
    assert_eq!(failure.code, "too_many_jobs");
    assert!(
        failure.message.contains("job_output"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("job_kill"), "{}", failure.message);
    assert!(failure.message.contains("job_list"), "{}", failure.message);

    // Settle one, then the next start succeeds.
    exec(&session, "job_kill", json!({"job_id": ids[0]}))
        .await
        .unwrap();
    exec(
        &session,
        "job_output",
        json!({"job_id": ids[0], "wait": true, "timeout_ms": 10_000}),
    )
    .await
    .unwrap();
    let (id, _) = start(&session, "sleep 600", None).await;
    assert!(!id.is_empty());

    session.shutdown_jobs();
}

// ---------- 5. turn cancellation does not kill jobs ----------

/// Cancelling the turn ends a `job_output` wait but leaves the job running (ADR 0037
/// decision 6).
#[tokio::test]
async fn a_cancelled_turn_leaves_the_job_running() {
    let dir = TempDir::new("jobs-cancel");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (id, _) = start(&session, "echo $$ > leader.pid; sleep 600", None).await;
    let leader = wait_for_pid(&root, "leader.pid").await;

    let token = CancellationToken::new();
    let wait = exec_cancel(
        &session,
        "job_output",
        json!({"job_id": id, "wait": true, "timeout_ms": 60_000}),
        token.clone(),
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(wait, cancel);
    let text = output_text(&result.unwrap());
    assert!(text.contains("the turn was cancelled"), "{text}");
    assert!(
        alive(leader),
        "a cancelled turn must not kill the job (pid {leader})"
    );

    session.shutdown_jobs();
}

// ---------- 6. session-end cleanup ----------

/// Session end kills every running job's group.
#[tokio::test]
async fn session_end_cleanup_kills_every_running_group() {
    let dir = TempDir::new("jobs-shutdown");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (_, _) = start(&session, "echo $$ > a.pid; sleep 600", None).await;
    let (_, _) = start(&session, "echo $$ > b.pid; sleep 600", None).await;
    let a = wait_for_pid(&root, "a.pid").await;
    let b = wait_for_pid(&root, "b.pid").await;

    session.shutdown_jobs();
    assert!(
        gone_within(a, Duration::from_secs(10)).await,
        "job a (pid {a}) survived the session-end cleanup"
    );
    assert!(
        gone_within(b, Duration::from_secs(10)).await,
        "job b (pid {b}) survived the session-end cleanup"
    );
    assert!(!group_alive(a), "group {a} still has members");
    assert!(!group_alive(b), "group {b} still has members");
}

// ---------- 7. job_list ----------

/// `job_list` reports each job's id, state, command and output path.
#[tokio::test]
async fn job_list_reports_the_states() {
    let dir = TempDir::new("jobs-list");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (done_id, done_output) = start(&session, "echo done", None).await;
    let done = output_text(
        &exec(
            &session,
            "job_output",
            json!({"job_id": done_id, "wait": true, "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(done.contains("[status: exited 0]"), "{done}");
    let (run_id, run_output) = start(&session, "sleep 600", None).await;

    let list = output_text(&exec(&session, "job_list", json!({})).await.unwrap());
    assert!(list.contains(&done_id), "{list}");
    assert!(list.contains("exited 0"), "{list}");
    assert!(list.contains(&done_output), "{list}");
    assert!(list.contains(&run_id), "{list}");
    assert!(list.contains("running"), "{list}");
    assert!(list.contains(&run_output), "{list}");

    let value = structured(&exec(&session, "job_list", json!({})).await.unwrap());
    assert_eq!(value["count"], json!(2), "{value}");

    session.shutdown_jobs();
}

// ---------- 8. unknown job ----------

#[tokio::test]
async fn an_unknown_job_is_refused() {
    let dir = TempDir::new("jobs-unknown");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let failure = exec(&session, "job_output", json!({"job_id": "job-0-0"}))
        .await
        .expect_err("unknown job");
    assert_eq!(failure.code, "unknown_job");
    let failure = exec(&session, "job_kill", json!({"job_id": "job-0-0"}))
        .await
        .expect_err("unknown job");
    assert_eq!(failure.code, "unknown_job");
}

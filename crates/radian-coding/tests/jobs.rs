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
use radian_coding::{CodingSession, CodingTools, JobEvent, JobState};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy, SandboxSlot};
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

/// Starts a background job and returns `(job_id, the output hint)`.
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
        value["output"]
            .as_str()
            .expect("the output hint")
            .to_owned(),
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
    let (id, _) = start(
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
    let output_path = session.jobs().output_path(&id);

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

// ---------- 2b. a waited settle is not notified ----------

/// A settle a `job_output wait=true` is blocked on is the model's already, so the registry
/// marks it `notify: false` (ADR 0037 decision 4). The waiter count is what carries that.
#[tokio::test]
async fn a_waited_settle_is_not_notified() {
    let dir = TempDir::new("jobs-waited");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");
    let mut events = session.jobs().subscribe();

    // The job ends on its own shortly, while the wait is blocked.
    let (id, _) = start(&session, "sleep 0.3; echo done", None).await;
    let wait = exec(
        &session,
        "job_output",
        json!({"job_id": id, "wait": true, "timeout_ms": 60_000}),
    );
    let settle = async {
        loop {
            match events.recv().await {
                Ok(JobEvent::Settled { notify, .. }) => return notify,
                Ok(JobEvent::Started { .. }) => continue,
                Err(error) => panic!("no settle event: {error}"),
            }
        }
    };
    let (result, notify) = tokio::join!(wait, settle);
    let text = output_text(&result.expect("the wait returns"));
    assert!(text.contains("[status: exited 0]"), "{text}");
    assert!(
        !notify,
        "a settle a wait already returned must not be notified again"
    );
}

// ---------- 2c. the notify flag per settle kind ----------

/// The `notify` flag of the next settle on `events` (skipping the start), bounded.
async fn next_settle_notify(events: &mut tokio::sync::broadcast::Receiver<JobEvent>) -> bool {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
            Ok(Ok(JobEvent::Settled { notify, .. })) => return notify,
            Ok(Ok(JobEvent::Started { .. })) => continue,
            Ok(Err(error)) => panic!("no settle event: {error}"),
            Err(_) => panic!("timed out waiting for a settle event"),
        }
    }
}

/// The registry computes `notify` itself, so each settle kind is pinned here rather than in a
/// host test that builds the flag by hand (ADR 0037 decisions 2, 4 and 6).
#[tokio::test]
async fn the_notify_flag_follows_the_settle_kind() {
    // A natural exit notifies.
    let dir = TempDir::new("jobs-notify-natural");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");
    let mut events = session.jobs().subscribe();
    let (_id, _) = start(&session, "echo done", None).await;
    assert!(
        next_settle_notify(&mut events).await,
        "a natural exit notifies"
    );

    // `job_kill` does not.
    let mut events = session.jobs().subscribe();
    let (id, _) = start(&session, "echo $$ > k.pid; sleep 600", None).await;
    wait_for_pid(&root, "k.pid").await;
    exec(&session, "job_kill", json!({"job_id": id}))
        .await
        .unwrap();
    assert!(
        !next_settle_notify(&mut events).await,
        "job_kill must not notify"
    );

    // A timeout notifies (decision a: the model set it but did not ask to kill the job).
    let mut events = session.jobs().subscribe();
    let (_id, _) = start(&session, "echo $$ > t.pid; sleep 600", Some(1)).await;
    assert!(next_settle_notify(&mut events).await, "a timeout notifies");
}

/// A session-end cleanup is a kill: the settle reads `killed` and `notify: false`
/// (ADR 0037 decision 4). Without `kill_all` marking the termination the settle would be
/// `signal 9` and notify.
#[tokio::test]
async fn a_session_cleanup_settle_is_not_notified() {
    let dir = TempDir::new("jobs-notify-cleanup");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");
    let mut events = session.jobs().subscribe();
    let (_id, _) = start(&session, "echo $$ > s.pid; sleep 600", None).await;
    wait_for_pid(&root, "s.pid").await;
    session.shutdown_jobs();

    loop {
        match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
            Ok(Ok(JobEvent::Settled { state, notify, .. })) => {
                assert_eq!(state.label(), "killed", "a cleanup settles as killed");
                assert!(!notify, "a session-exit settle must not notify");
                break;
            }
            Ok(Ok(JobEvent::Started { .. })) => continue,
            Ok(Err(error)) => panic!("no settle event: {error}"),
            Err(_) => panic!("timed out waiting for a settle event"),
        }
    }
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
    // A wrong kill would settle well within this window; the job must outlive it. This is a
    // bounded process-state wait, not a sleep to let a race resolve.
    assert!(
        !gone_within(leader, Duration::from_secs(1)).await,
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

// ---------- 3b. a natural exit kills the group too ----------

/// A command that exits on its own but leaves a background grandchild: the supervisor kills
/// the whole process group when the leader exits, so the grandchild does not outlive it. This
/// is the group-cleanup path that `job_kill` and the timeout do **not** exercise (they signal
/// the group themselves).
#[tokio::test]
async fn a_natural_exit_kills_a_background_grandchild() {
    let dir = TempDir::new("jobs-grandchild");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    // The leader backgrounds a long sleep (same process group) and exits 0 at once.
    let (id, _) = start(
        &session,
        "sleep 300 & echo $! > grandchild.pid; exit 0",
        None,
    )
    .await;
    let text = output_text(
        &exec(
            &session,
            "job_output",
            json!({"job_id": id, "wait": true, "timeout_ms": 15_000}),
        )
        .await
        .unwrap(),
    );
    assert!(text.contains("[status: exited 0]"), "{text}");

    let grandchild = wait_for_pid(&root, "grandchild.pid").await;
    assert!(
        gone_within(grandchild, Duration::from_secs(10)).await,
        "the grandchild (pid {grandchild}) survived the leader's exit"
    );
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
        !gone_within(leader, Duration::from_secs(1)).await,
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

/// Dropping the registry (the panic-path net, ADR 0037 decision 6) kills every running group.
/// The pty test `pty_panic_path_does_not_orphan_a_background_job` covers the whole app; this
/// pins the mechanism directly.
#[tokio::test]
async fn dropping_the_registry_kills_every_running_group() {
    let dir = TempDir::new("jobs-drop");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (_, _) = start(&session, "echo $$ > drop.pid; sleep 600", None).await;
    let pid = wait_for_pid(&root, "drop.pid").await;

    drop(session);
    assert!(
        gone_within(pid, Duration::from_secs(10)).await,
        "the job (pid {pid}) survived the registry drop"
    );
    assert!(!group_alive(pid), "group {pid} still has members");
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
    assert!(list.contains(&run_id), "{list}");
    assert!(list.contains("running"), "{list}");
    // The list names `job_output` where the output path used to be (ADR 0039 decision 1).
    assert_eq!(done_output, radian_coding::OUTPUT_HINT, "{done_output}");
    assert_eq!(run_output, radian_coding::OUTPUT_HINT, "{run_output}");
    assert!(list.contains(radian_coding::OUTPUT_HINT), "{list}");

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

// ---------- 9. a background job keeps the sandbox it started with (ADR 0038 slice 2) ----------

/// The sandbox is read from the shared slot when a call starts, so a job is spawned with the
/// sandbox in force at that moment. Swapping the slot afterwards cannot reach the running job:
/// a job started confined stays confined, and its outside write never lands.
#[tokio::test]
async fn a_running_background_job_keeps_the_sandbox_it_started_with() {
    let dir = TempDir::new("jobs-slot");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = common::sentinel_outside(&dir);
    let marker = outside.join("job.txt");

    let policy = match SandboxPolicy::new(SandboxMode::WorkspaceWrite, &root) {
        Ok(policy) => policy,
        Err(_) => return,
    };
    let confined = match Sandbox::from_policy(policy) {
        Ok(sandbox) => sandbox,
        Err(e) => {
            eprintln!("skipping: no working workspace-write backend: {e}");
            return;
        }
    };
    let slot = SandboxSlot::with(confined);
    let tools = CodingTools::with_sandbox_slot(&root, slot.clone()).expect("matching workspaces");
    let session = tools.open_session().await.expect("session");

    // Start the job confined. It writes the marker outside the workspace, which the confined
    // sandbox denies.
    let (job_id, _) = start(&session, &format!("echo job > {}", marker.display()), None).await;

    // Swap the slot to `full` while the job is (or may still be) running.
    let full = Sandbox::from_policy(
        SandboxPolicy::new(SandboxMode::FullAccess, &root).expect("full-access policy"),
    )
    .expect("full-access needs no backend");
    slot.set(Some(full));

    // Wait for the job to settle, then assert the outside write never landed: the job kept the
    // sandbox it started with.
    wait_until("the job settled", || {
        session
            .jobs()
            .list()
            .iter()
            .all(|job| !matches!(job.state, JobState::Running))
    })
    .await;
    assert!(
        !marker.exists(),
        "the running job's confined sandbox must have denied the outside write (job {job_id})"
    );
}

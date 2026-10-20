//! Background jobs (ADR 0037 slices 1-2): the `shell` tool's `background: true` and the
//! `job { op: read | wait | kill }`, the one background tool (ADR 0037 decision 4).
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
use radian_coding::{CodingSession, CodingTools, JobEvent, JobState, Limits};
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

/// Starts a background job and returns `(job_id, the output hint)`. `timeout_secs` is written
/// into the arguments verbatim: the tool still **accepts** the field, but a background job has
/// no deadline (ADR 0037), so the value is passed through to be ignored. Callers that
/// want a deadline assert on `job`'s state instead.
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
/// holds; the output file fills; `job` returns only the increment since the last read.
#[tokio::test]
async fn a_background_job_returns_at_once_and_job_returns_increments() {
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
        &exec(&session, "job", json!({"id": id, "op": "read"}))
            .await
            .unwrap(),
    );
    assert!(first.contains("first"), "{first}");
    assert!(first.contains("[status: running]"), "{first}");
    assert!(!first.contains("second"), "not released yet: {first}");

    // A second read with nothing new says so: the cursor advanced.
    let again = output_text(
        &exec(&session, "job", json!({"id": id, "op": "read"}))
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
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 10_000}),
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

/// A settle a `job` wait is blocked on is the model's already, so the registry
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
        "job",
        json!({"id": id, "op": "wait", "timeout_ms": 60_000}),
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

// ---------- 2d. a job that settled while nobody waited ----------

/// The state of the next settle on `events` (skipping the start), bounded. Waiting on the
/// registry's **own** notice, not on a sleep, is what makes the check below deterministic: when
/// it returns, the job has settled with no waiter.
async fn next_settle_state(events: &mut tokio::sync::broadcast::Receiver<JobEvent>) -> JobState {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
            Ok(Ok(JobEvent::Settled { state, .. })) => return state,
            Ok(Ok(JobEvent::Started { .. })) => continue,
            Ok(Err(error)) => panic!("no settle event: {error}"),
            Err(_) => panic!("timed out waiting for a settle event"),
        }
    }
}

/// A job that settles with **no** waiter leaves the watched value `true` as well
/// (`send_replace`; `send` would return `Err` without storing it), so a later
/// `job` wait returns at once with the exited state, and never the contradictory
/// "[still running …]" line. On the old code it slept the whole 30 s and printed both lines.
#[tokio::test]
async fn a_wait_on_a_settled_job_returns_at_once() {
    let dir = TempDir::new("jobs-settled-wait");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");
    let mut events = session.jobs().subscribe();

    let (id, _) = start(&session, "echo done", None).await;

    // Nobody waits while it settles; the registry's own settle notice is the barrier.
    assert_eq!(
        next_settle_state(&mut events).await.label(),
        "exited 0",
        "the job must settle on its own before the wait below"
    );

    let started = Instant::now();
    let text = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 30_000}),
        )
        .await
        .unwrap(),
    );
    let waited = started.elapsed();
    assert!(text.contains("[status: exited 0]"), "{text}");
    assert!(
        !text.contains("still running"),
        "a settled job must never be reported as still running: {text}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "a wait on a settled job must return at once, not after its timeout: {waited:?}"
    );
}

/// The same for an adopted job: a foreground command moved to the background that finishes with
/// nobody waiting is settled at once on the next `job` wait (the adopt path shares
/// `settle`).
#[tokio::test]
async fn a_wait_on_a_settled_adopted_job_returns_at_once() {
    let dir = TempDir::new("auto-settled-wait");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = adopting_tools(&root, Duration::from_millis(300));
    let session = tools.open_session().await.expect("session");
    let mut events = session.jobs().subscribe();

    mkfifo(&root.join("gate"));
    let (id, _) = adopted(
        &exec(
            &session,
            "shell",
            json!({"command": "cat gate > /dev/null; echo after"}),
        )
        .await
        .unwrap(),
    );

    // Nobody waits: release it, and wait on the registry's settle notice.
    release_fifo(&root.join("gate"));
    assert_eq!(
        next_settle_state(&mut events).await.label(),
        "exited 0",
        "the adopted job must settle on its own before the wait below"
    );

    let started = Instant::now();
    let text = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 30_000}),
        )
        .await
        .unwrap(),
    );
    let waited = started.elapsed();
    assert!(text.contains("after"), "{text}");
    assert!(text.contains("[status: exited 0]"), "{text}");
    assert!(
        !text.contains("still running"),
        "a settled job must never be reported as still running: {text}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "a wait on a settled adopted job must return at once: {waited:?}"
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

    // a `job` kill does not.
    let mut events = session.jobs().subscribe();
    let (id, _) = start(&session, "echo $$ > k.pid; sleep 600", None).await;
    wait_for_pid(&root, "k.pid").await;
    exec(&session, "job", json!({"id": id, "op": "kill"}))
        .await
        .unwrap();
    assert!(
        !next_settle_notify(&mut events).await,
        "a kill must not notify"
    );

    // A timeout notifies: the model set the deadline itself but did not ask to kill the job, so
    // it may be waiting on the settle. ADR 0037 removed the explicit-background
    // timeout, so the only deadline left is the one an adopted foreground call carries
    // (revision 1 decision 3); this drives that path.
    let adopting = adopting_tools(&root, Duration::from_millis(300));
    let adopted_session = adopting.open_session().await.expect("session");
    let mut events = adopted_session.jobs().subscribe();
    let output = exec(
        &adopted_session,
        "shell",
        json!({"command": "echo $$ > t.pid; sleep 600", "timeout_secs": 2}),
    )
    .await
    .unwrap();
    let value = structured(&output);
    assert_eq!(value["adopted"], json!(true), "{value}");
    assert!(
        next_settle_notify(&mut events).await,
        "an adopted job's timeout notifies"
    );
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
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 300}),
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

// ---------- 3. an explicit background `timeout_secs` is ignored (ADR 0037) ----------

/// ADR 0037: `background: true` ignores `timeout_secs`. The job is still
/// running after the second the field would have killed it at, the whole group is alive, and the
/// acknowledgement says the field is ignored and names the two ways to bound the job. On the
/// pre-revision code the deadline fires here and the job settles as timed out.
#[tokio::test]
async fn a_background_timeout_secs_is_ignored() {
    let dir = TempDir::new("jobs-timeout-ignored");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let output = exec(
        &session,
        "shell",
        json!({"command": TWO_SLEEPS, "background": true, "timeout_secs": 1}),
    )
    .await
    .expect("start the job");
    let value = structured(&output);
    let id = value["job_id"].as_str().expect("a job id").to_owned();
    assert_eq!(value["timeout_secs"], json!(null), "{value}");
    assert_eq!(value["timeout_secs_ignored"], json!(true), "{value}");
    let text = output_text(&output);
    assert!(
        text.contains("is ignored for background jobs"),
        "the acknowledgement must say the field is ignored: {text}"
    );
    assert!(
        text.contains("notified when it finishes"),
        "the acknowledgement ends with the notice line: {text}"
    );

    let leader = wait_for_pid(&root, "leader.pid").await;
    let background = wait_for_pid(&root, "bg.pid").await;

    // Wait on the job's **own** state, with a bound past the old 1 s deadline: it is still
    // running (a bounded wait, not a sleep for ordering). On the pre-revision code it would have
    // settled as timed out at 1 s, so this returns `[status: timed out]` at once instead.
    let status = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 2000}),
        )
        .await
        .unwrap(),
    );
    assert!(status.contains("[status: running]"), "{status}");
    assert!(status.contains("the job was not killed"), "{status}");
    // A bounded process-state check: on the pre-revision code both are gone long before now.
    assert!(
        !gone_within(leader, Duration::from_secs(1)).await,
        "the shell (pid {leader}) was killed by an ignored `timeout_secs`"
    );
    assert!(
        !gone_within(background, Duration::from_secs(1)).await,
        "the child (pid {background}) was killed by an ignored `timeout_secs`"
    );
    assert!(group_alive(leader), "group {leader} lost members");

    // a `job` kill is the way to stop it.
    exec(&session, "job", json!({"id": id, "op": "kill"}))
        .await
        .unwrap();
    let killed = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(killed.contains("[status: killed]"), "{killed}");
}

/// The contrast (ADR 0037): a **foreground** command's `timeout_secs` is
/// still a deadline, and it still kills the whole session at 1 s.
#[tokio::test]
async fn a_foreground_timeout_secs_still_kills_the_command() {
    let dir = TempDir::new("jobs-foreground-timeout");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    // No adoption: this call must run to its own 1 s deadline.
    let tools = transparent_tools(&root).with_limits(Limits {
        auto_background_after: None,
        ..Limits::default()
    });
    let session = tools.open_session().await.expect("session");

    let output = exec(
        &session,
        "shell",
        json!({"command": "echo $$ > fg.pid; sleep 600", "timeout_secs": 1}),
    )
    .await
    .unwrap();
    let value = structured(&output);
    assert_eq!(value["timed_out"], json!(true), "{value}");
    assert_eq!(value["timeout_secs"], json!(1), "{value}");
    assert_eq!(value["background"], json!(null), "{value}");

    let pid = wait_for_pid(&root, "fg.pid").await;
    assert!(
        gone_within(pid, Duration::from_secs(10)).await,
        "the foreground command (pid {pid}) survived its `timeout_secs`"
    );
    assert!(!group_alive(pid), "group {pid} still has members");
}

// ---------- 3b. kill kills the whole group ----------

/// `job` kill SIGTERMs the whole group (then SIGKILLs): neither the shell nor its background
/// child survives, and the group is empty.
#[tokio::test]
async fn job_kill_op_kills_the_whole_process_group() {
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
        &exec(&session, "job", json!({"id": id, "op": "kill"}))
            .await
            .unwrap(),
    );
    assert!(killed.contains("stopping"), "{killed}");

    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "the shell (pid {leader}) survived the kill"
    );
    assert!(
        gone_within(background, Duration::from_secs(10)).await,
        "the child (pid {background}) survived the kill"
    );
    assert!(!group_alive(leader), "group {leader} still has members");

    // The job settles as killed, and a second kill is a no-op that says so.
    let done = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(done.contains("[status: killed]"), "{done}");
    let again = output_text(
        &exec(&session, "job", json!({"id": id, "op": "kill"}))
            .await
            .unwrap(),
    );
    assert!(again.contains("already finished"), "{again}");
}

// ---------- 3c. a natural exit kills the group too ----------

/// A command that exits on its own but leaves a background grandchild: the supervisor kills
/// the whole process group when the leader exits, so the grandchild does not outlive it. This
/// is the group-cleanup path that a `job` kill and the timeout do **not** exercise (they signal
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
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 15_000}),
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
        failure.message.contains("`op: \"wait\"`"),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("`op: \"kill\"`"),
        "{}",
        failure.message
    );
    assert!(!failure.message.contains("job_list"), "{}", failure.message);

    // Settle one, then the next start succeeds.
    exec(&session, "job", json!({"id": ids[0], "op": "kill"}))
        .await
        .unwrap();
    exec(
        &session,
        "job",
        json!({"id": ids[0], "op": "wait", "timeout_ms": 10_000}),
    )
    .await
    .unwrap();
    let (id, _) = start(&session, "sleep 600", None).await;
    assert!(!id.is_empty());

    session.shutdown_jobs();
}

// ---------- 5. turn cancellation does not kill jobs ----------

/// Cancelling the turn ends a `job` wait but leaves the job running (ADR 0037
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
        "job",
        json!({"id": id, "op": "wait", "timeout_ms": 60_000}),
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

// ---------- 7. the job_list alias ----------

/// The `job_list` alias reports each job's id, state, command and output path, prefixed
/// with the rename line.
#[tokio::test]
async fn the_job_list_alias_reports_the_states() {
    let dir = TempDir::new("jobs-list");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.expect("session");

    let (done_id, done_output) = start(&session, "echo done", None).await;
    let done = output_text(
        &exec(
            &session,
            "job",
            json!({"id": done_id, "op": "wait", "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(done.contains("[status: exited 0]"), "{done}");
    let (run_id, run_output) = start(&session, "sleep 600", None).await;

    let list = output_text(&exec(&session, "job_list", json!({})).await.unwrap());
    assert!(
        list.starts_with("`job_list` was renamed"),
        "the alias names the rename first: {list}"
    );
    assert!(list.contains(&done_id), "{list}");
    assert!(list.contains("exited 0"), "{list}");
    assert!(list.contains(&run_id), "{list}");
    assert!(list.contains("running"), "{list}");
    // The list names `job` where the output path used to be (ADR 0039 decision 1).
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

    let failure = exec(&session, "job", json!({"id": "job-0-0", "op": "read"}))
        .await
        .expect_err("unknown job");
    assert_eq!(failure.code, "unknown_job");
    let failure = exec(&session, "job", json!({"id": "job-0-0", "op": "kill"}))
        .await
        .expect_err("unknown job");
    assert_eq!(failure.code, "unknown_job");
    assert!(
        failure.message.contains("no job `job-0-0`"),
        "one error name for both kinds: {failure:?}"
    );
    // The old names are still accepted and answer with the rename line (ADR 0043 decision 3).
    let failure = exec(&session, "job_output", json!({"job_id": "job-0-0"}))
        .await
        .expect_err("unknown job");
    assert_eq!(failure.code, "unknown_job");
    assert!(
        failure.message.starts_with("`job_output` was renamed"),
        "{failure:?}"
    );
    let failure = exec(&session, "job_kill", json!({"job_id": "job-0-0"}))
        .await
        .expect_err("unknown job");
    assert_eq!(failure.code, "unknown_job");
    assert!(
        failure.message.starts_with("`job_kill` was renamed"),
        "{failure:?}"
    );
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

// ---------- 10. adopting a running foreground call (ADR 0037) ----------

/// Tools whose foreground calls are adopted once they run past `threshold`.
fn adopting_tools(ws: &Path, threshold: Duration) -> CodingTools {
    transparent_tools(ws).with_limits(Limits {
        auto_background_after: Some(threshold),
        ..Limits::default()
    })
}

/// Asserts `output` is an adoption result and returns `(job_id, text)`.
fn adopted(output: &ToolOutput) -> (String, String) {
    let value = structured(output);
    assert_eq!(value["background"], json!(true), "{value}");
    assert_eq!(value["adopted"], json!(true), "{value}");
    (
        value["job_id"].as_str().expect("a job id").to_owned(),
        output_text(output),
    )
}

/// The acceptance case: a foreground call still running at the threshold is moved to the
/// background **in place** — the same process (its pid does not change), its pre-adoption
/// output lands in the job's file, and `job` starts after the adoption point. Fails on
/// the pre-revision code, where the call would run on to the foreground timeout (and, without
/// one, for the default 60 s).
#[tokio::test]
async fn a_long_foreground_command_is_adopted_in_place() {
    let dir = TempDir::new("auto-adopt");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = adopting_tools(&root, Duration::from_secs(1));
    let session = tools.open_session().await.expect("session");

    mkfifo(&root.join("gate"));
    let start_time = Instant::now();
    let call = exec(
        &session,
        "shell",
        json!({"command": "echo $$ > leader.pid; echo before; cat gate > /dev/null; echo after"}),
    );
    let leader_pid = wait_for_pid(&root, "leader.pid");
    let (result, pid) = tokio::join!(call, leader_pid);
    let (id, text) = adopted(&result.expect("the call is adopted"));

    assert!(
        start_time.elapsed() < Duration::from_secs(5),
        "the call must return near the threshold, not at a foreground timeout: {:?}",
        start_time.elapsed()
    );
    assert!(text.contains("before"), "{text}");
    assert!(text.contains("moved to the background"), "{text}");
    assert!(text.contains(&id), "{text}");

    // Never restarted: the pid file is unchanged and the leader is alive.
    assert_eq!(
        wait_for_pid(&root, "leader.pid").await,
        pid,
        "the command must not be restarted"
    );
    assert!(alive(pid), "the adopted process (pid {pid}) must be alive");

    // Registered and running.
    let list = output_text(&exec(&session, "job_list", json!({})).await.unwrap());
    assert!(list.contains(&id), "{list}");
    assert!(list.contains("running"), "{list}");

    // The pre-adoption output is already in the job's file.
    let path = session.jobs().output_path(&id);
    wait_for_file(&path, "before").await;

    // Release the command; the job settles.
    release_fifo(&root.join("gate"));
    let done = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 10_000}),
        )
        .await
        .unwrap(),
    );
    assert!(done.contains("after"), "{done}");
    assert!(done.contains("[status: exited 0]"), "{done}");
    // `job` starts at the adoption point: the tail already in the result is not
    // repeated.
    assert!(!done.contains("before"), "the increment only: {done}");

    // The file has the whole output, before and after adoption.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("before"), "{content:?}");
    assert!(content.contains("after"), "{content:?}");
}

/// A cancel before the threshold kills the foreground call and adopts nothing (decision 6).
#[tokio::test]
async fn a_cancel_before_the_threshold_kills_it_and_adopts_nothing() {
    let dir = TempDir::new("auto-cancel");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    // A long threshold: the cancel is what ends the call, not the timer.
    let tools = adopting_tools(&root, Duration::from_secs(30));
    let session = tools.open_session().await.expect("session");

    let token = CancellationToken::new();
    let call = exec_cancel(
        &session,
        "shell",
        json!({"command": "echo $$ > leader.pid; sleep 600", "timeout_secs": 600}),
        token.clone(),
    );
    let pid = async {
        let pid = wait_for_pid(&root, "leader.pid").await;
        token.cancel();
        pid
    };
    let (result, pid) = tokio::join!(call, pid);
    let value = structured(&result.expect("the call returns"));
    assert_eq!(value["cancelled"], json!(true), "{value}");
    assert!(
        gone_within(pid, Duration::from_secs(10)).await,
        "a pre-threshold cancel must kill the command (pid {pid})"
    );
    assert!(
        session.jobs().list().is_empty(),
        "a pre-threshold cancel must adopt nothing"
    );
}

/// The explicit `timeout_secs` keeps counting from the command's start after adoption
/// (decision 3): the job is killed at the original deadline, not at adoption + timeout.
#[tokio::test]
async fn an_adopted_job_is_killed_by_the_original_timeout() {
    let dir = TempDir::new("auto-timeout");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    // Adopt at 2 s, deadline at 3 s: the kill must land near 3 s, not near 5 s (which is
    // what restarting the clock at adoption would give).
    let tools = adopting_tools(&root, Duration::from_secs(2));
    let session = tools.open_session().await.expect("session");

    let start = Instant::now();
    let (id, _) = adopted(
        &exec(
            &session,
            "shell",
            json!({"command": "echo $$ > leader.pid; sleep 600", "timeout_secs": 3}),
        )
        .await
        .unwrap(),
    );
    let settled = output_text(
        &exec(
            &session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 15_000}),
        )
        .await
        .unwrap(),
    );
    assert!(settled.contains("[status: timed out]"), "{settled}");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(3_800),
        "the timeout must count from the command's start, not from adoption: {elapsed:?}"
    );
}

/// With all four slots busy the call is not adopted: it runs to its foreground timeout and
/// the result says why (decision 4).
#[tokio::test]
async fn four_running_jobs_leave_no_room_and_the_call_is_not_adopted() {
    let dir = TempDir::new("auto-slotfull");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = adopting_tools(&root, Duration::from_millis(300));
    let session = tools.open_session().await.expect("session");

    for _ in 0..4 {
        let (id, _) = start(&session, "sleep 600", None).await;
        assert!(!id.is_empty());
    }

    let output = exec(
        &session,
        "shell",
        json!({"command": "echo $$ > fg.pid; sleep 600", "timeout_secs": 2}),
    )
    .await
    .unwrap();
    let value = structured(&output);
    assert_eq!(value["adopted"], json!(null), "{value}");
    assert_eq!(value["timed_out"], json!(true), "{value}");
    assert_eq!(value["slots_full"], json!(true), "{value}");
    let text = output_text(&output);
    assert!(text.contains("slots were busy"), "{text}");
    assert_eq!(
        session.jobs().list().len(),
        4,
        "no fifth job may be created"
    );

    session.shutdown_jobs();
}

/// `auto_background_after = None` (the config's `0`) keeps the foreground rules: the call is
/// killed at its timeout and never adopted. The same call with a threshold before the timeout
/// **is** adopted (the test above), so this pins the off switch.
#[tokio::test]
async fn auto_background_off_keeps_the_foreground_timeout() {
    let dir = TempDir::new("auto-off");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        auto_background_after: None,
        ..Limits::default()
    });
    let session = tools.open_session().await.expect("session");

    let output = exec(
        &session,
        "shell",
        json!({"command": "echo $$ > leader.pid; sleep 600", "timeout_secs": 2}),
    )
    .await
    .unwrap();
    let value = structured(&output);
    assert_eq!(value["timed_out"], json!(true), "{value}");
    assert_eq!(value["adopted"], json!(null), "{value}");
    assert_eq!(value["slots_full"], json!(false), "{value}");
    assert!(session.jobs().list().is_empty(), "nothing may be adopted");
}

/// After adoption the job belongs to the registry, so the turn's cancellation no longer kills
/// it (decision 6). Fails if the cancel path stayed wired to the adopted job.
#[tokio::test]
async fn an_adopted_job_survives_the_turn_cancellation() {
    let dir = TempDir::new("auto-cancel-after");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = adopting_tools(&root, Duration::from_millis(300));
    let session = tools.open_session().await.expect("session");

    mkfifo(&root.join("gate"));
    let token = CancellationToken::new();
    let (id, _) = adopted(
        &exec_cancel(
            &session,
            "shell",
            json!({"command": "echo $$ > leader.pid; cat gate > /dev/null"}),
            token.clone(),
        )
        .await
        .unwrap(),
    );
    let leader = wait_for_pid(&root, "leader.pid").await;
    token.cancel();
    assert!(
        !gone_within(leader, Duration::from_secs(1)).await,
        "a cancelled turn must not kill an adopted job (pid {leader})"
    );
    let list = output_text(&exec(&session, "job_list", json!({})).await.unwrap());
    assert!(list.contains(&id) && list.contains("running"), "{list}");

    // Session end is the way to stop it.
    session.shutdown_jobs();
    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "session shutdown must kill the adopted job (pid {leader})"
    );
}

/// Session shutdown kills an adopted job (ADR 0037 decision 4 / 6).
#[tokio::test]
async fn session_shutdown_kills_an_adopted_job() {
    let dir = TempDir::new("auto-shutdown");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = adopting_tools(&root, Duration::from_millis(300));
    let session = tools.open_session().await.expect("session");

    let (_id, _) = adopted(
        &exec(
            &session,
            "shell",
            json!({"command": "echo $$ > leader.pid; sleep 600"}),
        )
        .await
        .unwrap(),
    );
    let leader = wait_for_pid(&root, "leader.pid").await;

    session.shutdown_jobs();
    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "the adopted job (pid {leader}) survived session shutdown"
    );
    assert!(!group_alive(leader), "group {leader} still has members");
}

/// An adopted job's natural completion is notified like any other job (ADR 0037 decision 3):
/// the model did not wait on it, so `notify` is true.
#[tokio::test]
async fn an_adopted_job_completion_is_notified() {
    let dir = TempDir::new("auto-notify");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = adopting_tools(&root, Duration::from_millis(300));
    let session = tools.open_session().await.expect("session");
    let mut events = session.jobs().subscribe();

    mkfifo(&root.join("gate"));
    let (_id, _) = adopted(
        &exec(
            &session,
            "shell",
            json!({"command": "cat gate > /dev/null; echo after"}),
        )
        .await
        .unwrap(),
    );
    release_fifo(&root.join("gate"));
    assert!(
        next_settle_notify(&mut events).await,
        "an adopted job's natural exit must notify"
    );
}

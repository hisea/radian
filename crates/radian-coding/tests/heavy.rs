//! ADR 0030 O8 / revision 1 R4: the process-level heavy-command gate on `shell`.
//!
//! These use a `FullAccess` sandbox (needs no backend), so they run everywhere. The gate is
//! injected with [`CodingTools::with_heavy`], never the process gate.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::{CodingSession, CodingTools, HeavySlots, Limits};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::json;

/// Runs one `shell` call through fresh `CodingTools` that carry `gate`.
async fn shell(
    ws: &Path,
    gate: Arc<HeavySlots>,
    command: &str,
    cancel: CancellationToken,
) -> Result<agent_core::ToolOutput, String> {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full access needs no backend");
    let tools = CodingTools::with_sandbox(ws, sandbox)
        .expect("matching workspaces")
        .with_heavy(gate);
    let session = tools.open_session().await.expect("open session");
    let call = ToolCall {
        id: "t".into(),
        name: "shell".into(),
        arguments: json!({ "command": command }),
    };
    let sink = agent_core::NoopSink;
    let cx = ToolContext {
        cancellation: cancel,
        events: &sink,
        index: 0,
    };
    session
        .execute(&call, cx)
        .await
        .map_err(|failure| failure.message)
}

/// The queued milliseconds the shell tool reported for a run (a heavy command), if any.
fn queued_ms(output: &agent_core::ToolOutput) -> Option<u64> {
    output.structured.as_ref()?.get("heavy_queued_ms")?.as_u64()
}

fn text_of(output: &agent_core::ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_heavy_command_waits_for_the_slot_and_a_light_one_does_not() {
    let dir = TempDir::new("heavy-gate");
    let ws = dir.path().to_path_buf();
    let gate = HeavySlots::new(1, ["echo heavy"]);

    // Hold the only slot.
    let cancel = CancellationToken::new();
    let held = gate
        .acquire("echo heavy", &cancel)
        .await
        .expect("acquire")
        .expect("heavy");

    // A heavy command cannot finish while the slot is held.
    let mut blocked = tokio::spawn({
        let ws = ws.clone();
        let gate = Arc::clone(&gate);
        async move { shell(&ws, gate, "echo heavy", CancellationToken::new()).await }
    });
    let early = tokio::time::timeout(Duration::from_millis(150), &mut blocked).await;
    assert!(early.is_err(), "a heavy command must wait for the slot");

    // A light command runs at once, holding no slot. (A timeout makes the "a non-heavy command
    // takes a slot" mutation fail cleanly instead of hanging.)
    let light = tokio::time::timeout(
        Duration::from_secs(5),
        shell(
            &ws,
            Arc::clone(&gate),
            "echo light",
            CancellationToken::new(),
        ),
    )
    .await
    .expect("a light command does not wait for a slot")
    .expect("the light command runs");
    assert_eq!(queued_ms(&light), None, "a light command never queues");
    assert!(
        text_of(&light).contains("echo light"),
        "the light command ran"
    );

    // Free the slot: the heavy command runs and reports that it queued.
    drop(held);
    let heavy = blocked
        .await
        .expect("the task")
        .expect("the heavy command runs");
    assert!(
        queued_ms(&heavy).is_some(),
        "the heavy command reports its queue wait"
    );
    assert_eq!(gate.available(), 1, "the slot is free again");
}

#[tokio::test]
async fn cancelling_a_queued_heavy_command_runs_nothing() {
    let dir = TempDir::new("heavy-cancel");
    let ws = dir.path().to_path_buf();
    let gate = HeavySlots::new(1, ["echo heavy"]);
    let marker = dir.file("marker");

    let cancel = CancellationToken::new();
    let held = gate
        .acquire("echo heavy", &cancel)
        .await
        .expect("acquire")
        .expect("heavy");

    // The command would create the marker if it ever ran.
    let waiting = CancellationToken::new();
    let token = waiting.clone();
    let task = tokio::spawn({
        let ws = ws.clone();
        let gate = Arc::clone(&gate);
        let command = format!("echo heavy > {}", marker.display());
        async move { shell(&ws, gate, &command, token).await }
    });
    // Let the call reach the wait, then cancel it. The timeout makes the "the gate ignores
    // cancellation" mutation fail cleanly instead of hanging.
    tokio::task::yield_now().await;
    waiting.cancel();
    let output = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("a cancelled wait returns at once")
        .expect("the task")
        .expect("a cancelled result, not an error");
    let structured = output.structured.clone().expect("structured");
    assert_eq!(structured["cancelled"], json!(true));
    assert_eq!(structured["heavy_queued"], json!(true));
    assert!(!marker.exists(), "a cancelled wait never runs the command");
    drop(held);
    assert_eq!(gate.available(), 1);
}

// ---------- the release at settle (ADR 0030 O8 review fix) ----------

/// Opens **one** session over `ws` with `gate` as the heavy gate. Unlike [`shell`], the whole
/// test shares this session and its job registry, so a background job and the commands after it
/// draw on the same gate.
async fn heavy_session(ws: &Path, gate: Arc<HeavySlots>) -> CodingSession {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full access needs no backend");
    let tools = CodingTools::with_sandbox(ws, sandbox)
        .expect("matching workspaces")
        .with_heavy(gate);
    tools.open_session().await.expect("open session")
}

/// Runs one tool call through an already-open session.
async fn exec(
    session: &CodingSession,
    name: &str,
    arguments: serde_json::Value,
) -> Result<agent_core::ToolOutput, String> {
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments,
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    session
        .execute(&call, cx)
        .await
        .map_err(|failure| failure.message)
}

/// Starts `command` as a background job and returns its id.
async fn start_background(session: &CodingSession, command: &str) -> String {
    let output = exec(
        session,
        "shell",
        json!({ "command": command, "background": true }),
    )
    .await
    .expect("start the job");
    output.structured.as_ref().expect("structured")["job_id"]
        .as_str()
        .expect("a job id")
        .to_owned()
}

/// Waits for the job to settle and returns the `[status: ...]` text.
async fn wait_for_settle(session: &CodingSession, id: &str) -> String {
    let output = exec(
        session,
        "job_output",
        json!({ "job_id": id, "wait": true, "timeout_ms": 30_000 }),
    )
    .await
    .expect("wait for the job");
    text_of(&output)
}

/// A background heavy job releases its slot when it settles on its own. A settled cell stays in
/// the registry for `job_list`, so without that release the slot is held until the cell is
/// dropped and every later heavy command blocks forever.
#[tokio::test]
async fn a_settled_background_heavy_job_releases_its_slot() {
    let dir = TempDir::new("heavy-settle");
    let ws = dir.path().to_path_buf();
    let gate = HeavySlots::new(1, ["echo heavy"]);
    let session = heavy_session(&ws, Arc::clone(&gate)).await;

    // A background heavy job that exits on its own holds the only slot until it settles.
    let id = start_background(&session, "echo heavy settled").await;
    let status = wait_for_settle(&session, &id).await;
    assert!(status.contains("[status: exited 0]"), "{status}");
    assert_eq!(
        gate.available(),
        1,
        "a settled background heavy job must release its slot"
    );

    // The next heavy command therefore runs without waiting (the missing release would leave
    // it queued forever, so the timeout makes the mutation fail instead of hang).
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        exec(&session, "shell", json!({ "command": "echo heavy again" })),
    )
    .await
    .expect("a settled heavy job must not block the next heavy command")
    .expect("the command runs");
    assert!(
        text_of(&output).contains("echo heavy again"),
        "the second heavy command ran"
    );
    assert_eq!(
        gate.available(),
        1,
        "the second heavy command released its slot"
    );
}

/// The same release happens when the job is stopped with `job_kill` rather than exiting on its
/// own (the settle path, not the natural-exit path).
#[tokio::test]
async fn a_killed_background_heavy_job_releases_its_slot() {
    let dir = TempDir::new("heavy-kill");
    let ws = dir.path().to_path_buf();
    let gate = HeavySlots::new(1, ["echo heavy"]);
    let session = heavy_session(&ws, Arc::clone(&gate)).await;

    // A long-running heavy job holds the only slot...
    let id = start_background(&session, "echo heavy start; sleep 600").await;
    assert_eq!(gate.available(), 0, "the running job holds the heavy slot");

    // ...until `job_kill` stops it.
    exec(&session, "job_kill", json!({ "job_id": id }))
        .await
        .expect("kill the job");
    let status = wait_for_settle(&session, &id).await;
    assert!(status.contains("[status: killed]"), "{status}");
    assert_eq!(
        gate.available(),
        1,
        "a killed background heavy job must release its slot"
    );

    // So the next heavy command runs without waiting.
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        exec(
            &session,
            "shell",
            json!({ "command": "echo heavy after-kill" }),
        ),
    )
    .await
    .expect("a killed heavy job must not block the next heavy command")
    .expect("the command runs");
    assert!(
        text_of(&output).contains("echo heavy after-kill"),
        "the second heavy command ran"
    );
}

// ---------- the heavy permit of an adopted foreground command (ADR 0037) ----------

/// An adopted foreground heavy command keeps its heavy slot **inside the job** (ADR 0030 O8):
/// the permit moves into `Jobs::adopt` like it does into `Jobs::start`, so the slot stays taken
/// until the job settles and is released at that settle. Without the move the foreground `run`
/// drops the permit the moment it returns at adoption, freeing the slot while the job still runs.
#[tokio::test]
async fn an_adopted_foreground_heavy_command_holds_and_releases_its_slot() {
    let dir = TempDir::new("heavy-adopt");
    let ws = dir.path().to_path_buf();
    let gate = HeavySlots::new(1, ["echo heavy"]);
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, &ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full access needs no backend");
    let tools = CodingTools::with_sandbox(&ws, sandbox)
        .expect("matching workspaces")
        .with_heavy(Arc::clone(&gate))
        .with_limits(Limits {
            // A tiny threshold: the foreground heavy command is adopted in place.
            auto_background_after: Some(Duration::from_millis(300)),
            ..Limits::default()
        });
    let session = tools.open_session().await.expect("open session");

    // The foreground heavy command runs past the threshold and is adopted; its job holds the
    // only slot from then on.
    let output = exec(
        &session,
        "shell",
        json!({ "command": "echo heavy adopt; sleep 600" }),
    )
    .await
    .expect("the call is adopted");
    let structured = output.structured.clone().expect("structured");
    assert_eq!(structured["adopted"], json!(true), "{structured}");
    let id = structured["job_id"].as_str().expect("a job id").to_owned();
    assert_eq!(
        gate.available(),
        0,
        "an adopted heavy job must keep the heavy slot"
    );

    // Killing the job settles it, and the settle releases the slot.
    exec(&session, "job_kill", json!({ "job_id": id }))
        .await
        .expect("kill the job");
    let status = wait_for_settle(&session, &id).await;
    assert!(status.contains("[status: killed]"), "{status}");
    assert_eq!(
        gate.available(),
        1,
        "a settled adopted heavy job must release its slot"
    );
}

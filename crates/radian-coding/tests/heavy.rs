//! ADR 0030 O8 / revision 1 R4: the process-level heavy-command gate on `shell`.
//!
//! These use a `FullAccess` sandbox (needs no backend), so they run everywhere. The gate is
//! injected with [`CodingTools::with_heavy`], never the process gate.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    CancellationToken, ContentPart, ToolCall, ToolContext, ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::{CodingTools, HeavySlots};
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

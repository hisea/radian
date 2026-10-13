//! Context budget guard (ADR 0015): the three usage states, the block, and the Continue path.

mod common;

use std::sync::{Arc, Mutex};

use agent_core::*;
use common::*;
use serde_json::{Value, json};

fn text_with_usage(s: &str, input: u64) -> Reply {
    Reply::Respond(ModelResponse {
        reasoning: Vec::new(),
        content: vec![ContentPart::text(s)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: Some(ModelUsage {
            input: Some(input),
            ..Default::default()
        }),
    })
}

fn calls_with_usage(list: Vec<ToolCall>, input: u64) -> Reply {
    Reply::Respond(ModelResponse {
        reasoning: Vec::new(),
        content: vec![],
        tool_calls: list,
        provider_payload: None,
        stop_reason: StopReason::ToolUse,
        usage: Some(ModelUsage {
            input: Some(input),
            ..Default::default()
        }),
    })
}

/// A tool argument big enough that a 64 KiB tool result lands in the next request.
fn big_args() -> Value {
    json!({"blob": "x".repeat(64 * 1024)})
}

fn with_budget<P: ToolProvider>(
    model: Arc<ScriptedModel>,
    tools: P,
    budget: ContextBudget,
) -> Arc<Agent<Arc<ScriptedModel>, P, ()>> {
    Arc::new(Agent::new(model, tools).with_config(AgentConfig {
        context_budget: Some(budget),
        ..Default::default()
    }))
}

fn block_report(status: &RunStatus) -> &BudgetReport {
    let RunStatus::ContextBudget(report) = status else {
        panic!("expected a ContextBudget stop, got {status:?}");
    };
    report
}

/// 1. A provider-reported `input` near the window blocks the *next* request, and the model
///    never sees that request.
#[tokio::test]
async fn reported_usage_blocks_the_next_request_before_it_is_sent() {
    let model = ScriptedModel::new([
        calls_with_usage(vec![call("c1", "echo", json!({"a": 1}))], 900),
        text_with_usage("never sent", 950),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = with_budget(model.clone(), tools, ContextBudget::new(1000))
        .open_session()
        .await
        .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let report = block_report(&outcome.status);
    assert_eq!(report.source, UsageSource::Reported);
    assert_eq!(report.baseline_input_tokens, Some(900));
    assert_eq!(report.window_tokens, 1000);
    // The second request was never sent: only the first call reached the model.
    assert_eq!(model.requests().len(), 1);
}

/// 2. With no provider usage, the projection is a local estimate and the report says so.
#[tokio::test]
async fn without_reported_usage_the_estimate_is_labelled() {
    let model = ScriptedModel::new([text("never sent")]);
    let mut chat = with_budget(model.clone(), NoTools, ContextBudget::new(100))
        .open_session()
        .await
        .unwrap();

    // 2000 chars across a 4-byte/token estimate is ~500 tokens, past the 80-token line.
    let outcome = chat
        .run_turn("x".repeat(2000), TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let report = block_report(&outcome.status);
    assert_eq!(report.source, UsageSource::Estimated);
    assert_eq!(report.baseline_input_tokens, None);
    assert_eq!(model.requests().len(), 0);
}

/// 3. No configured window disables the guard, and the host can see that it is off.
#[tokio::test]
async fn no_window_configured_disables_the_guard() {
    let model = ScriptedModel::new([text("ok")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(model.requests().len(), 1);
    assert_eq!(chat.context_budget_status(), &BudgetStatus::Disabled);
}

/// 4. The Continue path: one relaxed request goes out, and the guard applies again after it.
#[tokio::test]
async fn relaxing_once_sends_that_request_and_restores_the_guard() {
    let model = ScriptedModel::new([
        Reply::Respond(ModelResponse {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![call("c1", "echo", big_args())],
            provider_payload: None,
            stop_reason: StopReason::ToolUse,
            usage: None,
        }),
        text("done"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = with_budget(model.clone(), tools, ContextBudget::new(1000))
        .open_session()
        .await
        .unwrap();

    // Turn 1: the 64 KiB tool result pushes the second request over the line.
    let first = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    block_report(&first.status);
    assert_eq!(model.requests().len(), 1);

    // Turn 2: the host chose Continue; the over-budget request goes out once.
    let second = chat
        .run_turn(
            "continue",
            TurnOptions {
                context_budget_relax: true,
                ..Default::default()
            },
            &NoopSink,
        )
        .await
        .unwrap();
    assert_eq!(second.status, RunStatus::Completed);
    assert_eq!(model.requests().len(), 2);

    // Turn 3: the exemption was consumed; the same oversized history blocks again.
    let third = chat
        .run_turn("more", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    block_report(&third.status);
    assert_eq!(model.requests().len(), 2);
}

/// A hook that snapshots `request.messages` at every `before_model_call`.
#[derive(Default)]
struct TranscriptSnaps(Mutex<Vec<Vec<u8>>>);

impl Hooks for TranscriptSnaps {
    async fn before_model_call(
        &self,
        _cx: &HookContext,
        request: &mut ModelRequest,
    ) -> Result<(), HookError> {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::to_vec(&request.messages).unwrap());
        Ok(())
    }
}

/// 5. Blocking changes nothing in the transcript: the bytes at the blocked request equal the
///    bytes after the turn ends (no implicit compression or rewriting).
#[tokio::test]
async fn blocking_does_not_change_the_transcript() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", big_args())]),
        text("never sent"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let snaps = Arc::new(TranscriptSnaps::default());
    let mut chat = Arc::new(
        Agent::new(model.clone(), tools)
            .with_config(AgentConfig {
                context_budget: Some(ContextBudget::new(1000)),
                ..Default::default()
            })
            .with_hooks(Arc::clone(&snaps)),
    )
    .open_session()
    .await
    .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    block_report(&outcome.status);

    // The hook ran on both requests; the second is the one the guard blocked.
    let snaps = snaps.0.lock().unwrap();
    assert_eq!(snaps.len(), 2);
    let blocked_view = snaps.last().unwrap();
    let final_view = serde_json::to_vec(chat.transcript()).unwrap();
    assert_eq!(blocked_view, &final_view);
    // And the growth from the tool result is real, so the guard did not simply hide it.
    assert!(snaps[1].len() > snaps[0].len());
    assert_eq!(chat.transcript().len(), 3);
}

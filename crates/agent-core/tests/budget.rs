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
    // A reported baseline plus estimated growth is not a precise report, and is labelled so.
    assert_eq!(report.source, UsageSource::ReportedPlusEstimate);
    assert_eq!(report.baseline_input_tokens, Some(900));
    assert!(report.estimated_growth_tokens.unwrap() > 0);
    assert_eq!(
        report.projected_tokens,
        900 + report.estimated_growth_tokens.unwrap()
    );
    assert_eq!(report.window_tokens, 1000);
    // The second request was never sent: only the first call reached the model.
    assert_eq!(model.requests().len(), 1);
}

fn request_with(text: &str) -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages: vec![Message::user(text)],
        tools: vec![],
        settings: serde_json::Value::Null,
    }
}

/// The estimator must not undercount non-ASCII text. Chinese is ~3 UTF-8 bytes per
/// character and 1+ token per character, so the documented lower bound is **one token per
/// non-ASCII character** (ADR 0015 C2). A bare `bytes/4` would give only 0.75. 3000 Han
/// characters are 9000 bytes; `9000/4 = 2250`, well below the real count.
#[test]
fn chinese_text_estimates_at_least_one_token_per_character() {
    let estimate = estimate_tokens(&request_with(&"中".repeat(3000)), 4);
    assert!(
        estimate >= 3000,
        "Chinese must not be undercounted: {estimate} < 3000"
    );
    assert!(estimate > 9000 / 4, "a plain bytes/4 would have been 2250");
}

/// The documented ASCII bound: every `bytes_per_token` ASCII bytes counts as at least one
/// token, rounded up (ADR 0015 C2).
#[test]
fn ascii_code_estimates_at_least_bytes_over_four() {
    let code = "let answer = compute(41, 1);\n".repeat(200);
    let ascii_bytes = code.len() as u64;
    let estimate = estimate_tokens(&request_with(&code), 4);
    let bound = ascii_bytes.div_ceil(4);
    assert!(estimate >= bound, "{estimate} < {bound}");
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

/// 4. The Continue path: one relaxed request goes out, and the guard applies again after it
///    — both for the same turn's next request and for a later turn.
#[tokio::test]
async fn relaxing_once_sends_that_request_and_restores_the_guard() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", big_args())]),
        calls(vec![call("c2", "echo", big_args())]),
        text("never sent"),
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

    // Turn 2: the host chose Continue. The over-budget request goes out once, then the same
    // turn's next request is protected again.
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
    block_report(&second.status);
    assert_eq!(model.requests().len(), 2);

    // Turn 3: the exemption was for one request last turn, not for the session.
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

/// 6. A session resumed from a reported usage restores that baseline, and its next projection
///    is exactly what an uninterrupted session would have projected over the same transcript
///    and configuration. The restore recomputes the request size with the same rule, so the
///    two reports are equal field for field.
#[tokio::test]
async fn a_restored_baseline_matches_an_uninterrupted_projection() {
    let budget = ContextBudget::new(2000);

    // Uninterrupted: turn 1 reports input 900; turn 2 is checked against the live baseline.
    let mut live = with_budget(
        ScriptedModel::new([text_with_usage("first", 900), text("second")]),
        NoTools,
        budget,
    )
    .open_session()
    .await
    .unwrap();
    live.run_turn("one", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    live.run_turn("two", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let live_status = live.context_budget_status().clone();

    // Resumed: the same transcript and config, with the reported baseline restored. The
    // request that produced the report held only the first user message, so its prefix is 1.
    let agent = Arc::new(
        Agent::new(ScriptedModel::new([text("second")]), NoTools).with_config(AgentConfig {
            context_budget: Some(budget),
            ..Default::default()
        }),
    );
    let mut resumed = agent
        .open_session_with_history(vec![Message::user("one"), Message::assistant("first")])
        .await
        .unwrap();
    assert!(
        resumed.restore_budget_baseline(1, 900),
        "the prefix is inside the transcript"
    );
    resumed
        .run_turn("two", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let resumed_status = resumed.context_budget_status().clone();

    assert_eq!(
        resumed_status, live_status,
        "resume must not change the projection"
    );
    let BudgetStatus::Ok(report) = &resumed_status else {
        panic!("expected an under-threshold report, got {resumed_status:?}");
    };
    assert_eq!(report.source, UsageSource::ReportedPlusEstimate);
    assert_eq!(report.baseline_input_tokens, Some(900));
    assert!(report.estimated_growth_tokens.unwrap() > 0);
}

/// A prefix outside the transcript is refused rather than turned into a baseline of zero: the
/// projection stays a pure estimate.
#[tokio::test]
async fn a_prefix_outside_the_transcript_leaves_the_estimate_alone() {
    let agent = Arc::new(
        Agent::new(ScriptedModel::new([text("ok")]), NoTools).with_config(AgentConfig {
            context_budget: Some(ContextBudget::new(1000)),
            ..Default::default()
        }),
    );
    let mut chat = agent
        .open_session_with_history(vec![Message::user("one")])
        .await
        .unwrap();
    assert!(!chat.restore_budget_baseline(9, 500));
    chat.run_turn("two", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let BudgetStatus::Ok(report) = chat.context_budget_status() else {
        panic!("expected a report");
    };
    assert_eq!(report.source, UsageSource::Estimated);
    assert_eq!(report.baseline_input_tokens, None);
}

//! Context budget projection (ADR 0015, kept by ADR 0018): the usage states and the fact that
//! crossing the input budget `B` does **not** stop the request.

mod common;

use std::sync::Arc;

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

fn over_report(status: &BudgetStatus) -> &BudgetReport {
    let BudgetStatus::Over(report) = status else {
        panic!("expected an over-budget status, got {status:?}");
    };
    report
}

fn declared_calls(transcript: &[Message]) -> Vec<String> {
    transcript
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        })
        .collect()
}

fn answered_calls(transcript: &[Message]) -> Vec<String> {
    transcript
        .iter()
        .filter_map(|message| match message {
            Message::Tool { result, .. } => Some(result.call_id.clone()),
            _ => None,
        })
        .collect()
}

/// 1. A provider-reported `input` over the input budget `B`: the request is **still sent**,
///    the turn completes, and the status is `Over` with an honest source.
#[tokio::test]
async fn reported_usage_over_the_budget_still_sends_the_request() {
    let model = ScriptedModel::new([
        calls_with_usage(vec![call("c1", "echo", json!({"a": 1}))], 900),
        text_with_usage("second reply", 950),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = with_budget(model.clone(), tools, ContextBudget::new(800))
        .open_session()
        .await
        .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    // Both requests reached the model: crossing `B` does not stop the turn.
    assert_eq!(model.requests().len(), 2);

    let report = over_report(chat.context_budget_status());
    // A reported baseline plus estimated growth is not a precise report, and is labelled so.
    assert_eq!(report.source, UsageSource::ReportedPlusEstimate);
    assert_eq!(report.baseline_input_tokens, Some(900));
    assert!(report.estimated_growth_tokens.unwrap() > 0);
    assert_eq!(
        report.projected_tokens,
        900 + report.estimated_growth_tokens.unwrap()
    );
    assert_eq!(report.input_budget_tokens, 800);
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

/// 2. With no provider usage, the projection is a local estimate and the report says so; the
///    over-budget request still goes out.
#[tokio::test]
async fn without_reported_usage_the_estimate_is_labelled_and_the_request_still_goes() {
    let model = ScriptedModel::new([text("reply")]);
    let mut chat = with_budget(model.clone(), NoTools, ContextBudget::new(100))
        .open_session()
        .await
        .unwrap();

    // 2000 chars across a 4-byte/token estimate is ~500 tokens, past the 100-token budget.
    let outcome = chat
        .run_turn("x".repeat(2000), TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(model.requests().len(), 1);
    let report = over_report(chat.context_budget_status());
    assert_eq!(report.source, UsageSource::Estimated);
    assert_eq!(report.baseline_input_tokens, None);
    assert_eq!(report.input_budget_tokens, 100);
}

/// 3. No configured budget means no number is claimed, and the host can see that.
#[tokio::test]
async fn no_budget_configured_is_disabled() {
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

/// 3b. A projection at or below `B` is `Ok`, and the boundary is strict: `T == B` is still
///     within budget, `T == B + 1` is over (ADR 0018; compaction is due only strictly above).
#[test]
fn the_boundary_is_strictly_above_the_budget() {
    let mut report = BudgetReport {
        source: UsageSource::Estimated,
        projected_tokens: 800,
        input_budget_tokens: 800,
        request_bytes: 0,
        baseline_input_tokens: None,
        estimated_growth_tokens: None,
    };
    assert!(!over_budget(&report), "T == B is within the budget");
    report.projected_tokens = 801;
    assert!(over_budget(&report), "T == B + 1 is over");
}

/// 3c. Under the budget the session status is `Ok`, not `Over`.
#[tokio::test]
async fn under_the_budget_is_ok() {
    let model = ScriptedModel::new([text("ok")]);
    let mut chat = with_budget(model.clone(), NoTools, ContextBudget::new(100_000))
        .open_session()
        .await
        .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(model.requests().len(), 1);
    let report = match chat.context_budget_status() {
        BudgetStatus::Ok(report) => report,
        other => panic!("expected Ok, got {other:?}"),
    };
    assert!(
        report.projected_tokens <= report.input_budget_tokens,
        "{report:?}"
    );
}

/// 4. A turn whose projection is over `B` completes normally: no stop, no relax, and the
///    final status is `Over`.
#[tokio::test]
async fn a_turn_over_the_budget_completes_normally() {
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", big_args())]), text("done")]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = with_budget(model.clone(), tools, ContextBudget::new(1000))
        .open_session()
        .await
        .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(model.requests().len(), 2);
    let report = over_report(chat.context_budget_status());
    assert!(report.projected_tokens > 1000, "{report:?}");
}

/// 5. Over `B`, the transcript is the normal one: nothing is dropped or rewritten to get
///    under the budget (there is no implicit compression).
#[tokio::test]
async fn over_the_budget_does_not_change_the_transcript() {
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", big_args())]), text("done")]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = with_budget(model.clone(), tools, ContextBudget::new(1000))
        .open_session()
        .await
        .unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let transcript = chat.transcript();
    assert_eq!(transcript.len(), 4, "{transcript:?}");
    assert!(matches!(&transcript[0], Message::User { .. }));
    assert!(matches!(&transcript[1], Message::Assistant { .. }));
    assert!(matches!(&transcript[2], Message::Tool { .. }));
    assert_eq!(transcript[3].text(), "done");
    assert_eq!(declared_calls(transcript), answered_calls(transcript));
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
        panic!("expected an under-budget report, got {resumed_status:?}");
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

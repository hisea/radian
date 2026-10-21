//! ADR 0017: an empty final answer (no tool calls, `EndTurn`, every visible text part
//! empty) is a transient model error, never a success. It is retried inside the model call
//! like a dropped stream, and the turn fails once the retries are spent.
//!
//! The retry waits use the paused test clock (`start_paused`), so the backoff costs no wall
//! time. Each test covers one cause.

mod common;

use std::sync::Arc;

use common::*;
use radian_core::testing::RecordingJournal;
use radian_core::*;
use serde_json::json;

/// A final answer with no content at all.
fn empty() -> Reply {
    Reply::Respond(ModelResponse {
        limits: None,
        reasoning: Vec::new(),
        content: vec![],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    })
}

/// A final answer whose only text part is empty, carrying `usage` to prove the discarded
/// attempt journals nothing.
fn empty_with_usage(usage: ModelUsage) -> Reply {
    Reply::Respond(ModelResponse {
        limits: None,
        reasoning: Vec::new(),
        content: vec![ContentPart::text("")],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: Some(usage),
    })
}

/// A final answer with reasoning but no visible text: empty for the user.
fn reasoning_only() -> Reply {
    Reply::Respond(ModelResponse {
        limits: None,
        reasoning: vec![ContentPart::text("thinking")],
        content: vec![],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    })
}

/// An empty text part with a tool call attached: the call still runs.
fn empty_text_with_call() -> Reply {
    Reply::Respond(ModelResponse {
        limits: None,
        reasoning: Vec::new(),
        content: vec![ContentPart::text("")],
        tool_calls: vec![call("c1", "echo", json!({}))],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    })
}

fn usage(n: u64) -> ModelUsage {
    ModelUsage {
        input: Some(n),
        ..ModelUsage::default()
    }
}

/// Opens a journaled session with `retries` configured.
async fn session_with_retries(
    model: Arc<ScriptedModel>,
    journal: Arc<RecordingJournal>,
    retries: u32,
) -> AgentSession<Arc<ScriptedModel>, NoTools, ()> {
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        retries,
        ..Default::default()
    }))
    .open_session_with_journal(Vec::new(), journal)
    .await
    .unwrap()
}

fn assistant_texts(transcript: &[Message]) -> Vec<String> {
    transcript
        .iter()
        .filter(|m| matches!(m, Message::Assistant { .. }))
        .map(Message::text)
        .collect()
}

fn journal_assistant_texts(records: &[JournalRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::Message { message, .. } => Some(message),
            _ => None,
        })
        .filter(|m| matches!(m, Message::Assistant { .. }))
        .map(Message::text)
        .collect()
}

fn journal_usages(records: &[JournalRecord]) -> Vec<ModelUsage> {
    records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::ModelUsage { usage } => Some(usage.clone()),
            _ => None,
        })
        .collect()
}

/// An empty reply followed by a normal one: the turn completes with the normal reply, and the
/// journal holds exactly the normal assistant message — including its usage, but never the
/// discarded attempt's.
#[tokio::test(start_paused = true)]
async fn an_empty_reply_is_retried_and_only_the_normal_reply_is_kept() {
    let model = ScriptedModel::new([empty_with_usage(usage(7)), {
        let Reply::Respond(mut r) = text("done") else {
            unreachable!()
        };
        r.usage = Some(usage(9));
        Reply::Respond(r)
    }]);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with_retries(Arc::clone(&model), journal.clone(), 3).await;
    let rec = Recorder::default();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &rec)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_message.unwrap().text(), "done");
    assert_eq!(assistant_texts(chat.transcript()), ["done"]);
    assert_eq!(model.requests().len(), 2, "one request per attempt");
    assert_eq!(
        journal_assistant_texts(&journal.records()),
        ["done"],
        "the empty attempt committed no assistant message"
    );
    assert_eq!(
        journal_usages(&journal.records()),
        [usage(9)],
        "the discarded attempt's usage is journaled nowhere (ADR 0007 J5 needs a message)"
    );
}

/// Empty replies on every attempt: the turn fails with the empty-response error, never
/// completes, and nothing is committed for them.
#[tokio::test(start_paused = true)]
async fn exhausted_empty_replies_fail_the_turn_instead_of_completing() {
    let model = ScriptedModel::new([empty(), empty(), empty()]);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with_retries(Arc::clone(&model), journal.clone(), 2).await;
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    let message = err.to_string();
    assert!(
        message.contains("the model returned an empty response"),
        "{message}"
    );
    assert!(message.contains("(after 2 retries)"), "{message}");
    assert_eq!(run.outcome().unwrap().status, RunStatus::Failed);
    assert_eq!(model.requests().len(), 3, "one try plus two retries");
    assert!(
        journal_assistant_texts(&journal.records()).is_empty(),
        "no assistant message was committed for any empty attempt"
    );
    assert!(
        journal_usages(&journal.records()).is_empty(),
        "no usage was journaled for any empty attempt"
    );
}

/// A reasoning-only reply (no text, no tools, `EndTurn`) is empty: it is retried, not
/// completed.
#[tokio::test(start_paused = true)]
async fn a_reasoning_only_reply_is_empty() {
    let model = ScriptedModel::new([reasoning_only(), text("ok")]);
    let mut chat = Arc::new(Agent::new(model.clone(), NoTools).with_config(AgentConfig {
        retries: 3,
        ..Default::default()
    }))
    .open_session()
    .await
    .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_message.unwrap().text(), "ok");
    assert_eq!(model.requests().len(), 2);
}

/// Whitespace-only text is not empty: `"  "` is committed and the turn completes.
#[tokio::test]
async fn whitespace_only_text_is_committed_not_retried() {
    let model = ScriptedModel::new([text("  ")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(assistant_texts(chat.transcript()), ["  "]);
    assert_eq!(model.requests().len(), 1, "no retry happened");
}

/// An empty text part with a tool call is not empty: the call runs as today.
#[tokio::test(start_paused = true)]
async fn an_empty_text_with_a_tool_call_still_dispatches() {
    let model = ScriptedModel::new([empty_text_with_call(), text("ok")]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = Arc::new(Agent::new(model.clone(), tools).with_config(AgentConfig {
        retries: 3,
        ..Default::default()
    }))
    .open_session()
    .await
    .unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(log.started(), ["c1"]);
    assert_eq!(model.requests().len(), 2);
}

/// Empty text with `MaxTokens` keeps today's handling: `Incomplete(MaxTokens)`, no retry.
#[tokio::test]
async fn an_empty_max_tokens_reply_stays_incomplete() {
    let model = ScriptedModel::new([stop(StopReason::MaxTokens, "")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Incomplete(IncompleteReason::MaxTokens)
    );
    assert_eq!(model.requests().len(), 1, "no retry happened");
}

/// Cancelling during the retry wait after an empty reply ends the turn `cancelled` and sends
/// no further request — the same path as any transient retry's backoff.
#[tokio::test(start_paused = true)]
async fn cancelling_during_the_wait_after_an_empty_reply_ends_cancelled() {
    let model = ScriptedModel::new([empty(), text("never sent")]);
    let mut chat = Arc::new(Agent::new(model.clone(), NoTools).with_config(AgentConfig {
        retries: 3,
        ..Default::default()
    }))
    .open_session()
    .await
    .unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let handle = run.cancellation_handle();
    let events = move |event: AgentEvent| {
        if matches!(event, AgentEvent::ModelRetrying { .. }) {
            handle.cancel();
        }
    };

    let outcome = run.run(&events).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    assert_eq!(
        model.requests().len(),
        1,
        "no request after the cancellation"
    );
}

//! ADR 0017 for the compaction summary call: a transient model error is retried
//! inside one summary call, before anything is committed, exactly as the turn's conversation
//! call retries it. Each of the two sumlen attempts has its own retry budget and its own 60 s
//! cap.
//!
//! The waits use the paused test clock (`start_paused`), so a 2 s/4 s/8 s backoff and the 60 s
//! cap cost no wall time. The summary path announces no `ModelRetrying` (a host clears its live
//! projection on that event, which is wrong mid-compaction), so the retries are observed through
//! the number of model requests and the committed summary.

mod common;

use std::sync::Arc;
use std::time::Duration;

use radian_core::testing::RecordingJournal;
use radian_core::{
    Agent, AgentConfig, AgentEvent, CancellationToken, CompactionError, CompactionFailure,
    ContentPart, ContextBudget, JournalRecord, Message, ModelError, ModelResponse, ModelUsage,
    NoTools, RunStatus, StopReason, TurnOptions,
};
use common::{Recorder, Reply, ScriptedModel, text};

fn summary_reply(summary: &str, input: u64) -> Reply {
    Reply::Respond(ModelResponse {
        limits: None,
        reasoning: Vec::new(),
        content: vec![ContentPart::text(summary)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: Some(ModelUsage {
            input: Some(input),
            ..Default::default()
        }),
    })
}

/// A compaction agent whose `retries` are the setting under study: one token per byte, so an
/// 8000-byte message is 8000 projected tokens and the first message crosses `B = 3000`.
fn retrying_agent(
    model: Arc<ScriptedModel>,
    retries: u32,
) -> Arc<Agent<Arc<ScriptedModel>, NoTools, ()>> {
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        context_budget: Some(ContextBudget::new(3_000).with_bytes_per_token(1)),
        auto_compact: true,
        retries,
        ..Default::default()
    }))
}

/// A history whose first message is far over the keep target, so compaction has a message to
/// summarize.
fn big_history() -> Vec<Message> {
    vec![
        Message::user("x".repeat(8_000)),
        Message::assistant("ok"),
        Message::user("last"),
    ]
}

/// The `deterministic` flag of every `ContextCompacted` candidate in the journal.
fn compacted_deterministic(journal: &RecordingJournal) -> Vec<bool> {
    journal
        .records()
        .iter()
        .filter_map(|record| match record {
            JournalRecord::ContextCompacted { deterministic, .. } => Some(*deterministic),
            _ => None,
        })
        .collect()
}

/// How many `ModelRetrying` the host was told about.
fn model_retrying_count(recorder: &Recorder) -> usize {
    recorder
        .events()
        .iter()
        .filter(|event| matches!(event, AgentEvent::ModelRetrying { .. }))
        .count()
}

/// A transient summary error is retried: the same summary request is resent, the retry succeeds,
/// and the model summary (not the deterministic fallback) commits.
#[tokio::test(start_paused = true)]
async fn a_transient_summary_error_is_retried_and_the_model_summary_commits() {
    let model = ScriptedModel::new([
        Reply::Unavailable("HTTP 503: busy".into(), None),
        summary_reply("short summary", 50),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = retrying_agent(model.clone(), 3);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();
    let started = tokio::time::Instant::now();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(session.transcript_revision(), 1, "the retry committed");
    assert!(
        session.transcript()[0].text().contains("short summary"),
        "the model summary committed: {}",
        session.transcript()[0].text()
    );
    assert_eq!(
        compacted_deterministic(&journal),
        vec![false],
        "the model summary committed, not the deterministic fallback"
    );
    // One failed attempt, its retry, then the compacted conversation.
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "one try, one retry, one conversation");
    assert_eq!(
        requests[0].messages, requests[1].messages,
        "the retry resends the same summary request"
    );
    // A host clears its live projection on `ModelRetrying`; the summary must not announce one.
    assert_eq!(model_retrying_count(&recorder), 0);
    // The backoff really ran (2 s plus jitter), on the paused clock.
    assert!(
        tokio::time::Instant::now() - started >= Duration::from_secs(2),
        "the retry waited the backoff"
    );
}

/// Transient summary errors past `retries` fall back deterministically on the automatic path.
#[tokio::test(start_paused = true)]
async fn transient_summary_errors_past_the_retries_fall_back() {
    let busy = || Reply::Unavailable("HTTP 503: busy".into(), None);
    let model = ScriptedModel::new([busy(), busy(), busy(), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = retrying_agent(model.clone(), 2);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(session.transcript_revision(), 1, "the fallback committed");
    assert_eq!(
        compacted_deterministic(&journal),
        vec![true],
        "only the deterministic candidate committed"
    );
    assert!(
        session.transcript()[0].text().contains("without a summary"),
        "{}",
        session.transcript()[0].text()
    );
    // One try plus two retries, then the compacted conversation.
    assert_eq!(
        model.requests().len(),
        4,
        "one try, two retries, conversation"
    );
    assert_eq!(model_retrying_count(&recorder), 0);
}

/// A manual `/compact` has no fallback: the transient errors past `retries` fail it, and the
/// error text carries the retries really made.
#[tokio::test(start_paused = true)]
async fn transient_summary_errors_past_the_retries_fail_a_manual_compact() {
    let busy = || Reply::Unavailable("HTTP 503: busy".into(), None);
    let model = ScriptedModel::new([busy(), busy(), busy()]);
    let agent = retrying_agent(model.clone(), 2);
    let mut session = agent
        .open_session_with_journal(
            vec![
                Message::user("x".repeat(200_000)),
                Message::assistant("ok"),
                Message::user("last"),
            ],
            Arc::new(RecordingJournal::new()),
        )
        .await
        .unwrap();

    let error = session
        .compact_now(
            None,
            &CancellationToken::new(),
            &Recorder::default(),
            None,
            None,
        )
        .await
        .unwrap_err();

    let CompactionFailure::Compaction(CompactionError::Summary(error)) = &error else {
        panic!("expected a summary failure: {error:?}");
    };
    let ModelError::Unavailable { message, .. } = error else {
        panic!("expected an unavailable error: {error:?}");
    };
    assert!(message.contains("(after 2 retries)"), "{message}");
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
    assert_eq!(model.requests().len(), 3, "one try plus two retries");
}

/// A non-transient summary error (a 400) is never retried, even with retries configured.
#[tokio::test(start_paused = true)]
async fn a_non_transient_summary_error_is_not_retried() {
    let model = ScriptedModel::new([
        Reply::Protocol("HTTP 400: bad request".into()),
        text("unused"),
    ]);
    let agent = retrying_agent(model.clone(), 3);
    let mut session = agent
        .open_session_with_journal(
            vec![
                Message::user("x".repeat(200_000)),
                Message::assistant("ok"),
                Message::user("last"),
            ],
            Arc::new(RecordingJournal::new()),
        )
        .await
        .unwrap();

    let error = session
        .compact_now(
            None,
            &CancellationToken::new(),
            &Recorder::default(),
            None,
            None,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            CompactionFailure::Compaction(CompactionError::Summary(ModelError::Protocol(_)))
        ),
        "{error:?}"
    );
    assert_eq!(model.requests().len(), 1, "no retry");
}

/// Cancelling during a summary backoff ends the turn `cancelled` at once, sends no further
/// request, and commits nothing.
#[tokio::test(start_paused = true)]
async fn cancelling_during_a_summary_backoff_cancels() {
    let model = ScriptedModel::new([
        Reply::Unavailable("HTTP 503: busy".into(), Some(Duration::from_secs(30))),
        summary_reply("never used", 10),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = retrying_agent(model.clone(), 3);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();
    let started = tokio::time::Instant::now();
    // A deadline that falls inside the 30 s backoff, not after it.
    let options = TurnOptions {
        deadline: Some(tokio::time::Instant::now() + Duration::from_secs(1)),
        ..TurnOptions::default()
    };

    let outcome = session.run_turn("go", options, &recorder).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(radian_core::CancelReason::DeadlineExceeded)
    );
    assert_eq!(
        model.requests().len(),
        1,
        "no request after the cancellation"
    );
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
    assert!(
        !journal
            .records()
            .iter()
            .any(|r| matches!(r, JournalRecord::TranscriptReplaced { .. })),
        "a cancelled compaction commits nothing"
    );
    assert!(
        tokio::time::Instant::now() - started < Duration::from_secs(30),
        "the cancellation ended the wait at once, not after the backoff"
    );
}

/// The one-call 60 s cap holds for a summary call: a `Retry-After` over 60 s is clamped, so the
/// summary gets exactly one retry and then gives up. The manual path is used because it observes
/// the summary call alone — an automatic path would add a conversation call whose own retry could
/// hide the summary's.
#[tokio::test(start_paused = true)]
async fn the_summary_call_waits_at_most_sixty_seconds() {
    let busy = || Reply::Unavailable("HTTP 503: busy".into(), Some(Duration::from_secs(90)));
    let model = ScriptedModel::new([busy(), busy(), busy()]);
    let agent = retrying_agent(model.clone(), 3);
    let mut session = agent
        .open_session_with_journal(
            vec![
                Message::user("x".repeat(200_000)),
                Message::assistant("ok"),
                Message::user("last"),
            ],
            Arc::new(RecordingJournal::new()),
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();

    let error = session
        .compact_now(
            None,
            &CancellationToken::new(),
            &Recorder::default(),
            None,
            None,
        )
        .await
        .unwrap_err();

    // 90 s clamped to the 60 s budget: the first retry is made, the budget is then spent, and the
    // call gives up rather than firing the remaining attempts back to back.
    let CompactionFailure::Compaction(CompactionError::Summary(error)) = &error else {
        panic!("expected a summary failure: {error:?}");
    };
    let ModelError::Unavailable { message, .. } = error else {
        panic!("expected an unavailable error: {error:?}");
    };
    assert!(message.contains("(after 1 retry)"), "{message}");
    assert_eq!(
        model.requests().len(),
        2,
        "one try plus the one capped retry"
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(60),
        "the whole backoff is the 60 s cap, not 90 s and not two waits"
    );
}

/// Each of the two sumlen attempts has its own retry budget: the original request can spend its
/// retry and still leave one for the shorten request. A shared budget would give up on the
/// shorten request instead, committing the deterministic fallback.
#[tokio::test(start_paused = true)]
async fn each_summary_attempt_has_its_own_retry_budget() {
    let overlong = "L".repeat(9_000);
    let model = ScriptedModel::new([
        Reply::Unavailable("HTTP 503: busy".into(), None), // attempt 1, try 1 fails
        summary_reply(&overlong, 50),                      // attempt 1, retry: over-long
        Reply::Unavailable("HTTP 503: busy".into(), None), // attempt 2 (shorten), try 1 fails
        summary_reply("the shorter summary", 10),          // attempt 2, retry: good
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = retrying_agent(model.clone(), 1);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(session.transcript_revision(), 1);
    assert!(
        session.transcript()[0]
            .text()
            .contains("the shorter summary"),
        "the shortened model summary committed: {}",
        session.transcript()[0].text()
    );
    assert_eq!(compacted_deterministic(&journal), vec![false]);
    // Two summary calls per sumlen attempt (the try and its own retry), then the conversation.
    assert_eq!(
        model.requests().len(),
        5,
        "four summary calls, then the compacted conversation"
    );
}

/// ADR 0017 fix round 1: an empty summary (`""`) is NOT the conversation empty-retry. ADR 0018
/// handles it itself — `check_summary` resends the original request once, immediately, and
/// every summary attempt is recorded as `CompactionUsage`.
#[tokio::test(start_paused = true)]
async fn an_empty_summary_is_resent_once_without_a_backoff_and_both_attempts_are_billed() {
    let model = ScriptedModel::new([
        summary_reply("", 50),
        summary_reply("short summary", 60),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = retrying_agent(model.clone(), 3);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();
    let started = tokio::time::Instant::now();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(session.transcript_revision(), 1, "the retry committed");
    assert!(
        session.transcript()[0].text().contains("short summary"),
        "the model summary committed: {}",
        session.transcript()[0].text()
    );
    assert_eq!(
        compacted_deterministic(&journal),
        vec![false],
        "the model summary committed, not the deterministic fallback"
    );
    // Two summary requests, then the compacted conversation.
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "two summaries, one conversation");
    assert_eq!(
        requests[0].messages, requests[1].messages,
        "the second summary resends the original summary request (not a shorten request)"
    );
    // ADR 0018 billing: every summary attempt is recorded, even the empty one.
    let billed: Vec<u64> = journal
        .records()
        .iter()
        .filter_map(|record| match record {
            JournalRecord::CompactionUsage { usage, .. } => usage.input,
            _ => None,
        })
        .collect();
    assert_eq!(
        billed,
        vec![50, 60],
        "both summary attempts are recorded as CompactionUsage"
    );
    assert_eq!(model_retrying_count(&recorder), 0);
    // The conversation empty-retry would have waited 2 s plus jitter; the controlled resend
    // is immediate.
    assert!(
        tokio::time::Instant::now() - started < Duration::from_secs(2),
        "the empty summary was resent immediately, with no retry backoff"
    );
}

/// ADR 0017 fix round 1: a manual `/compact` whose two summaries are both `""` fails with
/// `InvalidSummary` (ADR 0018), not with the conversation empty-retry's transport error.
#[tokio::test(start_paused = true)]
async fn two_empty_summaries_fail_a_manual_compact_with_invalid_summary() {
    let model = ScriptedModel::new([summary_reply("", 50), summary_reply("", 60)]);
    // `retries = 1`: on the old (buggy) path the two empties spend exactly the one retry and
    // fail as `Summary(Transport("... (after 1 retry)"))`; on the fixed path the retry count
    // is irrelevant — ADR 0018 resends once and reports `InvalidSummary`.
    let agent = retrying_agent(model.clone(), 1);
    let mut session = agent
        .open_session_with_journal(
            vec![
                Message::user("x".repeat(200_000)),
                Message::assistant("ok"),
                Message::user("last"),
            ],
            Arc::new(RecordingJournal::new()),
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();

    let error = session
        .compact_now(
            None,
            &CancellationToken::new(),
            &Recorder::default(),
            None,
            None,
        )
        .await
        .unwrap_err();

    let CompactionFailure::Compaction(CompactionError::InvalidSummary(message)) = &error else {
        panic!("expected InvalidSummary, not the empty-retry transport error: {error:?}");
    };
    assert!(
        message.contains("empty"),
        "the InvalidSummary names the empty summary: {message}"
    );
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
    let requests = model.requests();
    assert_eq!(
        requests.len(),
        2,
        "the original request plus its one resend"
    );
    assert_eq!(
        requests[0].messages, requests[1].messages,
        "the resend is the original summary request"
    );
    assert!(
        tokio::time::Instant::now() - started < Duration::from_secs(2),
        "the resend was immediate, with no retry backoff"
    );
}

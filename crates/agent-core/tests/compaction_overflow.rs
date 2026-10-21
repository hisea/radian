//! ADR 0018 §兜底、手动与关闭: the provider-overflow fallback (compact to half and retry once)
//! and the manual `compact_now` (Core half). The proactive budget path stays covered in
//! `compaction_flow.rs`.

mod common;

use std::sync::Arc;

use agent_core::testing::RecordingJournal;
use agent_core::{
    Agent, AgentConfig, AgentEvent, CancellationToken, CompactionError, CompactionFailure,
    CompactionOutcome, CompactionTrigger, ContentPart, ContextBudget, JournalRecord, Message,
    ModelError, ModelResponse, ModelUsage, NoTools, RunError, RunStatus, StopReason, TurnOptions,
    compaction,
};
use common::{Recorder, Reply, ScriptedModel, overflow, text};

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

/// A model that returns `reply` then the conversation answer, with `summary` for any
/// summarizer call; the fallback's exact call sequence is asserted per test.
fn overflow_agent(
    model: Arc<ScriptedModel>,
    budget: Option<u64>,
    auto_compact: bool,
) -> Arc<Agent<Arc<ScriptedModel>, NoTools, ()>> {
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        context_budget: budget.map(ContextBudget::new),
        auto_compact,
        // These tests count requests exactly; a transient failure is one request here, not
        // the ADR 0017 retry storm.
        retries: 0,
        ..Default::default()
    }))
}

/// A big first message to summarize plus a small tail to keep, so the fallback can reach half.
fn big_prefix_history() -> Vec<Message> {
    vec![
        Message::user("x".repeat(200_000)),
        Message::assistant("ok"),
        Message::user("last"),
    ]
}

fn label(record: &JournalRecord) -> String {
    match record {
        JournalRecord::TranscriptReplaced { revision, .. } => format!("replaced:{revision}"),
        JournalRecord::TranscriptReplacementCommitted { revision, .. } => {
            format!("committed:{revision}")
        }
        JournalRecord::CompactionUsage { revision, .. } => format!("compaction-usage:{revision}"),
        JournalRecord::ContextCompacted { revision, .. } => format!("compacted:{revision}"),
        JournalRecord::Message { .. } => "message".into(),
        _ => "other".into(),
    }
}

fn starts(events: &[AgentEvent]) -> Vec<CompactionTrigger> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::CompactionStarted { trigger, .. } => Some(*trigger),
            _ => None,
        })
        .collect()
}

/// 1. Window unknown (no budget) + a provider overflow: the fallback compacts with `K = 20000`,
///    retries once, and the commit order matches the proactive path.
#[tokio::test]
async fn an_overflow_with_no_window_compacts_to_half_and_retries_once() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(
        outcome.model_calls, 1,
        "the summary and the retry are one logical request"
    );
    assert_eq!(session.transcript_revision(), 1, "the fallback committed");
    assert!(compaction::is_compaction_summary(&session.transcript()[0]));

    // Conversation, summary, retried conversation — and the retry sends the compacted view.
    let requests = model.requests();
    assert_eq!(
        requests.len(),
        3,
        "one conversation, one summary, one retry"
    );
    assert!(requests[0].system_prompt.is_none());
    assert!(
        requests[1]
            .system_prompt
            .as_deref()
            .unwrap()
            .starts_with(compaction::SUMMARY_SYSTEM_PROMPT)
    );
    assert!(compaction::is_compaction_summary(&requests[2].messages[0]));

    assert_eq!(
        starts(&recorder.events()),
        vec![CompactionTrigger::Overflow]
    );

    // The journal order is the same as the proactive path's.
    let kinds: Vec<String> = journal.records().iter().map(label).collect();
    let at = |k: &str| {
        kinds
            .iter()
            .position(|x| x == k)
            .unwrap_or_else(|| panic!("{k} missing in {kinds:?}"))
    };
    assert!(at("compaction-usage:1") < at("compacted:1"));
    assert!(at("compacted:1") < at("replaced:1"));
    assert!(at("replaced:1") < at("committed:1"));
}

/// 2. The retry also overflows: failed, and only one retry (two conversation calls + one
///    summary). The error text says it was compacted and retried.
#[tokio::test]
async fn a_retry_that_still_overflows_fails_after_one_retry() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
        overflow("prompt is too long"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    match error {
        RunError::Model(ModelError::ContextOverflow(message)) => {
            assert!(message.contains("compacted once and retried"), "{message}");
        }
        other => panic!("expected a context-overflow model error, got {other:?}"),
    }
    assert_eq!(
        model.requests().len(),
        3,
        "exactly one summary and one retry"
    );
    assert_eq!(session.transcript_revision(), 1);
}

/// 3. A model error that is not a recognizable overflow is a plain failure: no compaction.
#[tokio::test]
async fn a_non_overflow_model_error_does_not_compact() {
    let model = ScriptedModel::new([Reply::Fail("connection reset".into())]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RunError::Model(ModelError::Transport(_))),
        "{error:?}"
    );
    assert_eq!(model.requests().len(), 1);
    assert_eq!(session.transcript_revision(), 0);
    assert!(starts(&recorder.events()).is_empty());
}

/// 4. Window known and the estimate low: the reference is `min(T_before, B)`. The model summary
///    cannot reach `B/2`, so the deterministic fallback drops units until it does — proving the
///    target is half of `B`, not half of the larger `T_before` (ADR 0018).
#[tokio::test]
async fn a_known_window_uses_min_of_projection_and_budget() {
    // `T_before` (~110000 tokens) is well over `B = 30000`; the deterministic fallback must
    // reach `B/2 = 15000`, which `T_before/2` would not require.
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    // `auto_compact = false`: the proactive path is off, so the request is sent and overflows.
    let agent = overflow_agent(model.clone(), Some(30_000), false);
    let history = vec![
        Message::user("x".repeat(380_000)),
        Message::assistant("ok"),
        Message::user("b".repeat(30_000)),
        Message::assistant("ok"),
        Message::user("c".repeat(30_000)),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(session.transcript_revision(), 1, "the fallback committed");
    // The committed view reaches half of `B`, which half of `T_before` would not demand.
    let (before, after) = journal
        .records()
        .iter()
        .find_map(|record| match record {
            JournalRecord::ContextCompacted {
                tokens_before,
                tokens_after,
                deterministic: true,
                ..
            } => Some((*tokens_before, *tokens_after)),
            _ => None,
        })
        .expect("a deterministic candidate");
    assert!(
        after * 2 <= 30_000,
        "{before} -> {after}: half of B = 30000 is required"
    );
}

/// 5. Even the newest unit plus the deterministic summary does not reach the target: `NothingFits`,
///    with nothing committed (ADR 0018).
#[tokio::test]
async fn a_fallback_that_cannot_reach_the_target_is_nothing_fits() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    // A tiny window: `B = 2000`, half is 1000, and the verbatim copies of the two big user
    // messages alone cost ~4000 projected tokens, so no drop level fits.
    let agent = overflow_agent(model.clone(), Some(2_000), false);
    let history = vec![
        Message::user("x".repeat(40_000)),
        Message::user("y".repeat(40_000)),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RunError::Compaction(CompactionError::NothingFits)),
        "{error:?}"
    );
    assert_eq!(session.transcript_revision(), 0);
    let kinds: Vec<String> = journal.records().iter().map(label).collect();
    assert!(
        !kinds.iter().any(|k| k.starts_with("compacted")
            || k.starts_with("replaced")
            || k.starts_with("committed")),
        "{kinds:?}"
    );
}

/// 6. After a proactive compaction the request still overflows: the fallback is allowed once
///    (it is not blocked by "already compacted this turn").
#[tokio::test]
async fn the_fallback_is_allowed_once_after_a_proactive_compaction() {
    // `T_before` (~100000 tokens) is over `B = 80000`, so the proactive path compacts first
    // and leaves a large kept suffix; the request still overflows and the fallback runs.
    let model = ScriptedModel::new([
        summary_reply(&"s".repeat(16_000), 50),
        overflow("prompt is too long"),
        summary_reply("a second summary", 50),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), Some(80_000), true);
    let history = vec![
        Message::user("x".repeat(380_000)),
        Message::assistant("ok"),
        Message::user("b".repeat(38_000)),
        Message::assistant("ok"),
        Message::user("c".repeat(38_000)),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let _ = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await;

    assert_eq!(
        starts(&recorder.events()),
        vec![CompactionTrigger::Budget, CompactionTrigger::Overflow],
        "the proactive path compacts, then the fallback is allowed"
    );
}

/// A manual `/compact` commits, and the instructions reach the summarizer.
#[tokio::test]
async fn compact_now_commits_and_passes_the_instructions() {
    let model = ScriptedModel::new([summary_reply("a short summary", 50)]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .compact_now(
            Some("focus on the parser".into()),
            &CancellationToken::new(),
            &recorder,
            None,
            None,
        )
        .await
        .unwrap();
    let CompactionOutcome::Compacted {
        revision,
        tokens_before,
        tokens_after,
    } = outcome
    else {
        panic!("expected a committed compaction, got {outcome:?}");
    };
    assert_eq!(revision, 1);
    assert!(tokens_after < tokens_before);
    assert_eq!(session.transcript_revision(), 1);
    assert!(compaction::is_compaction_summary(&session.transcript()[0]));
    assert_eq!(starts(&recorder.events()), vec![CompactionTrigger::Manual]);

    // The instructions appear in the one summary request.
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .system_prompt
            .as_deref()
            .unwrap()
            .starts_with(compaction::SUMMARY_SYSTEM_PROMPT)
    );
    assert!(
        requests[0].messages[0]
            .text()
            .contains("focus on the parser"),
        "{:?}",
        requests[0].messages[0].text()
    );
}

/// A manual `/compact` on a history that already fits is `NothingToCompact`, not an error.
#[tokio::test]
async fn compact_now_on_a_small_history_is_nothing_to_compact() {
    let model = ScriptedModel::new([]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(vec![Message::user("hi")], journal)
        .await
        .unwrap();
    let recorder = Recorder::default();
    let outcome = session
        .compact_now(None, &CancellationToken::new(), &recorder, None, None)
        .await
        .unwrap();
    assert_eq!(outcome, CompactionOutcome::NothingToCompact);
    assert_eq!(session.transcript_revision(), 0);
    assert!(model.requests().is_empty(), "no summary was requested");
}

/// A manual `/compact` only has to shrink: `T_after <= B` is not required.
#[tokio::test]
async fn compact_now_only_requires_a_smaller_request() {
    let model = ScriptedModel::new([summary_reply("a short summary", 50)]);
    let journal = Arc::new(RecordingJournal::new());
    // A tiny budget: the compacted view is far over `B`, but it is smaller than before.
    let agent = overflow_agent(model.clone(), Some(500), false);
    let history = vec![
        Message::user("x".repeat(12_000)),
        Message::assistant("ok"),
        Message::user("y".repeat(6_000)),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal)
        .await
        .unwrap();
    let recorder = Recorder::default();
    let outcome = session
        .compact_now(None, &CancellationToken::new(), &recorder, None, None)
        .await
        .unwrap();
    let CompactionOutcome::Compacted {
        tokens_after,
        tokens_before,
        ..
    } = outcome
    else {
        panic!("expected a committed compaction, got {outcome:?}");
    };
    assert!(tokens_after < tokens_before);
    assert!(tokens_after > 500, "still over B, and that is allowed here");
}

/// Cancelling a manual `/compact` commits nothing.
#[tokio::test]
async fn a_cancelled_compact_now_commits_nothing() {
    let model = ScriptedModel::new([summary_reply("a short summary", 50)]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();
    let token = CancellationToken::new();
    token.cancel();
    let error = session
        .compact_now(None, &token, &recorder, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            CompactionFailure::Compaction(CompactionError::Cancelled)
        ),
        "{error:?}"
    );
    assert_eq!(session.transcript_revision(), 0);
    let kinds: Vec<String> = journal.records().iter().map(label).collect();
    assert!(
        !kinds.iter().any(|k| k.starts_with("replaced")),
        "{kinds:?}"
    );
}

// ---------- dogfood7: an over-long summary is shortened on the retry ----------

fn compaction_usage_count(journal: &RecordingJournal) -> usize {
    journal
        .records()
        .iter()
        .filter(|record| matches!(record, JournalRecord::CompactionUsage { .. }))
        .count()
}

/// A manual `/compact` whose first summary is over the limit shortens it on the retry.
#[tokio::test]
async fn compact_now_shortens_an_overlong_summary() {
    let overlong = "L".repeat(40_000);
    let model = ScriptedModel::new([
        summary_reply(&overlong, 50),
        summary_reply("the shorter summary", 10),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .compact_now(None, &CancellationToken::new(), &recorder, None, None)
        .await
        .unwrap();
    assert!(
        matches!(outcome, CompactionOutcome::Compacted { .. }),
        "expected a commit, got {outcome:?}"
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "summary, shortening retry");
    // No budget: keep = 20000, so the ceiling is min(8192, 20000) = 8192; 80% is 6553.
    let shorten_prompt = requests[1].system_prompt.as_deref().unwrap();
    assert!(shorten_prompt.contains("6553 tokens"), "{shorten_prompt}");
    let body = requests[1].messages[0].text();
    assert!(
        body.contains(&overlong),
        "the over-long summary is not the input"
    );
    assert!(!body.contains("Conversation to summarize"), "{body}");
    assert_eq!(compaction_usage_count(&journal), 2);
}

/// The provider-overflow fallback shortens an over-long summary on its one retry, then sends
/// the compacted request.
#[tokio::test]
async fn the_overflow_fallback_shortens_an_overlong_summary() {
    let overlong = "L".repeat(40_000);
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply(&overlong, 50),
        summary_reply("the shorter summary", 10),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(session.transcript_revision(), 1, "the fallback committed");
    let requests = model.requests();
    assert_eq!(
        requests.len(),
        4,
        "conversation, summary, shorten, retried conversation"
    );
    let body = requests[2].messages[0].text();
    assert!(body.contains(&overlong), "{body}");
    assert!(!body.contains("Conversation to summarize"), "{body}");
    assert_eq!(compaction_usage_count(&journal), 2);
}

// ---------- dogfood9: the summary ceiling scales with the window ----------

/// The overflow fallback scales the ceiling with the window it knows about.
#[tokio::test]
async fn the_fallback_scales_the_summary_ceiling_with_the_window() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), Some(103_616), true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), journal)
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    let requests = model.requests();
    // 0: the overflowing request, 1: the summary, 2: the retried conversation.
    assert!(requests.len() >= 2, "{}", requests.len());
    let prompt = requests[1].system_prompt.as_deref().unwrap();
    assert!(prompt.contains("12952 tokens"), "{prompt}");
    assert!(!prompt.contains("8192 tokens"), "{prompt}");
}

/// A manual `/compact` uses the window when it has one and the fixed default when it does not.
#[tokio::test]
async fn compact_now_scales_the_summary_ceiling_with_the_window() {
    // With a window: the scaled ceiling.
    let model = ScriptedModel::new([summary_reply("a short summary", 50)]);
    let agent = overflow_agent(model.clone(), Some(103_616), true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), Arc::new(RecordingJournal::new()))
        .await
        .unwrap();
    let recorder = Recorder::default();
    session
        .compact_now(None, &CancellationToken::new(), &recorder, None, None)
        .await
        .unwrap();
    let prompt = model.requests()[0]
        .system_prompt
        .as_deref()
        .unwrap()
        .to_string();
    assert!(prompt.contains("12952 tokens"), "{prompt}");

    // No window: the fixed default (there is no `B` to scale against).
    let model = ScriptedModel::new([summary_reply("a short summary", 50)]);
    let agent = overflow_agent(model.clone(), None, true);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), Arc::new(RecordingJournal::new()))
        .await
        .unwrap();
    let recorder = Recorder::default();
    session
        .compact_now(None, &CancellationToken::new(), &recorder, None, None)
        .await
        .unwrap();
    let prompt = model.requests()[0]
        .system_prompt
        .as_deref()
        .unwrap()
        .to_string();
    assert!(prompt.contains("8192 tokens"), "{prompt}");
}

/// ADR 0018: the verbatim user messages are part of `T_after`,
/// so the deterministic fallback's target counts them. Here the two verbatim copies alone
/// (~1024 tokens) exceed half of `B = 2000`, so even the newest unit cannot fit and the turn
/// fails explicitly instead of sending a request that is still over the window.
#[tokio::test]
async fn the_verbatim_counts_toward_the_fallback_target() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), Some(2_000), false);
    let history = vec![
        Message::user("x".repeat(48_000)),
        Message::user("y".repeat(36_000)),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RunError::Compaction(CompactionError::NothingFits)),
        "{error:?}"
    );
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
    let kinds: Vec<String> = journal.records().iter().map(label).collect();
    assert!(
        !kinds.iter().any(|kind| kind.starts_with("compacted")),
        "no candidate was written: {kinds:?}"
    );
}

/// ADR 0026 D7: `compact_now` with a budget targets **that** budget, not the configured one —
/// this is how the `/model` confirm compaction reaches the new model's B'. With no configured
/// budget the pre-D7 code used the unlimited keep target and had nothing to do on this
/// history; the given budget must drive it to compact anyway.
#[tokio::test]
async fn compact_now_honours_the_budget_it_is_given() {
    let model = ScriptedModel::new([summary_reply("a short summary", 50)]);
    let agent = overflow_agent(model, None, false);
    let mut session = agent
        .open_session_with_journal(big_prefix_history(), Arc::new(RecordingJournal::new()))
        .await
        .unwrap();
    let outcome = session
        .compact_now(
            None,
            &CancellationToken::new(),
            &Recorder::default(),
            Some(ContextBudget::new(500)),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, CompactionOutcome::Compacted { .. }),
        "the given budget drives the compaction: {outcome:?}"
    );
}

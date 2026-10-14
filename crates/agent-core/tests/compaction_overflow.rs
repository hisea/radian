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

/// 4. Window known and the estimate low: the reference is `min(T_before, B)`, so a target of
///    `B/2` is required even though `T_before` is larger.
#[tokio::test]
async fn a_known_window_uses_min_of_projection_and_budget() {
    // `T_before` (~110000 tokens) is well over `B = 30000`; the kept suffix (~15000, plus the
    // verbatim copy of the huge prefix) is too large for `B/2 = 15000`, but small enough that
    // `T_before/2` would have passed.
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    // `auto_compact = false`: the proactive path is off, so the request is sent and overflows.
    let agent = overflow_agent(model.clone(), Some(30_000), false);
    // 30000-byte messages are 7500 tokens each, so the plan keeps `[asst, b, asst, c, go]`
    // (~15000) and summarizes the huge prefix; `T_after > B/2` makes the fallback fail.
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

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    match error {
        RunError::Compaction(CompactionError::Insufficient { target, .. }) => {
            assert_eq!(target, 15_000, "half of min(T_before, B)");
        }
        other => panic!("expected InsufficientCompaction, got {other:?}"),
    }
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
}

/// 5. The fallback cannot reach half (the last unit alone is over the target): no commit.
#[tokio::test]
async fn a_fallback_that_cannot_reach_half_commits_nothing() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
    // ~30000 tokens total, of which the plan can only summarize ~10000: the kept suffix
    // (~20000) is over half of `T_before`, so the fallback cannot reach its target.
    let history = vec![
        Message::user("x".repeat(38_000)),
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

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            RunError::Compaction(CompactionError::Insufficient { .. })
        ),
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
        .compact_now(None, &CancellationToken::new(), &recorder)
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
        .compact_now(None, &CancellationToken::new(), &recorder)
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
        .compact_now(None, &token, &recorder)
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
        .compact_now(None, &CancellationToken::new(), &recorder)
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
        .compact_now(None, &CancellationToken::new(), &recorder)
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
        .compact_now(None, &CancellationToken::new(), &recorder)
        .await
        .unwrap();
    let prompt = model.requests()[0]
        .system_prompt
        .as_deref()
        .unwrap()
        .to_string();
    assert!(prompt.contains("8192 tokens"), "{prompt}");
}

/// ADR 0018: the verbatim user messages are part of `T_after`, so the fallback's
/// half-gain threshold is higher than it was. This history is chosen to sit **inside** that
/// band: without the verbatim `T_after` would be ~9000 and half of `T_before` (21000) would
/// pass, while with it `T_after` is ~11000, so the fallback fails explicitly instead of sending
/// a request that is still over the window.
#[tokio::test]
async fn the_verbatim_raises_the_fallback_threshold() {
    let model = ScriptedModel::new([
        overflow("prompt is too long"),
        summary_reply("a short summary", 50),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = overflow_agent(model.clone(), None, true);
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
        matches!(
            error,
            RunError::Compaction(CompactionError::Insufficient { .. })
        ),
        "{error:?}"
    );
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
    let kinds: Vec<String> = journal.records().iter().map(label).collect();
    assert!(
        !kinds.iter().any(|kind| kind.starts_with("compacted")),
        "no candidate was written: {kinds:?}"
    );
}

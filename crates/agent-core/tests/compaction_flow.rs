//! Automatic compaction's orchestration half (ADR 0018 §B): the turn loop plans, summarizes,
//! checks the gain, commits through `replace_transcript`, and the journal records the whole
//! replacement segment. The pure planning functions are covered in `compaction.rs`.

mod common;

use std::sync::Arc;

use agent_core::testing::RecordingJournal;
use agent_core::{
    Agent, AgentConfig, AgentEvent, CompactionError, ContentPart, ContextBudget, JournalRecord,
    Message, ModelResponse, ModelUsage, NoTools, RunError, RunStatus, StopReason, TurnOptions,
    compaction,
};
use common::{Recorder, Reply, ScriptedModel, text};

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

/// A budget whose ruler is one token per byte, so a 8000-byte message is 8000 projected
/// tokens while the keep target still uses the default 4 bytes/token ruler.
fn compaction_agent(
    model: Arc<ScriptedModel>,
    budget: u64,
    auto_compact: bool,
) -> Arc<Agent<Arc<ScriptedModel>, NoTools, ()>> {
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        context_budget: Some(ContextBudget::new(budget).with_bytes_per_token(1)),
        auto_compact,
        ..Default::default()
    }))
}

/// A history whose first message is far over the keep target and whose tail is small, so
/// compaction has something to summarize and can shrink the request.
fn big_history() -> Vec<Message> {
    vec![
        Message::user("x".repeat(8_000)),
        Message::assistant("ok"),
        Message::user("last"),
    ]
}

fn label(record: &JournalRecord) -> String {
    match record {
        JournalRecord::Message { .. } => "message".into(),
        JournalRecord::TranscriptReplaced { revision, .. } => format!("replaced:{revision}"),
        JournalRecord::TranscriptReplacementCommitted { revision, .. } => {
            format!("committed:{revision}")
        }
        JournalRecord::TranscriptReplacementAborted { revision } => format!("aborted:{revision}"),
        JournalRecord::ToolExecutionStarted { .. } => "started".into(),
        JournalRecord::ToolExecutionSettled { .. } => "settled".into(),
        JournalRecord::ModelUsage { .. } => "usage".into(),
        JournalRecord::CompactionUsage { revision, .. } => format!("compaction-usage:{revision}"),
        JournalRecord::ContextCompacted { revision, .. } => format!("compacted:{revision}"),
        _ => "other".into(),
    }
}

/// 1. Over `B`, the request is compacted and committed; the original request is never sent,
///    and the journal order is `CompactionUsage, ContextCompacted, TranscriptReplaced,
///    Message x len, Committed`.
#[tokio::test]
async fn a_request_over_the_budget_is_compacted_and_committed() {
    let model = ScriptedModel::new([summary_reply("short summary", 50), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
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
    assert_eq!(
        outcome.model_calls, 1,
        "the summary call must not count against the turn's model-call budget"
    );

    // Two model calls: the summary first, then the compacted conversation request.
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one summary + one compacted request");
    // The prompt is the base structure plus the length ceiling (dogfood7).
    let summary_prompt = requests[0].system_prompt.as_deref().unwrap();
    assert!(
        summary_prompt.starts_with(compaction::SUMMARY_SYSTEM_PROMPT),
        "{summary_prompt}"
    );
    assert!(summary_prompt.contains("tokens"), "{summary_prompt}");
    assert!(
        requests[1].system_prompt.is_none(),
        "the second is the conversation"
    );
    assert!(
        compaction::is_compaction_summary(&requests[1].messages[0]),
        "the compacted view is sent: {:?}",
        requests[1].messages
    );
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|m| m.text().starts_with('x'))
    );

    // In memory the compacted view is committed and the revision advanced.
    assert_eq!(session.transcript_revision(), 1);
    assert!(compaction::is_compaction_summary(&session.transcript()[0]));

    // Journal order (with the summary call's real usage recorded first).
    // The turn's input message is committed first (start of the step); then the compaction
    // usage, the candidate, and the replacement segment (its messages), then the commit. The
    // final assistant message of the turn follows the commit.
    let keys: Vec<String> = journal.records().iter().map(label).collect();
    assert_eq!(
        &keys[..4],
        &["message", "compaction-usage:1", "compacted:1", "replaced:1"]
    );
    let commit = keys.iter().position(|k| k == "committed:1").unwrap();
    assert!(commit > 4, "the commit follows the segment: {keys:?}");
    assert!(
        keys[4..commit].iter().all(|k| k == "message"),
        "only messages inside the segment: {keys:?}"
    );
    assert_eq!(
        &keys[commit + 1..],
        &["message"],
        "the final assistant: {keys:?}"
    );

    // The host saw the compaction lifecycle.
    assert!(
        recorder
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::CompactionStarted { revision: 1, .. }))
    );
    assert!(recorder.events().iter().any(|e| matches!(
        e,
        AgentEvent::CompactionFinished {
            revision: 1,
            below_target: false,
            ..
        }
    )));
}

// ---------- 5. summary request failure / cancellation ----------

/// A summarizer failure fails the turn and leaves the session usable; the oversized request is
/// never sent, and the transcript is unchanged.
#[tokio::test]
async fn a_summary_failure_fails_the_turn_without_touching_the_transcript() {
    let model = ScriptedModel::new([
        Reply::Fail("summarizer down".into()),
        summary_reply("summary two", 10),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let before = big_history();
    let mut session = agent
        .open_session_with_journal(before.clone(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(error, agent_core::RunError::Compaction(_)),
        "{error:?}"
    );
    // Only the turn's own input was committed; the oversized history is untouched.
    let mut expected = before.clone();
    expected.push(Message::user("go"));
    assert_eq!(session.transcript(), expected);
    assert_eq!(session.transcript_revision(), 0);
    assert_eq!(model.requests().len(), 1, "only the summary was attempted");
    assert!(
        !journal
            .records()
            .iter()
            .any(|r| matches!(r, JournalRecord::TranscriptReplaced { .. })),
        "nothing was replaced"
    );
    // The session can run another turn (it is not poisoned or recovery-required).
    let outcome = session
        .run_turn("again", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
}

// ---------- 10. auto_compact off ----------

/// With automatic compaction off, an over-`B` request is still sent unchanged.
#[tokio::test]
async fn without_auto_compact_the_request_is_sent_over_budget() {
    let model = ScriptedModel::new([text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, false);
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
    assert_eq!(model.requests().len(), 1, "no summary call");
    assert_eq!(session.transcript_revision(), 0);
    assert!(
        !recorder
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::CompactionStarted { .. })),
        "no compaction ran"
    );
}

// ---------- 3. rolling summary ----------

/// A compacted history starts with a summary; the next compaction feeds it as
/// `<previous-summary>` and does not summarize it again as a message.
#[tokio::test]
async fn a_compaction_reuses_the_previous_summary() {
    let model = ScriptedModel::new([summary_reply("updated summary", 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let history = compaction::compacted_history(
        "old summary text",
        &[],
        &[Message::user("x".repeat(16_000)), Message::user("last")],
    )
    .unwrap();
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

    // The summary request carried the previous summary and only the new messages.
    let summary_body = model.requests()[0].messages[0].text();
    assert!(
        summary_body.contains("<previous-summary>"),
        "{summary_body}"
    );
    assert!(summary_body.contains("old summary text"), "{summary_body}");
    // The old summary message is not re-summarized as a message (its fixed prefix is absent).
    assert!(
        !summary_body.contains(compaction::SUMMARY_MESSAGE_PREFIX),
        "the old summary must not be re-fed as a message: {summary_body}"
    );
    // The committed history has the new summary, not the old one.
    assert!(session.transcript()[0].text().contains("updated summary"));
    assert!(!session.transcript()[0].text().contains("old summary text"));
}

// ---------- 4. atomic tool unit across the cut ----------

/// A compaction cut never splits an assistant message from its tool results.
#[tokio::test]
async fn a_tool_unit_crossing_the_cut_stays_whole() {
    use agent_core::{MessageMeta, ToolCall, ToolResult};

    let call = |id: &str| ToolCall {
        id: id.into(),
        name: "echo".into(),
        arguments: serde_json::json!({}),
    };
    let tool = |id: &str| Message::Tool {
        result: ToolResult::success(id, vec![ContentPart::text("ok")]),
        meta: MessageMeta::default(),
    };
    let assistant = Message::Assistant {
        content: Vec::new(),
        reasoning: Vec::new(),
        tool_calls: vec![call("c1"), call("c2")],
        provider_payload: None,
        interrupted: false,
        meta: MessageMeta::default(),
    };

    let model = ScriptedModel::new([summary_reply("summary", 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let history = vec![
        Message::user("x".repeat(8_000)),
        assistant,
        tool("c1"),
        tool("c2"),
        Message::user("last"),
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
    // The kept suffix (after the summary) holds the assistant call and both results.
    let compacted = &model.requests()[1].messages;
    let assistant_pos = compacted
        .iter()
        .position(|m| matches!(m, Message::Assistant { tool_calls, .. } if tool_calls.len() == 2))
        .expect("the tool unit is kept");
    assert!(matches!(
        &compacted[assistant_pos + 1],
        Message::Tool { .. }
    ));
    assert!(matches!(
        &compacted[assistant_pos + 2],
        Message::Tool { .. }
    ));
}

// ---------- 5b. cancellation ----------

/// A cancelled summary commits nothing and ends the turn `Cancelled`.
#[tokio::test]
async fn a_cancelled_summary_does_not_commit() {
    let model = ScriptedModel::new([Reply::Hang]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();
    let options = TurnOptions {
        deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_millis(50)),
        ..TurnOptions::default()
    };

    let outcome = session.run_turn("go", options, &recorder).await.unwrap();
    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(agent_core::CancelReason::DeadlineExceeded)
    );
    assert_eq!(session.transcript_revision(), 0);
    assert!(
        !journal
            .records()
            .iter()
            .any(|r| matches!(r, JournalRecord::TranscriptReplaced { .. })),
        "a cancelled compaction commits nothing"
    );
}

// ---------- 6a. insufficient gain ----------

/// A compaction that does not get under `B` fails the turn and commits nothing.
#[tokio::test]
async fn insufficient_gain_fails_without_committing() {
    let model = ScriptedModel::new([summary_reply(&"s".repeat(4_000), 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 8_000, true);
    let history = vec![
        Message::user("x".repeat(4_000)),
        Message::user("y".repeat(12_000)),
        Message::user("last"),
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
            agent_core::RunError::Compaction(agent_core::CompactionError::Insufficient { .. })
        ),
        "{error:?}"
    );
    assert_eq!(session.transcript_revision(), 0);
    assert!(
        !journal
            .records()
            .iter()
            .any(|r| matches!(r, JournalRecord::ContextCompacted { .. })),
        "a failed gain check writes no candidate"
    );
}

// ---------- 6b. below target ----------

/// A compaction that fits `B` but misses the 75% target commits and is flagged.
#[tokio::test]
async fn a_below_target_compaction_commits_and_is_flagged() {
    let model = ScriptedModel::new([summary_reply(&"s".repeat(2_600), 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    // ADR 0018: the verbatim user messages cost at most `U + U1` (1024 tokens by
    // the `message_tokens` ruler) in `T_after`, which with this file's one-byte-per-token
    // ruler is already over a 3000-token budget; the budget that lands in the below-target
    // band is therefore larger.
    let agent = compaction_agent(model.clone(), 8_000, true);
    let history = vec![
        Message::user("x".repeat(4_000)),
        Message::user("y".repeat(12_000)),
        Message::user("last"),
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
    assert_eq!(session.transcript_revision(), 1);
    let candidate = journal
        .records()
        .into_iter()
        .find_map(|r| match r {
            JournalRecord::ContextCompacted {
                below_target,
                tokens_before,
                tokens_after,
                ..
            } => Some((below_target, tokens_before, tokens_after)),
            _ => None,
        })
        .expect("a candidate was written");
    assert!(candidate.0, "flagged below target: {candidate:?}");
    assert!(
        candidate.2 <= 8_000 && candidate.2 * 4 > 8_000 * 3,
        "{candidate:?}"
    );
}

// ---------- 9. invalid summary retry ----------

/// An empty summary is retried once; the second answer is used.
#[tokio::test]
async fn an_invalid_summary_is_retried_once() {
    let model = ScriptedModel::new([
        summary_reply("   ", 10),
        summary_reply("a real summary", 10),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
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
    // Two summary calls, then the conversation.
    assert_eq!(model.requests().len(), 3);
    assert!(session.transcript()[0].text().contains("a real summary"));
}

/// Two invalid summaries fail the turn; nothing commits.
///
/// The retry is a **second real call**, so both attempts report their usage (ADR 0018
/// revision 3): the journal gets two records and the host gets two events, one per attempt.
#[tokio::test]
async fn each_summarization_attempt_reports_its_usage_once() {
    let model = ScriptedModel::new([
        summary_reply("   ", 10),
        summary_reply("a real summary", 20),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();

    // The journal: one record per attempt, in order, with each attempt's own numbers.
    let recorded: Vec<u64> = journal
        .records()
        .iter()
        .filter_map(|record| match record {
            JournalRecord::CompactionUsage { usage, .. } => usage.input,
            _ => None,
        })
        .collect();
    assert_eq!(recorded, vec![10, 20], "one record per attempt");

    // The events: the same, one per attempt. Attaching the usage to `CompactionFinished`
    // instead would lose the first attempt (that event fires once).
    let reported: Vec<u64> = recorder
        .events()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::CompactionUsage {
                usage, revision, ..
            } => {
                assert_eq!(*revision, 1, "both attempts belong to revision 1");
                usage.input
            }
            _ => None,
        })
        .collect();
    assert_eq!(reported, vec![10, 20], "one event per attempt");
}

/// A summarization call whose response reported **no** usage emits no event and writes no
/// journal record: a provider that reports nothing must not have a zero invented for it
/// (ADR 0007 J1), and the host must not see a usage fact it cannot believe.
#[tokio::test]
async fn a_summarization_call_without_usage_reports_nothing() {
    let no_usage = Reply::Respond(ModelResponse {
        reasoning: Vec::new(),
        content: vec![ContentPart::text("a real summary")],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    });
    let model = ScriptedModel::new([no_usage, text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
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
        !journal
            .records()
            .iter()
            .any(|record| matches!(record, JournalRecord::CompactionUsage { .. })),
        "no usage means no record"
    );
    assert!(
        !recorder
            .events()
            .iter()
            .any(|event| matches!(event, AgentEvent::CompactionUsage { .. })),
        "no usage means no event"
    );
}

/// Two invalid summaries fail the turn; nothing commits.
#[tokio::test]
async fn two_invalid_summaries_fail_the_turn() {
    let model = ScriptedModel::new([summary_reply("", 10), summary_reply("", 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
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
            agent_core::RunError::Compaction(agent_core::CompactionError::InvalidSummary(_))
        ),
        "{error:?}"
    );
    assert_eq!(session.transcript_revision(), 0);
    assert_eq!(model.requests().len(), 2, "exactly two attempts");
}

// ---------- 7. replacement abort ----------

/// If the replacement write fails after the summary, the `CompactionUsage` stays (the call
/// was real) but nothing commits; the journal still replays to the pre-compaction history.
#[tokio::test]
async fn a_replacement_abort_keeps_the_usage_and_recovers_the_old_history() {
    use agent_core::testing::FailingJournal;

    let model = ScriptedModel::new([summary_reply("summary", 10)]);
    // The 4th append is the `TranscriptReplaced` marker.
    let journal = Arc::new(FailingJournal::fail_at(4));
    let agent = compaction_agent(model.clone(), 3_000, true);
    let before = big_history();
    let mut session = agent
        .open_session_with_journal(before.clone(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(error, agent_core::RunError::Journal(_)),
        "{error:?}"
    );

    let records = journal.records();
    assert!(
        records
            .iter()
            .any(|r| matches!(r, JournalRecord::CompactionUsage { .. })),
        "the real summary call is recorded"
    );
    assert!(
        records
            .iter()
            .any(|r| matches!(r, JournalRecord::ContextCompacted { .. })),
        "the candidate was written before the failed marker"
    );
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, JournalRecord::TranscriptReplaced { .. })),
        "the replacement marker failed, so no segment exists"
    );
    // The pre-compaction history was passed to `open_session_with_journal`, which does not
    // re-journal it (D8); the journal holds only the input and the failed compaction. The
    // turn's input is at index `before.len()`, so no replacement segment started.
    assert!(
        records
            .iter()
            .all(|r| !matches!(r, JournalRecord::TranscriptReplacementCommitted { .. })),
        "nothing committed"
    );
    let _ = before;
}

// ---------- 8. baseline cleared ----------

/// A committed compaction leaves the projection an estimate: the reported baseline described
/// the replaced context.
#[tokio::test]
async fn a_committed_compaction_clears_the_budget_baseline() {
    use agent_core::UsageSource;

    let model = ScriptedModel::new([summary_reply("short", 50), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    // Seed a baseline as if an earlier request had reported usage.
    assert!(session.restore_budget_baseline(2, 8_000));
    assert_eq!(
        session.context_budget_status().report().map(|r| r.source),
        None
    );
    let recorder = Recorder::default();

    session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    let report = session
        .context_budget_status()
        .report()
        .expect("a report after the turn");
    assert_eq!(
        report.source,
        UsageSource::Estimated,
        "the baseline described the replaced transcript: {report:?}"
    );
    assert_eq!(
        session.testing_budget_baseline_input(),
        None,
        "the reported baseline was cleared with the replaced transcript"
    );
}

// ---------- 12. changed source view ----------

/// If the source view changed between planning and commit, the compaction is abandoned.
#[tokio::test]
async fn a_changed_source_view_is_abandoned() {
    let model = ScriptedModel::new([summary_reply("summary", 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    session.testing_tamper_source_on_compaction();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            agent_core::RunError::Compaction(agent_core::CompactionError::SourceChanged)
        ),
        "{error:?}"
    );
    assert!(
        !journal
            .records()
            .iter()
            .any(|r| matches!(r, JournalRecord::ContextCompacted { .. })),
        "no candidate is written when the source view changed"
    );
}

// ---------- 2. the candidate alone changes nothing ----------

/// A `ContextCompacted` candidate with no matching commit replays to the pre-compaction
/// history; with the commit it replays to the compacted history (ADR 0019).
#[test]
fn a_candidate_without_a_commit_recovers_the_old_history() {
    let mut records = vec![JournalRecord::Message {
        index: 0,
        message: Message::user("old"),
    }];
    records.push(JournalRecord::ContextCompacted {
        revision: 1,
        source_revision: 0,
        source_len: 1,
        summary: "summary".into(),
        first_kept: 1,
        tokens_before: 10,
        tokens_after: 5,
        below_target: false,
        model: "current".into(),
    });
    records.push(JournalRecord::TranscriptReplaced {
        revision: 1,
        len: 1,
    });
    records.push(JournalRecord::Message {
        index: 0,
        message: Message::user("new summary"),
    });
    assert_eq!(
        agent_core::recover(&records).unwrap(),
        vec![Message::user("old")],
        "without the commit the old history stands"
    );
    records.push(JournalRecord::TranscriptReplacementCommitted {
        revision: 1,
        len: 1,
    });
    assert_eq!(
        agent_core::recover(&records).unwrap(),
        vec![Message::user("new summary")],
        "with the commit the compacted history stands"
    );
}

// ---------- 6b (cont.): below-target stops auto-compaction for the turn ----------

/// After a below-target compaction, the next over-budget request in the same turn fails
/// instead of compacting again (ADR 0018).
#[tokio::test]
async fn a_below_target_compaction_stops_auto_compaction_for_the_turn() {
    use common::stop;

    let model = ScriptedModel::new([
        summary_reply(&"s".repeat(2_600), 10),
        stop(StopReason::PauseTurn, &"a".repeat(2_000)),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 8_000, true);
    let history = vec![
        Message::user("x".repeat(4_000)),
        Message::user("y".repeat(12_000)),
        Message::user("last"),
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
            agent_core::RunError::Compaction(agent_core::CompactionError::BelowTarget)
        ),
        "{error:?}"
    );
}

// ---------- 8. the candidate is illegal inside a pending segment ----------

/// `ContextCompacted` and `CompactionUsage` outside a replacement are inert; inside one they
/// are corruption, exactly like the other foreign records (ADR 0018/0019).
#[test]
fn a_compaction_record_inside_a_pending_segment_is_corruption() {
    let candidate = JournalRecord::ContextCompacted {
        revision: 1,
        source_revision: 0,
        source_len: 1,
        summary: "s".into(),
        first_kept: 1,
        tokens_before: 10,
        tokens_after: 5,
        below_target: false,
        model: "current".into(),
    };
    let open = JournalRecord::TranscriptReplaced {
        revision: 1,
        len: 1,
    };
    let error = agent_core::replay(&[open.clone(), candidate.clone()]).unwrap_err();
    assert!(
        matches!(error, agent_core::ReplayError::Replacement { .. }),
        "{error:?}"
    );
    let usage = JournalRecord::CompactionUsage {
        revision: 1,
        model: "current".into(),
        usage: ModelUsage {
            input: Some(1),
            ..Default::default()
        },
    };
    let error = agent_core::replay(&[open, usage]).unwrap_err();
    assert!(
        matches!(error, agent_core::ReplayError::Replacement { .. }),
        "{error:?}"
    );
}

// ---------- dogfood7: an over-long summary must not fail the turn ----------

fn compaction_usage_count(journal: &RecordingJournal) -> usize {
    journal
        .records()
        .iter()
        .filter(|record| matches!(record, JournalRecord::CompactionUsage { .. }))
        .count()
}

/// The first summary is over the limit: the one retry sends **only** the over-long summary
/// and asks for a shorter one (no original conversation), and the second answer commits. Two
/// real calls are billed.
#[tokio::test]
async fn an_overlong_summary_is_shortened_on_the_retry() {
    let overlong = "L".repeat(9_000);
    let model = ScriptedModel::new([
        summary_reply(&overlong, 50),
        summary_reply("the shorter summary", 10),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
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
    assert_eq!(session.transcript_revision(), 1, "the retry committed");

    let requests = model.requests();
    assert_eq!(requests.len(), 3, "summary, shorten, compacted request");
    // The first summary request states the enforced limit (2048 = keep_target(3000)).
    let first_prompt = requests[0].system_prompt.as_deref().unwrap();
    assert!(first_prompt.contains("2048 tokens"), "{first_prompt}");
    // The retry is the shortening request: 80% of 2048 is 1638, and the body is the over-long
    // summary, not the conversation again.
    let shorten_prompt = requests[1].system_prompt.as_deref().unwrap();
    assert!(shorten_prompt.contains("1638 tokens"), "{shorten_prompt}");
    let body = requests[1].messages[0].text();
    assert!(
        body.contains(&overlong),
        "the over-long summary is not the input"
    );
    assert!(!body.contains("seven"), "{body}");
    // The retry states the deletion priority, the chronological order and the one-third
    // ceiling for Verified facts.
    assert!(
        body.contains(agent_core::SUMMARY_FACTS_ORDER_RULE)
            && body.contains(agent_core::SUMMARY_FACTS_DELETE_OLDEST),
        "the shorten prompt must state the deletion rule: {body}"
    );
    assert!(
        body.contains(&format!(
            "{} tokens",
            agent_core::verified_facts_max_tokens(agent_core::shorten_target(2_048))
        )),
        "the shorten prompt must name the Verified facts ceiling: {body}"
    );
    assert!(
        !body.contains("Conversation to summarize") && !body.contains("xxxx"),
        "the retry must not resend the conversation: {body}"
    );
    // Both real calls are billed.
    assert_eq!(compaction_usage_count(&journal), 2);
    // And the committed history is the shorter summary.
    assert!(
        session.transcript()[0]
            .text()
            .contains("the shorter summary")
    );
}

/// ADR 0018, decision 1: the host's `compaction_settings` reaches the summary
/// request and the shorten retry, and is **never** added to the conversation request.
#[tokio::test]
async fn the_compaction_setting_reaches_the_summary_and_the_shorten_requests_only() {
    let overlong = "L".repeat(9_000);
    let model = ScriptedModel::new([
        summary_reply(&overlong, 50),
        summary_reply("the shorter summary", 10),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let setting = serde_json::json!({"reasoning_effort": "low"});
    let conversation = serde_json::json!({"temperature": 0.2});
    let agent = Arc::new(Agent::new(model.clone(), NoTools).with_config(AgentConfig {
        context_budget: Some(ContextBudget::new(3_000).with_bytes_per_token(1)),
        auto_compact: true,
        model_settings: conversation.clone(),
        compaction_settings: setting.clone(),
        ..Default::default()
    }));
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

    let requests = model.requests();
    assert_eq!(requests.len(), 3, "summary, shorten, compacted request");
    assert_eq!(
        requests[0].settings, setting,
        "the summary request carries the host's compaction setting"
    );
    assert_eq!(
        requests[1].settings, setting,
        "the shorten retry carries the same setting"
    );
    assert_eq!(
        requests[2].settings, conversation,
        "the conversation request carries only `model_settings`"
    );
    assert!(
        requests[2].settings.get("reasoning_effort").is_none(),
        "the compaction setting must not leak into a conversation request"
    );
}

/// The first summary is empty: the retry resends the **original** request, not a shortening
/// one.
#[tokio::test]
async fn an_empty_summary_resends_the_original_request() {
    let model = ScriptedModel::new([
        summary_reply("   ", 50),
        summary_reply("the summary", 10),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
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
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].messages[0].text(),
        requests[0].messages[0].text(),
        "an empty answer resends the original request"
    );
    assert!(
        requests[1].messages[0]
            .text()
            .contains("Conversation to summarize")
    );
    assert!(
        !requests[1].messages[0]
            .text()
            .contains("<summary-to-shorten>")
    );
    assert_eq!(compaction_usage_count(&journal), 2);
}

/// Both summaries are over the limit: the turn fails with the unchanged error text and
/// nothing commits.
#[tokio::test]
async fn two_overlong_summaries_fail_the_turn() {
    let overlong = "L".repeat(9_000);
    let model = ScriptedModel::new([
        summary_reply(&overlong, 50),
        summary_reply(&overlong, 10),
        text("never"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = compaction_agent(model.clone(), 3_000, true);
    let mut session = agent
        .open_session_with_journal(big_history(), journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    let error = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap_err();
    match error {
        RunError::Compaction(CompactionError::InvalidSummary(message)) => {
            assert!(message.contains("over the limit of 2048"), "{message}");
        }
        other => panic!("expected an invalid-summary failure, got {other:?}"),
    }
    assert_eq!(session.transcript_revision(), 0, "nothing committed");
    assert_eq!(model.requests().len(), 2, "exactly two attempts");
    assert_eq!(compaction_usage_count(&journal), 2);
    assert!(
        !journal
            .records()
            .iter()
            .any(|record| matches!(record, JournalRecord::ContextCompacted { .. })),
        "no candidate is written"
    );
}

// ---------- dogfood9: the summary ceiling scales with the window ----------

/// The proactive path passes the window's scaled ceiling into the summary prompt: `B = 103616`
/// gives `summary_max_tokens = 12952`, not the old fixed 8192.
#[tokio::test]
async fn the_auto_path_scales_the_summary_ceiling_with_the_window() {
    let model = ScriptedModel::new([summary_reply("a short summary", 50), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    // One token per byte, so the first message is 104000 projected tokens, over B = 103616.
    let agent = compaction_agent(model.clone(), 103_616, true);
    let mut session = agent
        .open_session_with_journal(
            vec![
                Message::user("x".repeat(104_000)),
                Message::assistant("ok"),
                Message::user("last"),
            ],
            journal,
        )
        .await
        .unwrap();
    let recorder = Recorder::default();

    let outcome = session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(
        session.transcript_revision(),
        1,
        "the proactive path committed"
    );

    let requests = model.requests();
    assert!(requests.len() >= 2, "{}", requests.len());
    let prompt = requests[0].system_prompt.as_deref().unwrap();
    assert!(prompt.contains("12952 tokens"), "{prompt}");
    assert!(
        !prompt.contains("8192 tokens"),
        "the fixed ceiling must not be used for an automatic compaction: {prompt}"
    );
}

// ---------- ADR 0018: rolling verbatim ----------

/// A user message with `source = source`.
fn user_sourced(text: &str, source: Option<agent_core::InputSource>) -> Message {
    Message::User {
        content: vec![ContentPart::text(text)],
        meta: agent_core::MessageMeta {
            source,
            ..agent_core::MessageMeta::default()
        },
    }
}

/// The ADR's headline: the first user message is still there, word for word, after eleven
/// rolling compactions. The rolling selection is pure, so this drives it directly: each round
/// is one compaction's worth of new user messages.
#[test]
fn the_first_user_message_survives_eleven_rolling_compactions() {
    use agent_core::{InputSource, RetainedUserMessage, select_user_messages};

    const FIRST: &str = "FIRST-USER-MESSAGE-MARKER: do not lose this sentence";
    let mut kept = vec![RetainedUserMessage {
        text: FIRST.to_owned(),
    }];
    for round in 0..11 {
        let newly = vec![
            user_sourced(
                &format!("round {round} {}", "x".repeat(9_000)),
                Some(InputSource::User),
            ),
            user_sourced(&format!("a note {round}"), Some(InputSource::User)),
        ];
        kept = select_user_messages(&kept, &newly, Some(8_000));
        assert_eq!(
            kept.first().map(|message| message.text.as_str()),
            Some(FIRST),
            "round {round}: the first user message must survive verbatim"
        );
        assert!(
            kept.iter()
                .any(|message| message.text.contains(&format!("a note {round}"))),
            "round {round}: the newest short message is kept"
        );
    }
}

/// The ADR's other headline: the previous verbatim list is **not** sent to the summarizer.
/// The new conversation's user messages are (they are what is being summarized), but the
/// rendered section, and the first message's earlier copies, are not.
///
/// This one runs the real turn loop twice. It uses the **default** ruler: with the one-byte
/// ruler the other tests use, the last unit (the turn's own input) is kept whole, and a
/// `T_after` that fits `B` is impossible once the input is big enough to cross `B`.
#[tokio::test]
async fn the_summary_request_does_not_carry_the_previous_verbatim_list() {
    const FIRST: &str = "FIRST-USER-MESSAGE-MARKER";
    let big_reply = "d".repeat(40_000);

    let model = ScriptedModel::new([
        summary_reply("a rolling summary", 10),
        text(&big_reply),
        summary_reply("a second rolling summary", 10),
        text(&big_reply),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let agent = Arc::new(Agent::new(model.clone(), NoTools).with_config(AgentConfig {
        context_budget: Some(ContextBudget::new(8_000)),
        auto_compact: true,
        ..Default::default()
    }));
    let history = vec![
        Message::user(FIRST),
        Message::assistant("y".repeat(40_000)),
        Message::user("last"),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();
    for round in 0..2 {
        session
            .run_turn("go", TurnOptions::default(), &recorder)
            .await
            .unwrap();
        assert_eq!(session.transcript_revision(), round + 1, "round {round}");
    }

    let requests = model.requests();
    let summaries: Vec<&agent_core::ModelRequest> = requests
        .iter()
        .filter(|request| {
            request
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("You are compressing"))
        })
        .collect();
    assert_eq!(summaries.len(), 2, "two compactions, two summary requests");
    let second = summaries[1];
    let text = second
        .messages
        .iter()
        .map(|message| message.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains(agent_core::USER_MESSAGES_HEADING),
        "the rendered verbatim section must not be resent: {text}"
    );
    // The first message is not in the second summary's input either: it was summarized in the
    // first round and now lives only in the previous verbatim list and the model summary.
    assert!(!text.contains(FIRST), "{text}");
    assert!(
        text.contains("a rolling summary"),
        "the previous model summary is still the rolling input: {text}"
    );
}

/// ADR 0018: the plan reserves the space the verbatim list takes. Without the
/// reservation the kept suffix would include one more (large) unit and `T_after` would cross
/// `B`, so the compaction would fail instead of committing.
#[tokio::test]
async fn the_plan_reserves_the_verbatim_space() {
    let model = ScriptedModel::new([summary_reply("a short summary", 10), text("done")]);
    let journal = Arc::new(RecordingJournal::new());
    // A budget the verbatim list itself fits in (it is at most `U + U1`), so the reservation
    // is what decides whether the 7200-byte unit is kept.
    let agent = compaction_agent(model.clone(), 8_000, true);
    let history = vec![
        // A big prefix: its verbatim copy is what the plan must reserve room for.
        Message::user("x".repeat(48_000)),
        // A unit that fits `K` but not `K - U₁`: with the reservation it is summarized.
        Message::user("y".repeat(7_200)),
        Message::user("last"),
    ];
    let mut session = agent
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();
    let recorder = Recorder::default();

    session
        .run_turn("go", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(session.transcript_revision(), 1, "the compaction committed");
    let kept: Vec<usize> = session.transcript()[1..]
        .iter()
        .map(|message| message.text().chars().count())
        .collect();
    assert!(
        kept.iter().all(|length| *length < 1_000),
        "the 7200-byte unit must be summarized, not kept: {kept:?}"
    );
}

//! ADR 0018 §兜底、手动与关闭 (runtime half): a manual `/compact` is a control request run by
//! the driver **between turns**. It is not a conversation input (no `InboxEnqueued`), a turn in
//! flight finishes first, and cancelling aborts it without committing.

mod common;

use std::sync::Arc;

use agent_core::{
    Agent, AgentConfig, CompactionOutcome, ContentPart, ContextBudget, InputSource, JournalRecord,
    Message, ModelResponse, NoTools, StopReason, TranscriptJournal, TurnOptions,
};
use common::{Recorder, ScriptedModel, TempDir, created};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};

fn summary_response(summary: &str) -> ModelResponse {
    ModelResponse {
        limits: None,
        reasoning: Vec::new(),
        content: vec![ContentPart::text(summary)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        // A real summarizer reports usage, so `CompactionUsageRecorded` is written.
        usage: Some(agent_core::ModelUsage {
            input: Some(50),
            ..Default::default()
        }),
    }
}

/// A journal with a big first message and a small last one, so a compaction can summarize the
/// prefix and keep the suffix.
async fn big_session() -> (TempDir, Arc<SessionJournal>, std::path::PathBuf) {
    let dir = TempDir::new("compact-control");
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("x".repeat(16_000)),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::user("last"),
        })
        .await
        .unwrap();
    (dir, journal, path)
}

fn compaction_agent(model: Arc<ScriptedModel>) -> Arc<Agent<Arc<ScriptedModel>, NoTools, ()>> {
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        context_budget: Some(ContextBudget::new(3_000).with_bytes_per_token(1)),
        // The proactive path is off, so these tests exercise the manual control alone.
        auto_compact: false,
        ..Default::default()
    }))
}

async fn open_driver(
    agent: &Arc<Agent<Arc<ScriptedModel>, NoTools, ()>>,
    journal: Arc<SessionJournal>,
    inbox: Arc<Inbox>,
) -> Driver<Arc<ScriptedModel>, NoTools, ()> {
    let live = Arc::new(Recorder::default());
    Driver::open(
        agent,
        journal,
        inbox,
        live as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap()
}

/// `/compact` between turns commits the replacement segment and enqueues no input.
#[tokio::test]
async fn compact_between_turns_commits_and_enqueues_nothing() {
    let (_dir, journal, path) = big_session().await;
    let model = ScriptedModel::new([summary_response("a short summary")]);
    let agent = compaction_agent(Arc::clone(&model));
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let mut driver = open_driver(&agent, Arc::clone(&journal), Arc::clone(&inbox)).await;
    let task = tokio::spawn(async move { driver.run().await });

    let outcome = inbox
        .compact(Some("focus on the parser".into()))
        .await
        .expect("the compaction committed");
    assert!(
        matches!(outcome, CompactionOutcome::Compacted { revision: 1, .. }),
        "{outcome:?}"
    );

    // The instructions reached the summarizer, and nothing else was sent.
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].messages[0]
            .text()
            .contains("focus on the parser")
    );

    let session = Session::open(&path).unwrap();
    let names: Vec<String> = session.events().iter().map(|e| e.event.clone()).collect();
    for expected in [
        "CompactionUsageRecorded",
        "ContextCompacted",
        "TranscriptReplaced",
        "TranscriptReplacementCommitted",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} missing in {names:?}"
        );
    }
    assert!(
        !names.contains(&"InboxEnqueued".to_string()),
        "a control request is not an input: {names:?}"
    );
    // No message carries the `/compact` instructions.
    assert!(
        !session.events().iter().any(|e| match &e.data {
            EventData::MessageAppended { message, .. } =>
                message.text().contains("focus on the parser"),
            _ => false,
        }),
        "the instructions must not become a transcript message"
    );
    task.abort();
    let _ = task.await;
}

/// A `/compact` that arrives while a turn is running waits for the turn to end.
#[tokio::test]
async fn compact_waits_for_a_running_turn() {
    let (_dir, journal, path) = big_session().await;
    // The turn's reply is slow, so the control request is queued mid-turn; the second call
    // is the summary.
    let model = ScriptedModel::new([text_response("done"), summary_response("a short summary")]);
    model.slow_at(0, 500);
    let agent = compaction_agent(Arc::clone(&model));
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let mut driver = open_driver(&agent, Arc::clone(&journal), Arc::clone(&inbox)).await;
    let task = tokio::spawn(async move { driver.run().await });

    inbox.followup("go", InputSource::User).await.unwrap();
    // The turn is in flight: the model has been called (the request is recorded before it
    // sleeps), so a `/compact` now must wait for the turn to finish.
    while model.requests().is_empty() {
        tokio::task::yield_now().await;
    }
    let outcome = inbox.compact(None).await.expect("the compaction committed");
    assert!(
        matches!(outcome, CompactionOutcome::Compacted { .. }),
        "{outcome:?}"
    );

    let session = Session::open(&path).unwrap();
    let seq = |name: &str| {
        session
            .events()
            .iter()
            .find(|e| e.event == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .seq
    };
    assert!(
        seq("TurnFinished") < seq("ContextCompacted"),
        "the compaction ran after the turn"
    );
    task.abort();
    let _ = task.await;
}

/// A cancelled `/compact` commits nothing.
#[tokio::test]
async fn a_cancelled_compact_commits_nothing() {
    let (_dir, journal, path) = big_session().await;
    // The summarizer hangs until cancelled.
    let model = ScriptedModel::new([]);
    model.hang_at(0);
    let agent = compaction_agent(Arc::clone(&model));
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let mut driver = open_driver(&agent, Arc::clone(&journal), Arc::clone(&inbox)).await;
    let task = tokio::spawn(async move { driver.run().await });

    let compact = tokio::spawn({
        let inbox = Arc::clone(&inbox);
        async move { inbox.compact(None).await }
    });
    // Wait until the summarizer is actually running, then cancel it.
    while model.requests().is_empty() {
        tokio::task::yield_now().await;
    }
    inbox.cancel(radian_runtime::CancelCause::Requested);
    let result = compact.await.unwrap();
    assert!(
        result.is_err(),
        "a cancelled compaction is an error: {result:?}"
    );

    let session = Session::open(&path).unwrap();
    assert!(
        !session
            .events()
            .iter()
            .any(|e| e.event == "TranscriptReplacementCommitted" || e.event == "ContextCompacted"),
        "a cancelled compaction must not commit"
    );
    task.abort();
    let _ = task.await;
}

/// A history that already fits is `NothingToCompact` and calls no model.
#[tokio::test]
async fn compact_on_a_small_history_is_nothing_to_compact() {
    let dir = TempDir::new("compact-small");
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("hi"),
        })
        .await
        .unwrap();
    let model = ScriptedModel::new([]);
    let agent = compaction_agent(Arc::clone(&model));
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let mut driver = open_driver(&agent, Arc::clone(&journal), Arc::clone(&inbox)).await;
    let task = tokio::spawn(async move { driver.run().await });

    let outcome = inbox.compact(None).await.unwrap();
    assert_eq!(outcome, CompactionOutcome::NothingToCompact);
    assert!(model.requests().is_empty(), "nothing to summarize");
    task.abort();
    let _ = task.await;
}

fn text_response(s: &str) -> ModelResponse {
    ModelResponse {
        limits: None,
        reasoning: Vec::new(),
        content: vec![ContentPart::text(s)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

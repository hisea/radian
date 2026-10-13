//! ADR 0018 §A + ADR 0011: the turn loop must not write to the journal while a core
//! replacement segment is open. The wake branch only handles cancellation; steering is
//! drained after `Step::Continue`, where the core reads it anyway.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    Agent, AgentConfig, ContentPart, ContextBudget, InputSource, JournalRecord, Message,
    ModelResponse, StopReason, TranscriptJournal, TurnOptions,
};
use common::{Recorder, ScriptedModel, TempDir, created, text};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};

fn summary_response(summary: &str) -> ModelResponse {
    ModelResponse {
        reasoning: Vec::new(),
        content: vec![ContentPart::text(summary)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

fn pause_turn_response() -> ModelResponse {
    ModelResponse {
        reasoning: Vec::new(),
        content: vec![ContentPart::text("thinking")],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::PauseTurn,
        usage: None,
    }
}

/// A steer that arrives while a replacement segment is open must not deadlock the driver:
/// the wake branch does not write to the journal, so the step finishes the segment and the
/// steer is claimed after the commit.
#[tokio::test]
async fn a_steer_during_an_open_segment_does_not_deadlock_the_driver() {
    let dir = TempDir::new("gate-steer");
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    // A big prefix and a small suffix, so compaction has something to summarize.
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

    // Summary first, then a `PauseTurn` step (so steering is drained), then the final answer.
    let model = ScriptedModel::new([
        summary_response("short summary"),
        pause_turn_response(),
        text("done"),
    ]);
    let agent = Arc::new(
        Agent::new(Arc::clone(&model), agent_core::NoTools).with_config(AgentConfig {
            // ADR 0018: the verbatim user messages are part of `T_after`, and with
            // this test's one-byte-per-token ruler they cost four times their `message_tokens`
            // budget, so the budget has to leave room for them.
            context_budget: Some(ContextBudget::new(8_000).with_bytes_per_token(1)),
            auto_compact: true,
            ..Default::default()
        }),
    );
    // Non-durable inbox: `followup`/`steer` must not touch the journal themselves, or the
    // test's own enqueue would also block on the gate before the driver ever wakes.
    let inbox = Inbox::new(16);
    let live = Arc::new(Recorder::default());
    let mut driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&live) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();

    journal.pause_next_segment_append();
    let wake_branch = driver.testing_wake_branch();
    let task = tokio::spawn(async move { driver.run().await });

    inbox.followup("go", InputSource::User).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), journal.wait_segment_paused())
        .await
        .expect("the replacement segment must be reached");

    // Register before steering so the wake is never missed.
    let reached = wake_branch.notified();
    tokio::pin!(reached);
    reached.as_mut().enable();
    let steer_id = inbox.steer("hold", InputSource::User).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("the driver must enter the wake branch while the segment is open");

    // Let the step finish the segment. With the old wake branch (which drained steering and
    // therefore wrote `InputClaimed`), this is where the driver deadlocks.
    journal.release_segment_append();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(&path)
                .map(|text| text.matches("\"TurnFinished\"").count() >= 1)
                .unwrap_or(false)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the turn must finish (the old wake branch deadlocks here)");
    task.abort();
    let _ = task.await;

    let session = Session::open(&path).unwrap();
    let committed_seq = session
        .events()
        .iter()
        .find(|e| e.event == "TranscriptReplacementCommitted")
        .expect("the segment committed")
        .seq;
    let claimed = session
        .events()
        .iter()
        .find(|e| {
            matches!(&e.data, EventData::InputClaimed { message_ids, .. }
                if message_ids.contains(&steer_id))
        })
        .expect("the steer was claimed");
    assert!(
        claimed.seq > committed_seq,
        "the steer must be claimed after the commit: {} vs {committed_seq}",
        claimed.seq
    );
    // The turn's requests were all sent: summary, the paused step, and the steered step.
    assert_eq!(model.requests().len(), 3, "{:?}", model.requests().len());
}

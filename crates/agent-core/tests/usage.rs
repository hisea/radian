//! `ModelUsage`: serialization shape (AC-A5-6) and the journal/event/ordering behaviour
//! (AC-A5-7..10).

mod common;

use std::sync::Arc;

use agent_core::testing::{FailingJournal, RecordingJournal};
use agent_core::*;
use common::*;

// ---------- serialization (AC-A5-6) ----------

#[test]
fn default_usage_serializes_as_an_empty_object() {
    assert_eq!(serde_json::to_string(&ModelUsage::default()).unwrap(), "{}");
}

#[test]
fn partial_usage_serializes_only_its_fields() {
    let usage = ModelUsage {
        input: Some(5),
        total: Some(12),
        ..ModelUsage::default()
    };
    assert_eq!(
        serde_json::to_string(&usage).unwrap(),
        r#"{"input":5,"total":12}"#
    );
}

#[test]
fn details_round_trip_preserves_the_nested_object_verbatim() {
    let raw = r#"{"cached_tokens":7,"audio_tokens":{"prefill":0,"written":0}}"#;
    let usage = ModelUsage {
        input: Some(3),
        details: Some(OpaqueJson::parse(raw).unwrap()),
        ..ModelUsage::default()
    };
    let json = serde_json::to_string(&usage).unwrap();
    assert_eq!(
        json,
        r#"{"input":3,"details":{"cached_tokens":7,"audio_tokens":{"prefill":0,"written":0}}}"#
    );
    let back: ModelUsage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, usage);
    assert_eq!(back.details.unwrap().get(), raw);
}

#[test]
fn old_json_with_missing_fields_reads_as_none() {
    let usage: ModelUsage = serde_json::from_str(r#"{"total":3}"#).unwrap();
    assert_eq!(usage.total, Some(3));
    assert_eq!(usage.input, None);
    assert_eq!(usage.output, None);
    assert_eq!(usage.cache_read, None);
    assert_eq!(usage.cache_write, None);
    assert_eq!(usage.details, None);
}

// ---------- journal and events (AC-A5-7/8/9/10) ----------

fn usage_value() -> ModelUsage {
    ModelUsage {
        input: Some(8),
        output: Some(2),
        total: Some(10),
        cache_read: Some(1),
        cache_write: Some(0),
        details: Some(OpaqueJson::parse(r#"{"cached_tokens":1}"#).unwrap()),
    }
}

/// An assistant reply that carries usage.
fn reply_with_usage() -> Reply {
    Reply::Respond(ModelResponse {
        content: vec![ContentPart::text("used")],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: Some(usage_value()),
    })
}

#[derive(Default)]
struct AllEvents(std::sync::Mutex<Vec<AgentEvent>>);

impl EventSink for AllEvents {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl AllEvents {
    fn usage_of(&self, model_call: usize) -> Option<ModelUsage> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ModelResponseReceived { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .nth(model_call)
            .unwrap()
    }
}

/// Opens a session whose every change is journaled to `journal`.
async fn session_with(
    model: Arc<ScriptedModel>,
    journal: Arc<dyn TranscriptJournal>,
) -> AgentSession<Arc<ScriptedModel>, NoTools, ()> {
    Arc::new(Agent::new(model, NoTools))
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap()
}

#[tokio::test]
async fn usage_is_journaled_after_the_assistant_message_and_only_when_present() {
    let model = ScriptedModel::new([reply_with_usage(), text("plain")]);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with(model, journal.clone()).await;

    chat.run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let records = journal.records();
    assert_eq!(records.len(), 3);
    assert!(matches!(&records[0], JournalRecord::Message { .. }));
    assert!(matches!(&records[1], JournalRecord::Message { .. }));
    assert!(matches!(
        &records[2],
        JournalRecord::ModelUsage { usage } if *usage == usage_value()
    ));

    // A turn without usage writes no usage record (J5): no empty shells.
    chat.run_turn("again", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let records = journal.records();
    assert_eq!(records.len(), 5);
    assert!(matches!(&records[3], JournalRecord::Message { .. }));
    assert!(matches!(&records[4], JournalRecord::Message { .. }));
}

#[tokio::test]
async fn usage_lands_in_the_model_response_received_event() {
    let model = ScriptedModel::new([reply_with_usage(), text("plain")]);
    let mut chat = session_with(model, Arc::new(RecordingJournal::new())).await;
    let events = AllEvents::default();

    chat.run_turn("hi", TurnOptions::default(), &events)
        .await
        .unwrap();
    assert_eq!(events.usage_of(0), Some(usage_value()));

    chat.run_turn("again", TurnOptions::default(), &events)
        .await
        .unwrap();
    assert_eq!(events.usage_of(1), None);
}

#[tokio::test]
async fn replay_and_recover_ignore_usage_records() {
    let messages = [
        JournalRecord::Message {
            index: 0,
            message: Message::user("a"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::assistant("b"),
        },
    ];
    let mut interleaved: Vec<JournalRecord> = Vec::new();
    for (i, record) in messages.iter().enumerate() {
        interleaved.push(record.clone());
        interleaved.push(JournalRecord::ModelUsage {
            usage: usage_value(),
        });
        let _ = i;
    }

    let plain = replay(&messages).unwrap();
    let with_usage = replay(&interleaved).unwrap();
    assert_eq!(with_usage, plain, "usage records must not become messages");
    assert_eq!(
        recover(&interleaved).unwrap(),
        plain,
        "recover ignores them too"
    );
}

#[tokio::test]
async fn usage_append_failure_sweep() {
    // Appends of the two-turn script: turn 1 = user, assistant, usage; turn 2 = user,
    // assistant. Every position fails once.
    for nth in 1..=5 {
        let journal = Arc::new(FailingJournal::fail_at(nth));
        let model = ScriptedModel::new([reply_with_usage(), text("plain")]);
        let mut chat = session_with(model, journal.clone()).await;

        let first = chat.run_turn("hi", TurnOptions::default(), &NoopSink).await;
        // Turn 2 only starts when turn 1 survived its appends.
        let error = match first {
            Err(e) => e,
            Ok(_) => chat
                .run_turn("again", TurnOptions::default(), &NoopSink)
                .await
                .expect_err("the failure must land in one of the two turns"),
        };
        assert!(
            matches!(error, RunError::Journal(ref e) if e.0.contains("injected")),
            "append {nth}: {error:?}"
        );

        // Memory is exactly the journaled message prefix of the success baseline. In
        // particular when the usage append fails (nth == 3), the assistant message is
        // already in memory and journal: it succeeded first.
        let expected: Vec<Message> = journal
            .records()
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Message { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(chat.transcript(), expected, "append {nth}");
        assert_eq!(journal.records().len(), nth - 1, "append {nth}");

        assert!(matches!(
            chat.start_turn("no"),
            Err(SessionError::RecoveryRequired)
        ));
        chat.close().await.unwrap();

        // Every failure point leaves a journal that recovers into openable history.
        let history = recover(&journal.records()).unwrap();
        Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
            .open_session_with_history(history)
            .await
            .unwrap();
    }
}

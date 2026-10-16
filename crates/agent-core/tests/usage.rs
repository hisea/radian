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
    // A record written before ADR 0007 says nothing about the input's cache
    // semantics; `None` is "not marked", which is not "zero" and not "false".
    assert_eq!(usage.input_includes_cache, None);
}

// ---------- the input's cache semantics (ADR 0007) ----------

/// The two shapes side by side, on the research's own numbers (§3.2): a 200 000-token prompt
/// with 190 000 of it cached. `prompt_total()` is the whole request in both (the context
/// budget's question), `uncached_input()` is the part billed at the full input rate (the
/// fee's question) — and the cached tokens are in exactly one of the two terms.
#[test]
fn the_prompt_total_is_the_whole_input_and_the_uncached_part_is_billed_at_the_full_rate() {
    // OpenAI shape: `input` contains the cached part.
    let openai = ModelUsage {
        input: Some(200_000),
        output: Some(2_000),
        cache_read: Some(190_000),
        input_includes_cache: Some(true),
        ..ModelUsage::default()
    };
    assert_eq!(openai.prompt_total(), Some(200_000));
    assert_eq!(openai.uncached_input(), Some(10_000));

    // Anthropic shape: `input` is a sibling of the cache fields.
    let anthropic = ModelUsage {
        input: Some(10_000),
        output: Some(2_000),
        cache_read: Some(190_000),
        cache_write: Some(1_000),
        input_includes_cache: Some(false),
        ..ModelUsage::default()
    };
    assert_eq!(anthropic.prompt_total(), Some(201_000));
    assert_eq!(anthropic.uncached_input(), Some(10_000));
}

/// An old record (no mark) is taken at face value: `input` alone, because adding the cache
/// fields could double-count the very thing the flag exists to disambiguate. A provider that
/// reports more cache than input must not make the subtraction wrap (saturating).
#[test]
fn an_unmarked_record_is_taken_at_face_value_and_a_bad_one_never_wraps() {
    let old = ModelUsage {
        input: Some(200_000),
        cache_read: Some(190_000),
        ..ModelUsage::default()
    };
    assert_eq!(old.input_includes_cache, None);
    assert_eq!(old.prompt_total(), Some(200_000), "not 390_000");
    assert_eq!(old.uncached_input(), Some(200_000), "not 10_000: unmarked");

    let inconsistent = ModelUsage {
        input: Some(5),
        cache_read: Some(9),
        input_includes_cache: Some(true),
        ..ModelUsage::default()
    };
    assert_eq!(inconsistent.uncached_input(), Some(0), "never wraps");

    // A cache reading with no input count at all is still something to report: the prompt is
    // at least the cached part. With no counters whatsoever, nothing is claimed.
    let cache_only = ModelUsage {
        cache_read: Some(7),
        ..ModelUsage::default()
    };
    assert_eq!(cache_only.prompt_total(), Some(7));
    assert_eq!(cache_only.uncached_input(), None);
    assert_eq!(ModelUsage::default().prompt_total(), None);
    assert_eq!(ModelUsage::default().uncached_input(), None);
}

/// `has_billable_counters` separates "reported zeros" from "reported nothing": a response of
/// all zeros has nothing to price, so the `cost` item hides instead of showing `$0.00`
/// (ADR 0027).
#[test]
fn only_a_nonzero_counter_is_billable() {
    assert!(
        !ModelUsage {
            input: Some(0),
            output: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            input_includes_cache: Some(true),
            ..ModelUsage::default()
        }
        .has_billable_counters()
    );
    assert!(!ModelUsage::default().has_billable_counters());
    assert!(
        !ModelUsage {
            input_includes_cache: Some(false),
            ..ModelUsage::default()
        }
        .has_billable_counters()
    );
    for billable in [
        ModelUsage {
            input: Some(1),
            ..ModelUsage::default()
        },
        ModelUsage {
            output: Some(1),
            ..ModelUsage::default()
        },
        ModelUsage {
            cache_read: Some(1),
            ..ModelUsage::default()
        },
        ModelUsage {
            cache_write: Some(1),
            ..ModelUsage::default()
        },
    ] {
        assert!(billable.has_billable_counters(), "{billable:?}");
    }
}

/// The mark round-trips through the v1 JSON, and a record without it still reads: the journal
/// needs no schema bump (ADR 0007; ADR 0006 §7a's "unknown optional field" rule in
/// reverse — an old reader ignores the new key, a new reader reads an old file).
#[test]
fn the_cache_semantics_mark_round_trips_and_is_optional() {
    let marked = ModelUsage {
        input: Some(11),
        cache_read: Some(3),
        input_includes_cache: Some(true),
        ..ModelUsage::default()
    };
    let json = serde_json::to_string(&marked).unwrap();
    assert_eq!(
        json,
        r#"{"input":11,"cache_read":3,"input_includes_cache":true}"#
    );
    let back: ModelUsage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, marked);

    let old: ModelUsage = serde_json::from_str(r#"{"input":11,"cache_read":3}"#).unwrap();
    assert_eq!(old.input_includes_cache, None);
    // An unmarked usage serializes without the key, so the wire shape of an old record is
    // byte-for-byte what it was.
    assert_eq!(
        serde_json::to_string(&old).unwrap(),
        r#"{"input":11,"cache_read":3}"#
    );
}

// ---------- journal and events (AC-A5-7/8/9/10) ----------

fn usage_value() -> ModelUsage {
    ModelUsage {
        input: Some(8),
        output: Some(2),
        total: Some(10),
        cache_read: Some(1),
        cache_write: Some(0),
        // The OpenAI shape (ADR 0007): `input` contains the one cached token.
        input_includes_cache: Some(true),
        details: Some(OpaqueJson::parse(r#"{"cached_tokens":1}"#).unwrap()),
    }
}

/// An assistant reply that carries usage.
fn reply_with_usage() -> Reply {
    Reply::Respond(ModelResponse {
        limits: None,
        reasoning: Vec::new(),
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

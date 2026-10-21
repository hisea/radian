//! ADR 0006-8/9/10/11: the end-to-end proof, repair write-back, selfcheck
//! non-interference, and the sensitive-information rule.

mod common;

use std::sync::Arc;

use common::{MemTools, ScriptedModel, TempDir, call, calls, created, text};
use radian_core::{
    Agent, JournalRecord, Message, NoopSink, RunStatus, TranscriptJournal, TurnOptions,
};
use radian_session::{EventData, Session, SessionJournal};
use serde_json::json;

fn session_id() -> &'static str {
    "sid-e2e"
}

/// §15-9: a real turn, journaled to disk by this crate, then recovered from the file and
/// continued in a fresh session.
#[tokio::test]
async fn a_full_turn_round_trips_through_the_file_and_continues() {
    let dir = TempDir::new("e2e");
    let path = dir.file("s.jsonl");
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({"n": 1}))]),
        text("first answer"),
    ]);
    let agent = Arc::new(Agent::new(model.clone(), MemTools::new()));
    let journal: Arc<dyn TranscriptJournal> =
        Arc::new(SessionJournal::create(&path, session_id(), created()).unwrap());

    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap();
    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    chat.close().await.unwrap();
    drop(chat);

    // Rebuild the history purely from the file, using the core's own recover logic.
    let session = Session::open(&path).unwrap();
    let history = session.history().unwrap();
    assert_eq!(history.len(), 4, "{history:?}");
    assert_eq!(history[0].text(), "go");
    assert_eq!(history[3].text(), "first answer");

    // Open a new session from that history and run another real turn.
    let model2 = ScriptedModel::new([text("second answer")]);
    let agent2 = Arc::new(Agent::new(model2.clone(), MemTools::new()));
    let mut chat2 = agent2
        .open_session_with_history(history.clone())
        .await
        .unwrap();
    let outcome = chat2
        .run_turn("continue", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_message.unwrap().text(), "second answer");

    // The new request carried the recovered history as a prefix.
    let request = &model2.requests()[0];
    assert_eq!(&request.messages[..history.len()], history.as_slice());
    assert_eq!(request.messages.last().unwrap().text(), "continue");
}

/// §15-8: repair write-back is a normal append through `resume`, and `seq` stays
/// continuous (ADR 0002 E9).
#[tokio::test]
async fn repair_write_back_appends_normally_and_keeps_seq_continuous() {
    let dir = TempDir::new("repair");
    let path = dir.file("s.jsonl");
    let sid = session_id();
    {
        let journal = SessionJournal::create(&path, sid, created()).unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("run the check"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 1,
                message: Message::Assistant {
                    reasoning: Vec::new(),
                    content: vec![],
                    tool_calls: vec![call("c1", "echo", json!({}))],
                    provider_payload: None,
                    interrupted: false,
                    meta: Default::default(),
                },
            })
            .await
            .unwrap();
        // Crash before the call settles: Started exists, no result message.
        journal
            .append(JournalRecord::ToolExecutionStarted {
                call_id: "c1".into(),
                name: "echo".into(),
                arguments: json!({}),
            })
            .await
            .unwrap();
    }

    let session = Session::open(&path).unwrap();
    let repaired = session.history().unwrap();
    assert_eq!(repaired.len(), 3, "{repaired:?}");
    let Message::Tool { result, .. } = &repaired[2] else {
        panic!("expected the repaired tool result");
    };
    assert_eq!(
        result.error_code.as_deref(),
        Some("outcome_unknown_after_crash")
    );

    // E9: write the repaired message back through the ordinary append path.
    let journal = SessionJournal::resume(&path, sid).unwrap();
    assert_eq!(journal.next_seq(), 4);
    journal
        .append(JournalRecord::Message {
            index: 2,
            message: repaired[2].clone(),
        })
        .await
        .unwrap();
    drop(journal);

    let session = Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [0, 1, 2, 3, 4], "seq must continue, not restart");
    // The write-back is idempotent: the call is now answered, so no second repair.
    assert_eq!(session.history().unwrap(), repaired);
}

/// §15-10: our journal is the mounted one; `radian-core`'s implicit recording selfcheck
/// only mounts when no journal is given, so it never double-writes or conflicts.
#[tokio::test]
async fn an_explicit_journal_is_the_only_one_mounted() {
    let dir = TempDir::new("selfcheck");
    let path = dir.file("s.jsonl");
    let model = ScriptedModel::new([text("done")]);
    let agent = Arc::new(Agent::new(model, MemTools::new()));
    let journal: Arc<dyn TranscriptJournal> =
        Arc::new(SessionJournal::create(&path, session_id(), created()).unwrap());
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap();
    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    chat.close().await.unwrap();
    drop(chat);

    // Exactly the session's own records are on disk: SessionCreated, user, assistant.
    let session = Session::open(&path).unwrap();
    let events: Vec<&str> = session.events().iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        events,
        ["SessionCreated", "MessageAppended", "MessageAppended"]
    );
}

/// §15-11: the writer never scrapes the environment, so a key that only lives there
/// cannot reach the file.
#[tokio::test]
async fn sensitive_values_never_reach_the_file() {
    // SAFETY: this test intentionally mutates the process environment; the variable is
    // read by nothing in this crate.
    unsafe {
        std::env::set_var("RADIAN_FAKE_API_KEY", "sk-live-DEADBEEF0123456789");
    }
    let dir = TempDir::new("secret");
    let path = dir.file("s.jsonl");
    let model = ScriptedModel::new([text("done")]);
    let agent = Arc::new(Agent::new(model, MemTools::new()));
    let journal: Arc<dyn TranscriptJournal> =
        Arc::new(SessionJournal::create(&path, session_id(), created()).unwrap());
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap();
    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    chat.close().await.unwrap();
    drop(chat);

    let raw = std::fs::read(&path).unwrap();
    let text = String::from_utf8(raw).unwrap();
    assert!(
        !text.contains("DEADBEEF"),
        "an environment secret must never be written:\n{text}"
    );
    assert!(!text.contains("RADIAN_FAKE_API_KEY"), "{text}");
    // No envelope carries a secret-looking key either.
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        for secret_key in ["api_key", "authorization", "password", "token"] {
            assert!(value.get(secret_key).is_none(), "{line}");
        }
    }
}

/// §15-7 (degraded: no SQLite in this slice): the fold is rebuilt from the JSONL and
/// agrees with an independent line scan performed in the test.
#[tokio::test]
async fn the_fold_agrees_with_an_independent_line_scan() {
    for name in [
        "v1-minimal.jsonl",
        "replace-resets-index.jsonl",
        "replace-committed.jsonl",
        "replace-aborted-then-append.jsonl",
        "replace-uncommitted-eof.jsonl",
        "crash-after-started.jsonl",
    ] {
        let session = Session::open(common::fixture(name)).unwrap();
        // Independent scan of ADR 0019 §2, written here without the crate's fold: a pending
        // segment holds the old history; only a matching commit swaps it in; an abort or end
        // of file drops it.
        let mut history: Vec<Message> = Vec::new();
        let mut pending: Option<Vec<Message>> = None;
        for event in session.events() {
            match &event.data {
                EventData::MessageAppended { index, message } => {
                    let bucket = pending.as_mut().unwrap_or(&mut history);
                    assert_eq!(*index, bucket.len() as u64, "{name}: index continuity");
                    bucket.push(message.clone());
                }
                EventData::TranscriptReplaced { .. } => pending = Some(Vec::new()),
                EventData::TranscriptReplacementCommitted { .. } => {
                    history = pending.take().expect("commit with no pending segment");
                }
                EventData::TranscriptReplacementAborted { .. } => pending = None,
                _ => {}
            }
        }
        let mut expected = history.clone();
        expected.extend(radian_core::repair(&history, &session.journal_records()));
        assert_eq!(session.history().unwrap(), expected, "{name}");

        // The core accepts the folded history for a new session (it answers every call).
        let agent = Arc::new(Agent::new(ScriptedModel::new([]), radian_core::NoTools));
        agent
            .open_session_with_history(expected)
            .await
            .unwrap_or_else(|e| panic!("{name}: the folded history must be valid: {e}"));
    }
}

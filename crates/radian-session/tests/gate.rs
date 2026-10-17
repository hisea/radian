//! ADR 0018 §A: the writer's replacement-segment gate. Host appends (`InboxEnqueued`) must
//! not land inside a core replacement segment, and a failed core append inside it must fail
//! the waiting host append instead of writing into a broken run.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::{InputSource, JournalRecord, Message, MessageId, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{
    InboxLane, ReplacementEnd, RuntimeEvent, Session, SessionJournal, WriteError,
};

const SID: &str = "gate-sid";
/// The injected-failure test has its own session id: the fault seam is keyed by session, so
/// the other tests here (which share `SID`) can never consume its armed failure.
const FAIL_SID: &str = "gate-fail-sid";

fn inbox_event(text: &str) -> RuntimeEvent {
    RuntimeEvent::InboxEnqueued {
        message: Message::user(text),
        message_id: MessageId(format!("m-{text}")),
        lane: InboxLane::NextTurn,
        source: InputSource::User,
        wake: true,
    }
}

/// A host append that arrives while a segment is open waits for the commit, so the file is
/// readable and the runtime event lands after the commit.
#[tokio::test]
async fn a_runtime_event_waits_for_the_replacement_segment_to_close() {
    let dir = TempDir::new("gate-wait");
    let path = dir.file("s.jsonl");
    let writer = Arc::new(SessionJournal::create(&path, SID, created()).unwrap());
    writer
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 2,
        })
        .await
        .unwrap();

    let waiter = Arc::clone(&writer);
    let task = tokio::spawn(async move { waiter.append_runtime_event(inbox_event("hello")).await });
    // The gate is the synchronization; the timeout is only a safety net so a missing gate
    // fails the test instead of hanging it.
    tokio::time::timeout(Duration::from_secs(5), writer.wait_for_gate_waiter())
        .await
        .expect("the host append must wait while the segment is open");
    assert!(writer.segment_open());

    writer
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("a"),
        })
        .await
        .unwrap();
    writer
        .append(JournalRecord::Message {
            index: 1,
            message: Message::user("b"),
        })
        .await
        .unwrap();
    writer
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 2,
        })
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    assert!(!writer.segment_open());

    let session = Session::open(&path).unwrap();
    let committed_seq = session
        .events()
        .iter()
        .find(|e| e.event == "TranscriptReplacementCommitted")
        .unwrap()
        .seq;
    let inbox_seq = session
        .events()
        .iter()
        .find(|e| e.event == "InboxEnqueued")
        .unwrap()
        .seq;
    assert!(
        inbox_seq > committed_seq,
        "the runtime event must land after the commit: {inbox_seq} vs {committed_seq}"
    );
    assert_eq!(
        session.history().unwrap(),
        vec![Message::user("a"), Message::user("b")]
    );
}

/// A core append that fails inside the open segment poisons the gate: the host waiter gets an
/// error, and the session is still recoverable (`resume` writes the abort).
#[tokio::test]
async fn a_failed_segment_write_fails_the_waiter_and_the_file_still_resumes() {
    let dir = TempDir::new("gate-poison");
    let path = dir.file("s.jsonl");
    let writer = Arc::new(SessionJournal::create(&path, FAIL_SID, created()).unwrap());
    writer
        .append(JournalRecord::TranscriptReplaced {
            revision: 2,
            len: 1,
        })
        .await
        .unwrap();
    writer
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("a"),
        })
        .await
        .unwrap();

    radian_session::fail_next_write(FAIL_SID, "TranscriptReplacementCommitted");
    let waiter = Arc::clone(&writer);
    let task = tokio::spawn(async move { waiter.append_runtime_event(inbox_event("hello")).await });
    tokio::time::timeout(Duration::from_secs(5), writer.wait_for_gate_waiter())
        .await
        .expect("the host append must wait while the segment is open");
    let commit = writer
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 2,
            len: 1,
        })
        .await;
    assert!(commit.is_err(), "the injected commit failure must surface");

    let waited = task.await.unwrap();
    assert!(
        matches!(waited, Err(WriteError::SegmentPoisoned { .. })),
        "the waiting host append must fail: {waited:?}"
    );
    // Later host appends fail too, instead of writing into the broken run.
    let later = writer.append_runtime_event(inbox_event("again")).await;
    assert!(
        matches!(later, Err(WriteError::SegmentPoisoned { .. })),
        "{later:?}"
    );

    // The file ends in a pending segment; a resume writes the abort and the file is sound.
    let pending = Session::open(&path).unwrap();
    assert_eq!(pending.uncommitted_replacements().len(), 1);
    assert_eq!(
        pending.uncommitted_replacements()[0].end,
        ReplacementEnd::EndOfFile
    );
    drop(writer);
    {
        let _resumed = SessionJournal::resume(&path, FAIL_SID).unwrap();
    }
    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.uncommitted_replacements()[0].end,
        ReplacementEnd::Aborted
    );
}

/// Without an open segment, a host append is immediate and unaffected.
#[tokio::test]
async fn without_a_segment_a_runtime_event_is_immediate() {
    let dir = TempDir::new("gate-none");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("a"),
        })
        .await
        .unwrap();
    journal
        .append_runtime_event(inbox_event("hello"))
        .await
        .unwrap();
    assert!(!journal.segment_open());
    let session = Session::open(&path).unwrap();
    assert!(session.events().iter().any(|e| e.event == "InboxEnqueued"));
}

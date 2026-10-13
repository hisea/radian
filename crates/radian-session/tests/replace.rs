//! Replacement segments and the normal appends that follow them (ADR 0006 §5.11).
//!
//! `TranscriptReplaced.len` describes the `len` messages that immediately follow its marker.
//! Once those have arrived the segment is closed, and every later `MessageAppended` is normal
//! transcript growth (`index` continues from `len`). Counting the later appends into the
//! segment made a session unreadable the moment it was appended to after a replacement.

mod common;

use agent_core::{JournalRecord, Message, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{RecoveredReplacement, Session, SessionJournal};

const SID: &str = "replace-sid";

fn envelope(seq: u64, event: &str, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"{SID}","seq":{seq},"ts":"2026-10-12T00:00:0{seq}.000000Z","event":"{event}","critical":true,"data":{data}}}"#
    )
}

fn user(text: &str) -> serde_json::Value {
    serde_json::json!({"user": {"content": [{"text": {"text": text}}]}})
}

/// The reviewer's exact reproduction: marker `len=1`, one replacement message, then one normal
/// append at `index=1`.
#[test]
fn a_normal_append_after_a_complete_replacement_is_not_counted_as_part_of_it() {
    let dir = TempDir::new("replace-minimal");
    let path = dir.file("s.jsonl");
    let body = [
        envelope(
            0,
            "SessionCreated",
            serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
        ),
        envelope(
            1,
            "TranscriptReplaced",
            serde_json::json!({"revision": 1, "len": 1}),
        ),
        envelope(
            2,
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("replacement")}),
        ),
        envelope(
            3,
            "MessageAppended",
            serde_json::json!({"index": 1, "message": user("normal append")}),
        ),
    ]
    .join("\n");
    std::fs::write(&path, format!("{body}\n")).unwrap();

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        vec![Message::user("replacement"), Message::user("normal append")]
    );
    assert!(session.skipped().is_empty());
}

/// A replacement, normal appends, a second replacement, normal appends: the segment resets at
/// each marker and the history is the last revision plus what followed it.
#[tokio::test]
async fn replacements_and_the_appends_between_them_fold_to_the_last_revision() {
    let dir = TempDir::new("replace-appends");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    let append = async |index: usize, message: Message| {
        journal
            .append(JournalRecord::Message { index, message })
            .await
            .unwrap();
    };
    append(0, Message::user("old")).await;
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 1,
        })
        .await
        .unwrap();
    append(0, Message::user("first replacement")).await;
    // Normal growth after the replacement: the index continues from `len`.
    append(1, Message::assistant("grown one")).await;
    append(2, Message::user("grown two")).await;
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 2,
            len: 2,
        })
        .await
        .unwrap();
    append(0, Message::user("second replacement")).await;
    append(1, Message::assistant("second reply")).await;
    append(2, Message::user("tail")).await;
    drop(journal);

    let session = Session::open(&path).unwrap();
    assert!(session.skipped().is_empty());
    assert_eq!(
        session.history().unwrap(),
        vec![
            Message::user("second replacement"),
            Message::assistant("second reply"),
            Message::user("tail"),
        ]
    );
}

/// `resume` continues the sequence after a replacement and its appends, and the appended line
/// reads back.
#[tokio::test]
async fn resume_continues_after_a_replacement_and_its_appends() {
    let dir = TempDir::new("replace-resume");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 1,
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("replacement"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::assistant("grown"),
        })
        .await
        .unwrap();
    let next = journal.next_seq();
    drop(journal);

    let journal = SessionJournal::resume(&path, SID).unwrap();
    assert_eq!(journal.next_seq(), next);
    journal
        .append(JournalRecord::Message {
            index: 2,
            message: Message::user("after resume"),
        })
        .await
        .unwrap();
    drop(journal);

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        vec![
            Message::user("replacement"),
            Message::assistant("grown"),
            Message::user("after resume"),
        ]
    );
}

/// A crash mid-replacement leaves a short segment at end of file. `resume` can continue
/// appending (ADR 0005) and the file stays readable: a non-message append leaves the segment
/// short, so the recovery diagnostic survives.
#[tokio::test]
async fn resume_continues_after_a_short_replacement() {
    let dir = TempDir::new("replace-short-resume");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 2,
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("prefix"),
        })
        .await
        .unwrap();
    let next = journal.next_seq();
    drop(journal);

    let before = Session::open(&path).unwrap();
    assert_eq!(before.history().unwrap(), vec![Message::user("prefix")]);
    assert_eq!(
        before.recovered_replacement(),
        Some(RecoveredReplacement {
            revision: 1,
            declared: 2,
            written: 1,
        })
    );

    let journal = SessionJournal::resume(&path, SID).unwrap();
    assert_eq!(journal.next_seq(), next);
    journal
        .append(JournalRecord::ModelUsage {
            usage: agent_core::ModelUsage::default(),
        })
        .await
        .unwrap();
    drop(journal);

    let after = Session::open(&path).unwrap();
    assert_eq!(after.history().unwrap(), vec![Message::user("prefix")]);
    assert_eq!(
        after.recovered_replacement(),
        Some(RecoveredReplacement {
            revision: 1,
            declared: 2,
            written: 1,
        })
    );
}

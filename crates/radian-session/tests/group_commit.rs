//! ADR 0040: the group commit — `write_all` + `flush` on every append, `fsync` only at the
//! sync points (a boundary, or before an external side effect), and on close/drop.
//!
//! These tests pin the sync set directly through the writer's own durability view
//! (`durable_len`, the file length at the last successful fsync) and the crash seam
//! (`simulate_system_crash`). The end-to-end crash traversal over a scripted turn lives in
//! `radian-runtime/tests/group_commit.rs`.

mod common;

use std::path::Path;

use radian_core::{JournalRecord, Message, ModelUsage, ToolResult, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{RuntimeEvent, Session, SessionJournal, TurnOutcome};
use serde_json::json;

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// A session id no other test uses. The `fail_next_write` seam is keyed by session id
/// process-wide, so tests that share an id can consume each other's armed failure.
fn sid(label: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{label}-{n}")
}

/// Every sync event makes the whole group written before it durable: after the append,
/// `durable_len` (the last fsynced length) equals the file length.
#[tokio::test]
async fn a_sync_point_makes_every_earlier_record_durable() {
    #[derive(Debug, Clone, Copy)]
    enum Sync {
        SessionCreated,
        ToolStarted,
        TurnFinished,
        TurnFailed,
        ReplacementCommitted,
        ReplacementAborted,
    }

    for case in [
        Sync::SessionCreated,
        Sync::ToolStarted,
        Sync::TurnFinished,
        Sync::TurnFailed,
        Sync::ReplacementCommitted,
        Sync::ReplacementAborted,
    ] {
        let sid = sid("sync");
        let dir = TempDir::new("sync");
        let path = dir.file("s.jsonl");
        let journal = SessionJournal::create(&path, &sid, created()).unwrap();

        if matches!(case, Sync::SessionCreated) {
            assert_eq!(
                journal.durable_len(),
                file_len(&path),
                "SessionCreated is a sync point (the first line): the file is durable"
            );
            continue;
        }

        // A non-sync record first, so the sync point has a group to carry.
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("a"),
            })
            .await
            .unwrap();
        assert!(
            journal.durable_len() < file_len(&path),
            "{case:?}: the message alone must not be durable"
        );

        match case {
            Sync::SessionCreated => unreachable!(),
            Sync::ToolStarted => journal
                .append(JournalRecord::ToolExecutionStarted {
                    call_id: "c1".into(),
                    name: "echo".into(),
                    arguments: json!({}),
                })
                .await
                .unwrap(),
            Sync::TurnFinished => journal
                .append_runtime_event(RuntimeEvent::TurnFinished {
                    turn_id: "t1".into(),
                    outcome: TurnOutcome::Completed,
                    reason: None,
                    stop_reason: None,
                })
                .await
                .unwrap(),
            Sync::TurnFailed => journal
                .append_runtime_event(RuntimeEvent::TurnFailed {
                    turn_id: "t1".into(),
                    message: "boom".into(),
                })
                .await
                .unwrap(),
            Sync::ReplacementCommitted => journal
                .append(JournalRecord::TranscriptReplacementCommitted {
                    revision: 1,
                    len: 0,
                })
                .await
                .unwrap(),
            Sync::ReplacementAborted => journal
                .append(JournalRecord::TranscriptReplacementAborted { revision: 1 })
                .await
                .unwrap(),
        }

        assert_eq!(
            journal.durable_len(),
            file_len(&path),
            "{case:?}: the sync point makes the whole group durable"
        );
        assert!(
            journal.sync_count() >= 1,
            "{case:?}: a sync event must fsync"
        );
        journal.close().unwrap();
    }
}

/// The records ADR 0040 decision 3 leaves out of the sync set are only `write`+`flush`ed: they
/// are not durable when `append` returns, and the next sync point makes them durable in one
/// group.
#[tokio::test]
async fn non_sync_records_ride_the_next_sync_point() {
    let sid = sid("nonsync");
    let dir = TempDir::new("nonsync");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, &sid, created()).unwrap();
    let after_create = journal.durable_len();
    let syncs_after_create = journal.sync_count();
    assert_eq!(after_create, file_len(&path), "SessionCreated is durable");

    let non_sync_events: &[(&str, RuntimeEvent)] = &[
        (
            "TurnStarted",
            RuntimeEvent::TurnStarted {
                turn_id: "t1".into(),
            },
        ),
        (
            "ConfigChanged",
            RuntimeEvent::ConfigChanged {
                revision: 1,
                change: json!({"kind": "Policy", "name": "auto"}),
            },
        ),
        (
            "BackgroundJobStarted",
            RuntimeEvent::BackgroundJobStarted {
                job_id: "j1".into(),
                command: "sleep 1".into(),
                output: "job_output".into(),
            },
        ),
        (
            "UserBash",
            RuntimeEvent::UserBash {
                command: "echo hi".into(),
                exclude_from_context: false,
                exit_code: Some(0),
                cancelled: false,
            },
        ),
    ];

    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("a"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ToolExecutionSettled {
            call_id: "c1".into(),
            result: ToolResult::success("c1", Vec::new()),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ModelUsage {
            usage: ModelUsage {
                input: Some(1),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    for (_, event) in non_sync_events {
        journal.append_runtime_event(event.clone()).await.unwrap();
    }

    assert_eq!(
        journal.durable_len(),
        after_create,
        "no non-sync record may fsync: the durable prefix is still the first line"
    );
    assert!(
        journal.durable_len() < file_len(&path),
        "the non-sync records are only flushed"
    );
    assert_eq!(
        journal.sync_count(),
        syncs_after_create,
        "no non-sync record may fsync"
    );

    // One sync point carries the whole group.
    journal
        .append_runtime_event(RuntimeEvent::TurnFinished {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            reason: None,
            stop_reason: None,
        })
        .await
        .unwrap();
    assert_eq!(
        journal.durable_len(),
        file_len(&path),
        "the turn boundary makes everything before it durable"
    );
    assert_eq!(
        journal.sync_count(),
        syncs_after_create + 1,
        "exactly one fsync for the group"
    );
}

/// A failed write does not burn a `seq`: the counter advances only after the write succeeds
/// (ADR 0006 §10; the group-commit change must not move it earlier).
#[tokio::test]
async fn a_failed_write_does_not_burn_a_seq() {
    let sid = sid("seq-fail");
    let dir = TempDir::new("seq-fail");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, &sid, created()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let seq = journal.next_seq();

    radian_session::fail_next_write(&sid, "ToolExecutionStarted");
    let error = journal
        .append(JournalRecord::ToolExecutionStarted {
            call_id: "c1".into(),
            name: "echo".into(),
            arguments: json!({}),
        })
        .await;
    assert!(error.is_err(), "the injected write failure must surface");
    assert_eq!(
        journal.next_seq(),
        seq,
        "a failed write must not advance the seq counter"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a failed write must leave the file untouched"
    );
}

/// `simulate_system_crash` drops exactly the bytes written after the last fsync; the file is
/// then a valid prefix that `resume` reads back.
#[tokio::test]
async fn simulate_system_crash_drops_exactly_the_unsynced_bytes() {
    let sid = sid("sim-crash");
    let dir = TempDir::new("sim-crash");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, &sid, created()).unwrap();
    let durable = std::fs::read(&path).unwrap();

    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("lost"),
        })
        .await
        .unwrap();
    assert!(std::fs::read(&path).unwrap().len() > durable.len());

    journal.simulate_system_crash().unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        durable,
        "the unsynced bytes are gone"
    );
    assert_eq!(journal.durable_len(), durable.len() as u64);
    drop(journal);

    let resumed = SessionJournal::resume(&path, &sid).unwrap();
    assert_eq!(resumed.next_seq(), 1, "only SessionCreated survives");
    let session = Session::open(&path).unwrap();
    assert_eq!(session.events().len(), 1, "the message was lost");
}

/// `close` makes the unsynced group durable and reports success; `Drop` runs the same sync
/// best-effort.
#[tokio::test]
async fn close_makes_the_pending_group_durable() {
    let sid = sid("close");
    let dir = TempDir::new("close");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, &sid, created()).unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("last"),
        })
        .await
        .unwrap();
    assert!(journal.durable_len() < file_len(&path));
    journal.close().unwrap();
    assert_eq!(journal.durable_len(), file_len(&path));

    // A second unsynced record, then `Drop`: the best-effort sync still makes it durable.
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::user("dropped"),
        })
        .await
        .unwrap();
    assert!(journal.durable_len() < file_len(&path));
    drop(journal);
    let session = Session::open(&path).unwrap();
    assert_eq!(session.events().len(), 3, "both messages are on disk");
}

/// The end-to-end writer failure still surfaces through `append` exactly like a write failure
/// (ADR 0001 D6): the caller ends its turn.
#[tokio::test]
async fn a_failed_segment_commit_still_reports_a_journal_error() {
    let sid = sid("fail-commit");
    let dir = TempDir::new("fail-commit");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, &sid, created()).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 0,
        })
        .await
        .unwrap();
    radian_session::fail_next_write(&sid, "TranscriptReplacementCommitted");
    let error = journal
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 0,
        })
        .await;
    assert!(
        matches!(error, Err(ref e) if e.to_string().contains("append failed")),
        "a failed commit append must surface as a journal error: {error:?}"
    );
}

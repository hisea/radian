//! ADR 0022: session fork. The fork point is a parent turn boundary; the child materializes
//! the parent's effective history there; the child is written before the parent records it.

mod common;

use std::path::Path;

use agent_core::{JournalRecord, Message, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{
    EventData, ForkError, ForkRequest, RuntimeEvent, Session, SessionJournal, TurnOutcome, fork,
    fork_point_history, fork_with_journal, last_fork_point,
};
use serde_json::json;

fn ids(label: &str) -> (String, String) {
    (format!("parent-{label}"), format!("child-{label}"))
}

async fn append_message(journal: &SessionJournal, index: usize, message: Message) {
    journal
        .append(JournalRecord::Message { index, message })
        .await
        .unwrap();
}

/// Appends one whole turn starting at transcript `index`; returns the `TurnFinished` seq and
/// the next free transcript index.
async fn add_turn(
    journal: &SessionJournal,
    turn_id: &str,
    index: usize,
    user: &str,
    assistant: &str,
) -> (u64, usize) {
    journal
        .append_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.into(),
        })
        .await
        .unwrap();
    append_message(journal, index, Message::user(user)).await;
    append_message(journal, index + 1, Message::assistant(assistant)).await;
    let seq = journal.next_seq();
    journal
        .append_runtime_event(RuntimeEvent::TurnFinished {
            turn_id: turn_id.into(),
            outcome: TurnOutcome::Completed,
            reason: None,
            stop_reason: None,
        })
        .await
        .unwrap();
    (seq, index + 2)
}

/// A parent with two complete turns. Returns its path and the last boundary seq.
async fn two_turn_parent(dir: &TempDir, parent_id: &str) -> (std::path::PathBuf, u64) {
    let path = dir.file("parent.jsonl");
    let journal = SessionJournal::create(&path, parent_id, created()).unwrap();
    let (_, index) = add_turn(&journal, "t-1", 0, "first user", "first assistant").await;
    let (boundary, _) = add_turn(&journal, "t-2", index, "second user", "second assistant").await;
    drop(journal);
    (path, boundary)
}

fn request(
    parent_path: &Path,
    parent_id: &str,
    seq: Option<u64>,
    child_path: &Path,
    child_id: &str,
) -> ForkRequest {
    ForkRequest {
        parent_path: parent_path.to_path_buf(),
        parent_id: parent_id.to_owned(),
        seq,
        child_path: child_path.to_path_buf(),
        child_id: child_id.to_owned(),
        workspace: "/work/example".into(),
        config_snapshot: json!({"model": {"provider": "example", "model": "m-1"}}),
    }
}

#[tokio::test]
async fn a_fork_materializes_the_effective_history_at_the_last_boundary() {
    let dir = TempDir::new("fork-basic");
    let (parent, child) = ids("basic");
    let (parent_path, boundary) = two_turn_parent(&dir, &parent).await;
    let child_path = dir.file("child.jsonl");

    let outcome = fork(request(&parent_path, &parent, None, &child_path, &child))
        .await
        .unwrap();
    assert_eq!(outcome.child_id, child);
    assert_eq!(outcome.seq, boundary, "the last turn boundary");
    assert_eq!(outcome.messages, 4);

    let opened = Session::open(&child_path).unwrap();
    assert_eq!(
        opened.history().unwrap(),
        vec![
            Message::user("first user"),
            Message::assistant("first assistant"),
            Message::user("second user"),
            Message::assistant("second assistant"),
        ]
    );
}

#[tokio::test]
async fn the_materialized_history_equals_recover_at_the_same_point() {
    let dir = TempDir::new("fork-equal");
    let (parent, _) = ids("equal");
    let (parent_path, boundary) = two_turn_parent(&dir, &parent).await;
    let session = Session::open(&parent_path).unwrap();
    let expected = agent_core::recover(&session.journal_records_through(boundary)).unwrap();
    assert_eq!(fork_point_history(&session, boundary).unwrap(), expected);
    assert_eq!(last_fork_point(&session), boundary);
}

#[tokio::test]
async fn a_seq_inside_a_turn_is_refused() {
    let dir = TempDir::new("fork-mid-turn");
    let (parent, _) = ids("midturn");
    let path = dir.file("p.jsonl");
    let journal = SessionJournal::create(&path, &parent, created()).unwrap();
    journal
        .append_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: "t-1".into(),
        })
        .await
        .unwrap();
    append_message(&journal, 0, Message::user("mid")).await;
    let mid = journal.next_seq() - 1; // the MessageAppended seq
    drop(journal);

    let session = Session::open(&path).unwrap();
    let error = fork_point_history(&session, mid).unwrap_err();
    match error {
        ForkError::NotATurnBoundary { detail, .. } => {
            assert!(detail.contains("turn is still running"), "{detail}")
        }
        other => panic!("expected a mid-turn refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_seq_inside_a_replacement_segment_is_refused() {
    let dir = TempDir::new("fork-segment");
    let (parent, _) = ids("segment");
    let (parent_path, boundary) = two_turn_parent(&dir, &parent).await;
    // An uncommitted replacement after the last boundary ends the file.
    let journal = SessionJournal::resume(&parent_path, &parent).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 2,
        })
        .await
        .unwrap();
    append_message(&journal, 0, Message::user("pending")).await;
    let inside = journal.next_seq() - 1;
    drop(journal);

    let session = Session::open(&parent_path).unwrap();
    let error = fork_point_history(&session, inside).unwrap_err();
    match error {
        ForkError::NotATurnBoundary { detail, .. } => {
            assert!(detail.contains("replacement segment"), "{detail}")
        }
        other => panic!("expected a segment refusal, got {other:?}"),
    }
    // The last boundary is still forkable and drops the pending segment.
    assert_eq!(last_fork_point(&session), boundary);
    assert_eq!(
        fork_point_history(&session, boundary).unwrap(),
        vec![
            Message::user("first user"),
            Message::assistant("first assistant"),
            Message::user("second user"),
            Message::assistant("second assistant"),
        ]
    );
}

#[tokio::test]
async fn an_unknown_or_out_of_range_seq_is_refused() {
    let dir = TempDir::new("fork-range");
    let (parent, _) = ids("range");
    let (parent_path, boundary) = two_turn_parent(&dir, &parent).await;
    let session = Session::open(&parent_path).unwrap();
    assert!(matches!(
        fork_point_history(&session, boundary + 100).unwrap_err(),
        ForkError::BeyondEnd { .. }
    ));
    // seq 1 is the first turn's TurnStarted: a real event, but not a boundary.
    assert!(matches!(
        fork_point_history(&session, 1).unwrap_err(),
        ForkError::NotATurnBoundary { .. }
    ));
}

#[tokio::test]
async fn a_committed_compaction_is_in_effect_in_the_child() {
    let dir = TempDir::new("fork-compacted");
    let (parent, child) = ids("compacted");
    let path = dir.file("p.jsonl");
    let journal = SessionJournal::create(&path, &parent, created()).unwrap();
    add_turn(&journal, "t-1", 0, "old one", "old two").await;
    // A committed replacement: the summary is the first message of the new view.
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 2,
        })
        .await
        .unwrap();
    append_message(&journal, 0, Message::user("the summary")).await;
    append_message(&journal, 1, Message::user("kept")).await;
    journal
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 2,
        })
        .await
        .unwrap();
    let (boundary, _) = add_turn(&journal, "t-2", 2, "after", "after reply").await;
    drop(journal);

    let child_path = dir.file("c.jsonl");
    fork(request(&path, &parent, Some(boundary), &child_path, &child))
        .await
        .unwrap();
    let opened = Session::open(&child_path).unwrap();
    assert_eq!(
        opened.history().unwrap(),
        vec![
            Message::user("the summary"),
            Message::user("kept"),
            Message::user("after"),
            Message::assistant("after reply"),
        ],
        "the committed replacement is the child's starting view"
    );
}

#[tokio::test]
async fn a_child_can_resume_and_fork_again() {
    let dir = TempDir::new("fork-chain");
    let (parent, child) = ids("chain");
    let (parent_path, _) = two_turn_parent(&dir, &parent).await;
    let child_path = dir.file("c.jsonl");
    fork(request(&parent_path, &parent, None, &child_path, &child))
        .await
        .unwrap();

    // The child resumes and grows; then it is itself forked.
    let journal = SessionJournal::resume(&child_path, &child).unwrap();
    let (boundary, _) = add_turn(&journal, "t-3", 4, "child user", "child assistant").await;
    drop(journal);
    let grandchild = "grandchild-chain";
    let grandchild_path = dir.file("g.jsonl");
    let outcome = fork(request(
        &child_path,
        &child,
        None,
        &grandchild_path,
        grandchild,
    ))
    .await
    .unwrap();
    assert_eq!(outcome.seq, boundary);
    let opened = Session::open(&grandchild_path).unwrap();
    assert_eq!(opened.history().unwrap().len(), 6);
}

#[tokio::test]
async fn the_parent_gains_only_the_forked_record_and_the_seqs_match() {
    let dir = TempDir::new("fork-parent-log");
    let (parent, child) = ids("log");
    let (parent_path, boundary) = two_turn_parent(&dir, &parent).await;
    let before = std::fs::read(&parent_path).unwrap();
    let child_path = dir.file("c.jsonl");
    fork(request(&parent_path, &parent, None, &child_path, &child))
        .await
        .unwrap();

    let after = std::fs::read(&parent_path).unwrap();
    // The original bytes are a prefix of the new file (only appended).
    assert!(after.starts_with(&before), "the parent's bytes changed");
    let text = String::from_utf8(after).unwrap();
    assert_eq!(
        text.matches("\"event\":\"SessionForked\"").count(),
        1,
        "exactly one SessionForked"
    );
    assert_eq!(text.matches("\"event\":\"MessageAppended\"").count(), 4);

    let session = Session::open(&parent_path).unwrap();
    let opened = Session::open(&child_path).unwrap();
    let created = opened
        .events()
        .iter()
        .find(|event| event.event == "SessionCreated")
        .unwrap();
    let EventData::Other(data) = &created.data else {
        panic!("SessionCreated is kept as Other");
    };
    assert_eq!(data["parent"]["seq"], json!(boundary));
    assert_eq!(data["parent"]["session_id"], json!(parent));
    // ADR 0030 O2: a fork writes `relation: "fork"` explicitly.
    assert_eq!(data["parent"]["relation"], json!("fork"), "{data}");
    // The parent's SessionForked carries the same seq and names the child.
    let forked = session
        .events()
        .iter()
        .find(|event| event.event == "SessionForked")
        .expect("the parent has SessionForked");
    let EventData::Other(forked) = &forked.data else {
        panic!("SessionForked is kept as Other");
    };
    assert_eq!(forked["parent_seq"], json!(boundary), "{forked}");
    assert_eq!(forked["child_session_id"], json!(child), "{forked}");
}

#[tokio::test]
async fn a_child_is_complete_when_the_parent_cannot_record_the_fork() {
    let dir = TempDir::new("fork-parent-fail");
    let (parent, child) = ids("nofork");
    let (parent_path, _) = two_turn_parent(&dir, &parent).await;
    let before = std::fs::read(&parent_path).unwrap();
    let child_path = dir.file("c.jsonl");

    radian_session::fail_next_write(&parent, "SessionForked");
    let error = fork(request(&parent_path, &parent, None, &child_path, &child))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkError::Parent { .. }), "{error:?}");

    assert_eq!(
        std::fs::read(&parent_path).unwrap(),
        before,
        "the parent changed"
    );
    let opened = Session::open(&child_path).unwrap();
    assert_eq!(opened.history().unwrap().len(), 4, "the child is complete");
}

#[tokio::test]
async fn a_child_failure_leaves_the_parent_untouched() {
    let dir = TempDir::new("fork-child-fail");
    let (parent, child) = ids("half");
    let (parent_path, _) = two_turn_parent(&dir, &parent).await;
    let before = std::fs::read(&parent_path).unwrap();
    let child_path = dir.file("c.jsonl");

    radian_session::fail_next_write(&child, "MessageAppended");
    let error = fork(request(&parent_path, &parent, None, &child_path, &child))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkError::ChildJournal { .. }), "{error:?}");

    assert_eq!(
        std::fs::read(&parent_path).unwrap(),
        before,
        "the parent changed"
    );
    // ADR 0022 §4 (review fix): the failed child is not left behind at all — no half-written
    // session, and no temp file.
    assert!(!child_path.exists(), "a half-written child must not exist");
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");
}

#[tokio::test]
async fn fork_with_journal_uses_the_open_writer() {
    let dir = TempDir::new("fork-inproc");
    let (parent, child) = ids("inproc");
    let (parent_path, boundary) = two_turn_parent(&dir, &parent).await;
    let child_path = dir.file("c.jsonl");
    // The TUI path: this process already holds the parent writer.
    let journal = SessionJournal::resume(&parent_path, &parent).unwrap();
    let outcome = fork_with_journal(
        request(&parent_path, &parent, None, &child_path, &child),
        &journal,
    )
    .await
    .unwrap();
    assert_eq!(outcome.seq, boundary);
    drop(journal);
    let opened = Session::open(&child_path).unwrap();
    assert_eq!(opened.history().unwrap().len(), 4);
}

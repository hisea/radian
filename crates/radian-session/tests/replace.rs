//! Replacement segments: the commit/abort markers and the commits and appends that follow
//! them (ADR 0019). A `TranscriptReplaced` opens a pending segment; only a matching
//! `TranscriptReplacementCommitted` replaces the history, a matching
//! `TranscriptReplacementAborted` drops it, and a file ending inside it is uncommitted.

mod common;

use common::{TempDir, created};
use radian_core::{JournalRecord, Message, ReplacementEnd, TranscriptJournal};
use radian_session::{
    ExportOptions, Index, ReadError, Session, SessionJournal, UncommittedReplacement, WriteError,
    export_json, export_markdown,
};

const SID: &str = "replace-sid";
/// The injected-abort-failure test has its own session id; the fault seam is keyed by
/// session, so a parallel `resume` test that writes an abort cannot consume it.
const ABORT_FAIL_SID: &str = "replace-abort-fail";

fn envelope(seq: u64, event: &str, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"{SID}","seq":{seq},"ts":"2026-10-12T00:00:0{seq}.000000Z","event":"{event}","critical":true,"data":{data}}}"#
    )
}

fn user(text: &str) -> serde_json::Value {
    serde_json::json!({"user": {"content": [{"text": {"text": text}}]}})
}

fn write_lines(path: &std::path::Path, lines: &[String]) {
    std::fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
}

/// A committed replacement followed by normal growth: the commit swaps the history in, and
/// the later `MessageAppended` indexes continue from the committed `len`.
#[test]
fn a_committed_replacement_then_normal_growth_folds_to_the_replacement() {
    let dir = TempDir::new("replace-committed");
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
            "TranscriptReplacementCommitted",
            serde_json::json!({"revision": 1, "len": 1}),
        ),
        envelope(
            4,
            "MessageAppended",
            serde_json::json!({"index": 1, "message": user("normal append")}),
        ),
    ];
    write_lines(&path, &body);

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        vec![Message::user("replacement"), Message::user("normal append")]
    );
    assert!(session.uncommitted_replacements().is_empty());
}

/// Without the commit the same file reads as the history before the replacement, and the
/// later append is *not* part of the segment (ADR 0019 §2's index-adjacency example): the
/// `Aborted` boundary is what makes the difference.
#[test]
fn an_aborted_replacement_drops_the_segment_and_the_next_append_is_normal() {
    let dir = TempDir::new("replace-aborted");
    let path = dir.file("s.jsonl");
    let body = [
        envelope(
            0,
            "SessionCreated",
            serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
        ),
        envelope(
            1,
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("old zero")}),
        ),
        envelope(
            2,
            "MessageAppended",
            serde_json::json!({"index": 1, "message": user("old one")}),
        ),
        // The replacement declared 4 but only wrote 2, then was aborted.
        envelope(
            3,
            "TranscriptReplaced",
            serde_json::json!({"revision": 7, "len": 4}),
        ),
        envelope(
            4,
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("pending zero")}),
        ),
        envelope(
            5,
            "MessageAppended",
            serde_json::json!({"index": 1, "message": user("pending one")}),
        ),
        envelope(
            6,
            "TranscriptReplacementAborted",
            serde_json::json!({"revision": 7}),
        ),
        // Index 2 continues the **old** history (length 2), not the dropped segment.
        envelope(
            7,
            "MessageAppended",
            serde_json::json!({"index": 2, "message": user("normal two")}),
        ),
    ];
    write_lines(&path, &body);

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        vec![
            Message::user("old zero"),
            Message::user("old one"),
            Message::user("normal two"),
        ]
    );
    assert_eq!(
        session.uncommitted_replacements(),
        &[UncommittedReplacement {
            revision: 7,
            declared_len: 4,
            written: 2,
            line: 4,
            end: ReplacementEnd::Aborted,
        }]
    );
}

/// An abort, then a new replacement that commits: the abort only closes the first segment.
#[test]
fn an_aborted_replacement_can_be_followed_by_a_committed_one() {
    let dir = TempDir::new("replace-abort-then-commit");
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
            serde_json::json!({"index": 0, "message": user("first try")}),
        ),
        envelope(
            3,
            "TranscriptReplacementAborted",
            serde_json::json!({"revision": 1}),
        ),
        envelope(
            4,
            "TranscriptReplaced",
            serde_json::json!({"revision": 2, "len": 1}),
        ),
        envelope(
            5,
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("second try")}),
        ),
        envelope(
            6,
            "TranscriptReplacementCommitted",
            serde_json::json!({"revision": 2, "len": 1}),
        ),
    ];
    write_lines(&path, &body);

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        vec![Message::user("second try")]
    );
    assert_eq!(
        session.uncommitted_replacements(),
        &[UncommittedReplacement {
            revision: 1,
            declared_len: 1,
            written: 1,
            line: 2,
            end: ReplacementEnd::Aborted,
        }]
    );
}

// ---------- the writer's resume ----------

/// A file ending inside a pending segment: `resume` writes `Aborted` first, and the next
/// append continues the old history. ADR 0019 §2's index-adjacency example: old history 2,
/// segment declared 4 and wrote 2, then index 2 arrives — the boundary keeps it normal.
#[tokio::test]
async fn resume_aborts_a_pending_segment_and_the_next_append_is_normal() {
    let dir = TempDir::new("replace-resume-abort");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    let append = async |index: usize, message: Message| {
        journal
            .append(JournalRecord::Message { index, message })
            .await
            .unwrap();
    };
    append(0, Message::user("old zero")).await;
    append(1, Message::user("old one")).await;
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 7,
            len: 4,
        })
        .await
        .unwrap();
    append(0, Message::user("pending zero")).await;
    append(1, Message::user("pending one")).await;
    drop(journal);

    // Read-only: the pending segment is diagnosed, the old history stands, and the file is
    // byte-for-byte unchanged.
    let before_bytes = std::fs::read(&path).unwrap();
    let before = Session::open(&path).unwrap();
    assert_eq!(
        before.history().unwrap(),
        vec![Message::user("old zero"), Message::user("old one")]
    );
    assert_eq!(before.uncommitted_replacements().len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);

    let journal = SessionJournal::resume(&path, SID).unwrap();
    journal
        .append(JournalRecord::Message {
            index: 2,
            message: Message::user("normal two"),
        })
        .await
        .unwrap();
    drop(journal);

    let after = Session::open(&path).unwrap();
    assert_eq!(
        after.history().unwrap(),
        vec![
            Message::user("old zero"),
            Message::user("old one"),
            Message::user("normal two"),
        ]
    );
    assert_eq!(after.uncommitted_replacements().len(), 1);
}

/// `resume` writes the abort once. A second `resume` sees the closed segment and adds
/// nothing (ADR 0019).
#[tokio::test]
async fn resume_does_not_repeat_the_abort() {
    let dir = TempDir::new("replace-resume-idempotent");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 3,
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
    drop(journal);

    let journal = SessionJournal::resume(&path, SID).unwrap();
    drop(journal);
    let after_first = std::fs::read_to_string(&path).unwrap();
    let aborts = after_first
        .matches("\"event\":\"TranscriptReplacementAborted\"")
        .count();
    assert_eq!(aborts, 1, "the first resume writes one abort");

    let journal = SessionJournal::resume(&path, SID).unwrap();
    drop(journal);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        after_first,
        "a second resume must not write another abort"
    );
}

/// A half-written `TranscriptReplacementAborted` is a torn tail: `resume` truncates it and
/// writes a fresh, complete abort.
#[tokio::test]
async fn a_torn_abort_is_truncated_and_rewritten() {
    let dir = TempDir::new("replace-resume-torn-abort");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 5,
            len: 1,
        })
        .await
        .unwrap();
    drop(journal);

    // Append a valid-JSON abort prefix with no LF: a torn tail.
    let torn = envelope(
        2,
        "TranscriptReplacementAborted",
        serde_json::json!({"revision": 5}),
    );
    let torn = &torn[..torn.len() - 4];
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.push_str(torn);
    std::fs::write(&path, body).unwrap();

    let journal = SessionJournal::resume(&path, SID).unwrap();
    drop(journal);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.ends_with("}\n"),
        "the file must end on a complete line"
    );
    assert_eq!(
        text.matches("\"event\":\"TranscriptReplacementAborted\"")
            .count(),
        1,
        "exactly one complete abort line"
    );
    let session = Session::open(&path).unwrap();
    assert_eq!(session.uncommitted_replacements().len(), 1);
}

// ---------- corruption (ADR 0019 §2) ----------

fn assert_corrupt_at(path: &std::path::Path, line: u64) {
    let error = Session::open(path).unwrap_err();
    assert!(
        matches!(&error, ReadError::Corrupt { line: found, .. } if *found == line),
        "expected corruption at line {line}, got {error:?}"
    );
}

/// A commit with no open replacement is corruption, not something a crash can produce.
#[test]
fn a_commit_without_a_replacement_is_corruption() {
    let dir = TempDir::new("replace-commit-orphan");
    let path = dir.file("s.jsonl");
    write_lines(
        &path,
        &[
            envelope(
                0,
                "SessionCreated",
                serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
            ),
            envelope(
                1,
                "TranscriptReplacementCommitted",
                serde_json::json!({"revision": 1, "len": 0}),
            ),
        ],
    );
    assert_corrupt_at(&path, 2);
}

/// A commit whose `revision` or `len` does not match the open segment is corruption.
#[test]
fn a_mismatched_commit_is_corruption() {
    let dir = TempDir::new("replace-commit-mismatch");
    let path = dir.file("s.jsonl");
    write_lines(
        &path,
        &[
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
                serde_json::json!({"index": 0, "message": user("a")}),
            ),
            envelope(
                3,
                "TranscriptReplacementCommitted",
                serde_json::json!({"revision": 2, "len": 1}),
            ),
        ],
    );
    assert_corrupt_at(&path, 4);
}

/// A message beyond the declared `len` before the commit is corruption.
#[test]
fn a_message_beyond_the_declared_len_is_corruption() {
    let dir = TempDir::new("replace-overflow");
    let path = dir.file("s.jsonl");
    write_lines(
        &path,
        &[
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
                serde_json::json!({"index": 0, "message": user("a")}),
            ),
            envelope(
                3,
                "MessageAppended",
                serde_json::json!({"index": 1, "message": user("b")}),
            ),
        ],
    );
    assert_corrupt_at(&path, 4);
}

/// A mismatched abort is corruption too.
#[test]
fn a_mismatched_abort_is_corruption() {
    let dir = TempDir::new("replace-abort-mismatch");
    let path = dir.file("s.jsonl");
    write_lines(
        &path,
        &[
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
                "TranscriptReplacementAborted",
                serde_json::json!({"revision": 9}),
            ),
        ],
    );
    assert_corrupt_at(&path, 3);
}

/// A normal runtime event inside a pending segment is corruption (ADR 0019 §2: only the
/// segment's messages, its commit or its abort may follow).
#[test]
fn a_runtime_event_inside_a_pending_segment_is_corruption() {
    let dir = TempDir::new("replace-runtime-inside");
    let path = dir.file("s.jsonl");
    write_lines(
        &path,
        &[
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
            envelope(2, "TurnStarted", serde_json::json!({"turn_id": "t-1"})),
        ],
    );
    assert_corrupt_at(&path, 3);
}

// ---------- a committed replacement through the writer, then a resume ----------

#[tokio::test]
async fn resume_continues_after_a_committed_replacement_and_its_appends() {
    let dir = TempDir::new("replace-resume-committed");
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
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 1,
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
    drop(journal);

    let journal = SessionJournal::resume(&path, SID).unwrap();
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
    assert!(session.uncommitted_replacements().is_empty());
}

/// Three committed replacements in a row fold to the last one.
#[tokio::test]
async fn three_committed_replacements_fold_to_the_last() {
    let dir = TempDir::new("replace-three");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    for (revision, text) in [(1u64, "one"), (2, "two"), (3, "three")] {
        journal
            .append(JournalRecord::TranscriptReplaced { revision, len: 1 })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user(text),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::TranscriptReplacementCommitted { revision, len: 1 })
            .await
            .unwrap();
    }
    drop(journal);

    let session = Session::open(&path).unwrap();
    assert_eq!(session.history().unwrap(), vec![Message::user("three")]);
    assert!(session.uncommitted_replacements().is_empty());
}

/// A message inside an uncommitted segment is not in the current history, so the export and
/// the search index treat it as absent (ADR 0019 实施要求).
#[test]
fn an_uncommitted_segment_is_absent_from_export_and_index() {
    let dir = TempDir::new("replace-export-index");
    let path = dir.file("s.jsonl");
    write_lines(
        &path,
        &[
            envelope(
                0,
                "SessionCreated",
                serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
            ),
            envelope(
                1,
                "MessageAppended",
                serde_json::json!({"index": 0, "message": user("committed old")}),
            ),
            envelope(
                2,
                "TranscriptReplaced",
                serde_json::json!({"revision": 1, "len": 2}),
            ),
            envelope(
                3,
                "MessageAppended",
                serde_json::json!({"index": 0, "message": user("uncommitted pending")}),
            ),
        ],
    );
    let session = Session::open(&path).unwrap();
    let json = export_json(&session, &ExportOptions::new()).unwrap();
    assert!(json.contains("committed old"), "{json}");
    assert!(!json.contains("uncommitted pending"), "{json}");
    let md = export_markdown(&session, &ExportOptions::new()).unwrap();
    assert!(md.contains("committed old"), "{md}");
    assert!(!md.contains("uncommitted pending"), "{md}");

    let mut index = Index::open(dir.file("i.db")).unwrap();
    index.rebuild(std::slice::from_ref(&path)).unwrap();
    let rows = index.indexed_events(session.session_id()).unwrap();
    assert!(
        rows.iter()
            .all(|row| row.text.as_deref() != Some("uncommitted pending")),
        "{rows:?}"
    );
    assert!(
        index
            .search("uncommitted pending", None, 10)
            .unwrap()
            .is_empty(),
        "the pending message must not be searchable"
    );
    assert!(
        !index.search("committed old", None, 10).unwrap().is_empty(),
        "the committed message stays searchable"
    );
}

// ---------- resume validates before touching the file (ADR 0019) ----------

/// ADR 0019: a corrupt file is refused by `resume` and left byte-for-byte untouched,
/// including when it also ends in a torn tail: validation happens before the truncation.
#[test]
fn resume_refuses_a_corrupt_file_without_touching_it() {
    let created = envelope(
        0,
        "SessionCreated",
        serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
    );
    let cases: [(&str, Vec<String>); 4] = [
        (
            "replace-resume-corrupt-pending-event",
            vec![
                created.clone(),
                envelope(
                    1,
                    "TranscriptReplaced",
                    serde_json::json!({"revision": 1, "len": 1}),
                ),
                envelope(2, "TurnStarted", serde_json::json!({"turn_id": "t-1"})),
            ],
        ),
        (
            "replace-resume-corrupt-commit-mismatch",
            vec![
                created.clone(),
                envelope(
                    1,
                    "TranscriptReplaced",
                    serde_json::json!({"revision": 1, "len": 1}),
                ),
                envelope(
                    2,
                    "MessageAppended",
                    serde_json::json!({"index": 0, "message": user("a")}),
                ),
                envelope(
                    3,
                    "TranscriptReplacementCommitted",
                    serde_json::json!({"revision": 2, "len": 1}),
                ),
            ],
        ),
        (
            "replace-resume-corrupt-abort-mismatch",
            vec![
                created.clone(),
                envelope(
                    1,
                    "TranscriptReplaced",
                    serde_json::json!({"revision": 1, "len": 1}),
                ),
                envelope(
                    2,
                    "TranscriptReplacementAborted",
                    serde_json::json!({"revision": 9}),
                ),
            ],
        ),
        (
            "replace-resume-corrupt-orphan-commit",
            vec![
                created,
                envelope(
                    1,
                    "TranscriptReplacementCommitted",
                    serde_json::json!({"revision": 1, "len": 0}),
                ),
            ],
        ),
    ];
    for (name, body) in cases {
        let dir = TempDir::new(name);
        let path = dir.file("s.jsonl");
        write_lines(&path, &body);
        // A torn tail must not be truncated before the corruption is reported.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(br#"{"schema_version":1,"session_id":"replace-sid","seq":9"#);
        std::fs::write(&path, &bytes).unwrap();
        let before = std::fs::read(&path).unwrap();

        match SessionJournal::resume(&path, SID) {
            Err(WriteError::Policy(_)) => {}
            Err(other) => panic!("{name}: expected a policy error, got {other:?}"),
            Ok(_) => panic!("{name}: a corrupt file must be refused"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{name}: resume must not touch a corrupt file, not even its torn tail"
        );
    }
}

/// ADR 0019: if writing the abort fails, `resume` returns the error and appends nothing
/// else. With no torn tail the file is byte-for-byte unchanged. The failure is injected at
/// the `Aborted` line via the `testing` feature, so the check is deterministic.
#[cfg(feature = "testing")]
#[tokio::test]
async fn resume_reports_a_failed_abort_write_and_appends_nothing() {
    let dir = TempDir::new("replace-resume-abort-fail");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, ABORT_FAIL_SID, created()).unwrap();
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 4,
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
    drop(journal);

    let before = std::fs::read(&path).unwrap();
    radian_session::fail_next_aborted_write(ABORT_FAIL_SID);
    match SessionJournal::resume(&path, ABORT_FAIL_SID) {
        Err(WriteError::Io { .. }) => {}
        Err(other) => panic!("expected the injected I/O failure, got {other:?}"),
        Ok(_) => panic!("a failed abort write must fail the resume"),
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a failed abort write must leave the file untouched"
    );

    // The seam is one-shot: the next resume (without arming it) writes the abort and succeeds.
    SessionJournal::resume(&path, ABORT_FAIL_SID).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text.matches("\"event\":\"TranscriptReplacementAborted\"")
            .count(),
        1,
        "the un-armed resume writes exactly one abort"
    );
}

//! ADR 0023 U6: the incremental, read-only `JournalTail`.

mod common;

use std::io::Write;
use std::path::Path;

use agent_core::{Message, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{JournalTail, ReadError, Session, SessionJournal};

fn envelope(seq: u64, event: &str, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"tail-sid","seq":{seq},"ts":"2026-10-12T00:00:0{}.000000Z","event":"{event}","critical":true,"data":{data}}}"#,
        seq % 10
    )
}

fn user(text: &str) -> serde_json::Value {
    serde_json::json!({"user": {"content": [{"text": {"text": text}}]}})
}

/// Appends `bytes` to `path`, creating it.
fn append(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
}

/// A deterministic pseudo-random generator, so the seeds and split points are reproducible.
fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// A valid session exercising messages, turns, a committed replacement, a compaction record,
/// a fork record and (sometimes) an uncommitted replacement at the end.
fn generate(seed: u64) -> String {
    let mut state = seed.wrapping_add(1);
    let mut seq = 1u64;
    let mut history = 0usize;
    let mut revision = 0u64;
    let mut lines = vec![envelope(
        0,
        "SessionCreated",
        serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
    )];
    let turns = next(&mut state) % 4;
    for turn in 0..turns {
        lines.push(envelope(
            seq,
            "TurnStarted",
            serde_json::json!({"turn_id": format!("t-{turn}")}),
        ));
        seq += 1;
        let messages = next(&mut state) % 3;
        for m in 0..messages {
            lines.push(envelope(
                seq,
                "MessageAppended",
                serde_json::json!({"index": history, "message": user(&format!("turn {turn} msg {m}"))}),
            ));
            history += 1;
            seq += 1;
        }
        lines.push(envelope(
            seq,
            "TurnFinished",
            serde_json::json!({"turn_id": format!("t-{turn}"), "outcome": "completed"}),
        ));
        seq += 1;
        if next(&mut state).is_multiple_of(3) {
            revision += 1;
            lines.push(envelope(
                seq,
                "CompactionUsageRecorded",
                serde_json::json!({"revision": revision, "model": "current", "usage": {"input": 5}}),
            ));
            seq += 1;
            lines.push(envelope(
                seq,
                "ContextCompacted",
                serde_json::json!({
                    "revision": revision, "source_revision": revision - 1, "source_len": history,
                    "summary": "a summary", "first_kept": 1, "tokens_before": 10,
                    "tokens_after": 5, "below_target": false, "model": "current"
                }),
            ));
            seq += 1;
        }
        if next(&mut state).is_multiple_of(4) {
            lines.push(envelope(
                seq,
                "SessionForked",
                serde_json::json!({"child_session_id": format!("child-{seed}"), "parent_seq": seq}),
            ));
            seq += 1;
        }
    }
    if next(&mut state).is_multiple_of(2) {
        // A committed replacement resets the index to 0.
        revision += 1;
        let len = 1 + (next(&mut state) % 2) as usize;
        lines.push(envelope(
            seq,
            "TranscriptReplaced",
            serde_json::json!({"revision": revision, "len": len}),
        ));
        seq += 1;
        for index in 0..len {
            lines.push(envelope(
                seq,
                "MessageAppended",
                serde_json::json!({"index": index, "message": user("replacement")}),
            ));
            seq += 1;
        }
        lines.push(envelope(
            seq,
            "TranscriptReplacementCommitted",
            serde_json::json!({"revision": revision, "len": len}),
        ));
        seq += 1;
        history = len;
        let _ = history;
    }
    if next(&mut state).is_multiple_of(2) {
        // An uncommitted replacement at the end.
        revision += 1;
        lines.push(envelope(
            seq,
            "TranscriptReplaced",
            serde_json::json!({"revision": revision, "len": 2}),
        ));
        seq += 1;
        lines.push(envelope(
            seq,
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("pending")}),
        ));
    }
    format!("{}\n", lines.join("\n"))
}

/// The fixtures that are valid sessions (not the deliberate corruption/torn-tail ones).
const VALID_FIXTURES: &[&str] = &[
    "v1-minimal.jsonl",
    "v1-forward.jsonl",
    "unknown-optional-field.jsonl",
    "unknown-enum-values.jsonl",
    "memory-injected.jsonl",
    "crash-after-started.jsonl",
    "replace-committed.jsonl",
    "replace-uncommitted-eof.jsonl",
    "replace-aborted-then-append.jsonl",
    "replace-resets-index.jsonl",
    "compaction-committed.jsonl",
    "compaction-candidate-only.jsonl",
    "fork-parent.jsonl",
    "fork-child.jsonl",
];

/// The core equivalence: appending a valid session in arbitrary chunks and polling leaves the
/// tail's history equal to a fresh `Session::open` at every step.
#[test]
fn incremental_polls_equal_a_fresh_open_at_every_split() {
    let dir = TempDir::new("tail-equivalence");
    let mut cases: Vec<(String, String)> = VALID_FIXTURES
        .iter()
        .map(|name| {
            (
                format!("fixture {name}"),
                std::fs::read_to_string(common::fixture(name)).unwrap(),
            )
        })
        .collect();
    for seed in 0..200u64 {
        cases.push((format!("generated {seed}"), generate(seed)));
    }

    for (name, body) in cases {
        let path = dir.file("s.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mut tail = JournalTail::open(&path).unwrap();
        let bytes = body.as_bytes();
        let mut state = 0x1234_5678_9abc_def0u64 ^ body.len() as u64;
        let mut points: Vec<usize> = (0..5)
            .map(|_| 1 + (next(&mut state) as usize % bytes.len().max(2)))
            .collect();
        points.push(bytes.len());
        points.sort_unstable();
        points.dedup();

        let mut written = 0usize;
        for point in points {
            append(&path, &bytes[written..point]);
            written = point;
            let update = tail.poll().unwrap();
            let expected = Session::open(&path).unwrap().history().unwrap();
            assert_eq!(update.history, expected, "{name}: at byte {written}");
            // A split mid-line is a torn tail: the offset stops at the last LF.
            let expected_offset = bytes[..written]
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|i| i as u64 + 1)
                .unwrap_or(0);
            assert_eq!(tail.offset(), expected_offset, "{name}: offset");
        }
        assert_eq!(written, bytes.len());
        // The final state is the whole file.
        assert_eq!(
            tail.poll().unwrap().history,
            Session::open(&path).unwrap().history().unwrap(),
            "{name}: final"
        );
    }
}

#[tokio::test]
async fn a_pending_segment_spans_polls() {
    let dir = TempDir::new("tail-pending");
    let path = dir.file("s.jsonl");
    // A committed base turn.
    let mut body = String::new();
    body.push_str(&envelope(
        0,
        "SessionCreated",
        serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
    ));
    body.push('\n');
    body.push_str(&envelope(
        1,
        "MessageAppended",
        serde_json::json!({"index": 0, "message": user("old")}),
    ));
    body.push('\n');
    append(&path, body.as_bytes());
    let mut tail = JournalTail::open(&path).unwrap();
    assert_eq!(tail.poll().unwrap().history, vec![Message::user("old")]);

    // Open a replacement segment and its first message: the poll still sees the old history.
    append(
        &path,
        format!(
            "{}\n{}\n",
            envelope(
                2,
                "TranscriptReplaced",
                serde_json::json!({"revision": 1, "len": 2})
            ),
            envelope(
                3,
                "MessageAppended",
                serde_json::json!({"index": 0, "message": user("new one")})
            )
        )
        .as_bytes(),
    );
    let update = tail.poll().unwrap();
    assert_eq!(
        update.history,
        vec![Message::user("old")],
        "a pending segment does not change the history"
    );
    assert!(
        update.history_changed,
        "the appended message is a transcript record"
    );

    // Commit it: now the replacement is the history.
    append(
        &path,
        format!(
            "{}\n{}\n",
            envelope(
                4,
                "MessageAppended",
                serde_json::json!({"index": 1, "message": user("new two")})
            ),
            envelope(
                5,
                "TranscriptReplacementCommitted",
                serde_json::json!({"revision": 1, "len": 2})
            )
        )
        .as_bytes(),
    );
    assert_eq!(
        tail.poll().unwrap().history,
        vec![Message::user("new one"), Message::user("new two")]
    );

    // Abort a second segment: the old (now first replacement's) history stands.
    append(
        &path,
        format!(
            "{}\n{}\n{}\n",
            envelope(
                6,
                "TranscriptReplaced",
                serde_json::json!({"revision": 2, "len": 2})
            ),
            envelope(
                7,
                "MessageAppended",
                serde_json::json!({"index": 0, "message": user("discarded")})
            ),
            envelope(
                8,
                "TranscriptReplacementAborted",
                serde_json::json!({"revision": 2})
            )
        )
        .as_bytes(),
    );
    assert_eq!(
        tail.poll().unwrap().history,
        vec![Message::user("new one"), Message::user("new two")]
    );
    // And it agrees with a fresh open.
    assert_eq!(
        tail.poll().unwrap().history,
        Session::open(&path).unwrap().history().unwrap()
    );
}

#[test]
fn a_torn_tail_is_not_read_until_complete() {
    let dir = TempDir::new("tail-torn");
    let path = dir.file("s.jsonl");
    append(
        &path,
        format!(
            "{}\n",
            envelope(
                0,
                "SessionCreated",
                serde_json::json!({"workspace": "/w", "config_snapshot": {}})
            )
        )
        .as_bytes(),
    );
    let mut tail = JournalTail::open(&path).unwrap();
    let base = tail.offset();

    let line = envelope(
        1,
        "MessageAppended",
        serde_json::json!({"index": 0, "message": user("hello")}),
    );
    // Half the line, no LF: not read, offset unchanged.
    append(&path, &line.as_bytes()[..line.len() / 2]);
    let update = tail.poll().unwrap();
    assert!(update.events.is_empty());
    assert_eq!(
        tail.offset(),
        base,
        "a torn tail does not advance the offset"
    );
    assert!(update.history.is_empty());
    // The rest completes it.
    append(&path, format!("{}\n", &line[line.len() / 2..]).as_bytes());
    assert_eq!(tail.poll().unwrap().history, vec![Message::user("hello")]);
}

#[test]
fn a_replaced_or_truncated_file_resets() {
    let dir = TempDir::new("tail-reset");
    let path = dir.file("s.jsonl");
    let first = generate(7);
    append(&path, first.as_bytes());
    let mut tail = JournalTail::open(&path).unwrap();
    assert_eq!(
        tail.poll().unwrap().history,
        Session::open(&path).unwrap().history().unwrap()
    );

    // A shorter file at the same path (truncate) triggers a reset.
    std::fs::write(&path, "").unwrap();
    let update = tail.poll().unwrap();
    assert!(update.reset);
    assert!(update.history.is_empty());

    // A different file renamed over the path triggers a reset too.
    let second = generate(11);
    let other = dir.file("other.jsonl");
    std::fs::write(&other, second.as_bytes()).unwrap();
    // Point the tail at the new inode: write a fresh file at `path` (same inode) would not
    // reset, so exercise the inode change that way (rename over the open path).
    let tail_path = dir.file("replaced.jsonl");
    std::fs::write(&tail_path, first.as_bytes()).unwrap();
    let mut tail = JournalTail::open(&tail_path).unwrap();
    std::fs::rename(&other, &tail_path).unwrap();
    let update = tail.poll().unwrap();
    assert!(update.reset, "a new inode must reset");
    assert_eq!(
        update.history,
        Session::open(&tail_path).unwrap().history().unwrap()
    );
}

#[test]
fn a_corrupt_line_reports_the_same_line_as_session_open() {
    let dir = TempDir::new("tail-corrupt");
    let path = dir.file("s.jsonl");
    std::fs::copy(common::fixture("replace-commit-mismatch.jsonl"), &path).unwrap();
    let expected = match Session::open(&path).unwrap_err() {
        ReadError::Corrupt { line, .. } => line,
        other => panic!("expected corruption, got {other}"),
    };
    match JournalTail::open(&path) {
        Err(ReadError::Corrupt { line, .. }) => assert_eq!(line, expected),
        Err(other) => panic!("expected corruption, got {other}"),
        Ok(_) => panic!("a corrupt file must be refused"),
    }
}

#[test]
fn a_corruption_keeps_polling_as_an_error() {
    let dir = TempDir::new("tail-corrupt-forever");
    let path = dir.file("s.jsonl");
    // A valid first line, opened, then a corrupt complete line appended.
    append(
        &path,
        format!(
            "{}\n",
            envelope(
                0,
                "SessionCreated",
                serde_json::json!({"workspace": "/w", "config_snapshot": {}})
            )
        )
        .as_bytes(),
    );
    let mut tail = JournalTail::open(&path).unwrap();
    assert!(tail.poll().unwrap().events.is_empty());
    append(&path, b"not json\n");
    assert!(tail.poll().is_err());
    assert!(tail.poll().is_err(), "a later poll must keep failing");
}

#[tokio::test]
async fn the_tail_is_read_only_and_needs_no_writer_lock() {
    let dir = TempDir::new("tail-readonly");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "tail-sid", created()).unwrap();
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: Message::user("hello"),
        })
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

    // The writer still holds the exclusive lock; the tail does not take it.
    let mut tail = JournalTail::open(&path).unwrap();
    let update = tail.poll().unwrap();
    assert_eq!(update.history, vec![Message::user("hello")]);

    assert_eq!(std::fs::read(&path).unwrap(), before, "the bytes changed");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before_mtime,
        "the mtime changed"
    );
    drop(journal);
}

#[test]
fn a_bad_seq_line_is_refused_like_open() {
    let dir = TempDir::new("tail-badseq");
    let path = dir.file("s.jsonl");
    append(
        &path,
        format!(
            "{}\n{}\n",
            envelope(
                0,
                "SessionCreated",
                serde_json::json!({"workspace": "/w", "config_snapshot": {}})
            ),
            // seq jumps from 0 to 2 on the second line.
            envelope(
                2,
                "MessageAppended",
                serde_json::json!({"index": 0, "message": user("hello")})
            )
        )
        .as_bytes(),
    );
    let expected = match Session::open(&path).unwrap_err() {
        ReadError::BadSeq { line, .. } => line,
        other => panic!("expected BadSeq, got {other}"),
    };
    match JournalTail::open(&path) {
        Err(ReadError::BadSeq { line, .. }) => assert_eq!(line, expected),
        Err(other) => panic!("expected BadSeq, got {other}"),
        Ok(_) => panic!("a bad-seq line must be refused"),
    }
}

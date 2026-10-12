//! Slice B: the SQLite index is a disposable cache over the JSONL (ADR 0006 §10).
//! These tests pin requirements 1–4 of the task: derivability, rebuild determinism,
//! inconsistency handling, and the torn-tail rule.

mod common;

use std::path::Path;

use agent_core::TranscriptJournal;
use common::{TempDir, fixture};
use radian_session::{Index, Session, SessionJournal};
use serde_json::Value;

/// The v1 event set, copied here so the test derives rows independently of the crate.
const KNOWN: [&str; 16] = [
    "SessionCreated",
    "InboxEnqueued",
    "InputClaimed",
    "InputAdmitted",
    "TurnStarted",
    "MessageAppended",
    "ToolExecutionStarted",
    "ToolExecutionSettled",
    "TranscriptReplaced",
    "TurnFinished",
    "ConfigChanged",
    "ContextCompacted",
    "Rewound",
    "PermissionResolved",
    "ModelUsageRecorded",
    "SessionForked",
];

/// The fixtures a schema-v1 reader accepts (the other two are refused by policy).
fn legal_fixtures() -> Vec<&'static str> {
    vec![
        "v1-minimal.jsonl",
        "v1-forward.jsonl",
        "unknown-optional-field.jsonl",
        "unknown-event-noncritical.jsonl",
        "truncated-tail.jsonl",
        "crash-after-started.jsonl",
        "replace-resets-index.jsonl",
    ]
}

/// An independent re-derivation of every indexed field from the raw file: envelope fields,
/// line offset/length (LF-commit rule), message index, and the visible text of user and
/// assistant messages. Deliberately does not use the crate's parser.
#[allow(clippy::type_complexity)]
fn raw_derive(
    path: &Path,
) -> Vec<(
    u64,
    String,
    String,
    bool,
    u64,
    u64,
    Option<u64>,
    Option<String>,
)> {
    let bytes = std::fs::read(path).unwrap();
    let mut out = Vec::new();
    let mut offset: usize = 0;
    for chunk in bytes.split_inclusive(|b| *b == b'\n') {
        if !chunk.ends_with(b"\n") {
            break; // torn tail: no LF, no commit
        }
        let raw = &chunk[..chunk.len() - 1];
        let value: Value = serde_json::from_slice(raw).unwrap();
        let event = value["event"].as_str().unwrap().to_owned();
        if KNOWN.contains(&event.as_str()) {
            let ts = value["ts"].as_str().unwrap().to_owned();
            let critical = value["critical"].as_bool().unwrap();
            let (message_index, text) = if event == "MessageAppended" {
                (
                    Some(value["data"]["index"].as_u64().unwrap()),
                    visible_text(&value["data"]["message"]),
                )
            } else {
                (None, None)
            };
            out.push((
                value["seq"].as_u64().unwrap(),
                ts,
                event,
                critical,
                offset as u64,
                chunk.len() as u64,
                message_index,
                text,
            ));
        }
        offset += chunk.len();
    }
    out
}

fn visible_text(message: &Value) -> Option<String> {
    // A tool-result message has no `user`/`assistant` key and is not indexed.
    let content = message
        .get("user")
        .or_else(|| message.get("assistant"))
        .and_then(|m| m.get("content"))?;
    let text: Vec<&str> = content
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text")?.get("text")?.as_str())
        .collect();
    let joined = text.join("\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Every column of every indexed row must equal the raw re-derivation, byte for byte.
#[test]
fn every_indexed_field_is_derivable_from_the_jsonl() {
    let dir = TempDir::new("derive");
    let mut index = Index::open(dir.file("index.db")).unwrap();
    let path = fixture("v1-minimal.jsonl");
    index.rebuild(std::slice::from_ref(&path)).unwrap();

    let session = Session::open(&path).unwrap();
    let rows = index.indexed_events(session.session_id()).unwrap();
    let expected = raw_derive(&path);
    assert_eq!(rows.len(), expected.len());
    for (row, (seq, ts, event, critical, offset, len, message_index, text)) in
        rows.iter().zip(expected.iter())
    {
        assert_eq!(row.seq, *seq);
        assert_eq!(&row.ts, ts);
        assert_eq!(&row.event, event);
        assert_eq!(row.critical, *critical);
        assert_eq!(row.byte_offset, *offset);
        assert_eq!(row.line_len, *len);
        assert_eq!(row.message_index, *message_index);
        assert_eq!(row.text, *text);
    }
    // The tool-result message is present but its text is deliberately not searchable.
    let tool_row = rows
        .iter()
        .find(|r| r.message_index == Some(2))
        .expect("message index 2 exists");
    assert_eq!(tool_row.text, None);
}

/// Deleting the database and rebuilding from the JSONL yields identical rows, for every
/// legal fixture.
#[test]
fn a_deleted_index_rebuilds_to_the_same_rows() {
    for name in legal_fixtures() {
        let dir = TempDir::new("rebuild");
        let path = fixture(name);
        let first_rows = {
            let mut index = Index::open(dir.file("first.db")).unwrap();
            let warnings = index.rebuild(std::slice::from_ref(&path)).unwrap();
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
            let session = Session::open(&path).unwrap();
            (
                index.indexed_events(session.session_id()).unwrap(),
                index.list(None).unwrap(),
            )
        };
        // Delete the cache entirely and rebuild into a fresh database.
        std::fs::remove_file(dir.file("first.db")).unwrap_or(());
        let second_rows = {
            let mut index = Index::open(dir.file("second.db")).unwrap();
            index.rebuild(std::slice::from_ref(&path)).unwrap();
            let session = Session::open(&path).unwrap();
            (
                index.indexed_events(session.session_id()).unwrap(),
                index.list(None).unwrap(),
            )
        };
        assert_eq!(first_rows.0, second_rows.0, "{name}: event rows differ");
        assert_eq!(
            first_rows.1, second_rows.1,
            "{name}: session summary differs"
        );

        // The rows agree with the reader's own count of accepted events.
        let session = Session::open(&path).unwrap();
        assert_eq!(second_rows.0.len(), session.events().len(), "{name}");
    }
}

/// A stale/tampered index is detected, rebuilt from the JSONL, and the JSONL is untouched.
#[test]
fn a_stale_index_is_detected_and_rebuilt_from_the_jsonl() {
    let dir = TempDir::new("stale");
    let path = fixture("v1-minimal.jsonl");
    let before = std::fs::read(&path).unwrap();
    let mut index = Index::open(dir.file("index.db")).unwrap();
    index.rebuild(std::slice::from_ref(&path)).unwrap();
    let session = Session::open(&path).unwrap();
    let sid = session.session_id().to_owned();
    assert!(index.audit(&path).unwrap().consistent);

    // Corrupt two rows in the cache (a byte offset and the event count), the way a stale
    // index left from an older file would look.
    let db = rusqlite::Connection::open(dir.file("index.db")).unwrap();
    db.execute(
        "UPDATE events SET byte_offset = byte_offset + 7 WHERE session_id = ?1 AND seq = 1",
        [&sid],
    )
    .unwrap();
    db.execute(
        "UPDATE sessions SET event_count = event_count + 3 WHERE session_id = ?1",
        [&sid],
    )
    .unwrap();
    drop(db);

    let audit = index.audit(&path).unwrap();
    assert!(!audit.consistent);
    assert!(!audit.issues.is_empty(), "issues must explain the mismatch");

    let reconcile = index.reconcile(&path).unwrap();
    assert!(reconcile.rebuilt, "{reconcile:?}");
    assert!(!reconcile.issues.is_empty());
    assert!(
        index.audit(&path).unwrap().consistent,
        "rebuilt from the JSONL"
    );
    assert_eq!(
        index.indexed_events(&sid).unwrap(),
        raw_derive(&path)
            .into_iter()
            .map(
                |(seq, ts, event, critical, offset, len, message_index, text)| {
                    radian_session::IndexedEvent {
                        seq,
                        ts,
                        event,
                        critical,
                        byte_offset: offset,
                        line_len: len,
                        message_index,
                        text,
                    }
                }
            )
            .collect::<Vec<_>>(),
        "the rebuilt rows equal the raw derivation"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the JSONL must never be modified by the index"
    );
}

/// A torn tail is not indexed; truncating (as `resume` does) and appending then rebuilding
/// keeps the index equal to the file.
#[tokio::test]
async fn a_torn_tail_is_not_indexed_and_is_picked_up_after_truncate_and_append() {
    let dir = TempDir::new("torn");
    let path = dir.file("session.jsonl");
    let before = std::fs::read(fixture("truncated-tail.jsonl")).unwrap();
    std::fs::write(&path, &before).unwrap();
    let committed = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap() as u64;
    assert!(
        committed < before.len() as u64,
        "the fixture has a torn tail"
    );

    let sid = "55555555-5555-4555-8555-555555555555";
    let mut index = Index::open(dir.file("index.db")).unwrap();
    index.rebuild(std::slice::from_ref(&path)).unwrap();
    let list = index.list(None).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].file_bytes, committed,
        "torn bytes are not committed"
    );
    assert_eq!(list[0].event_count, 5);
    let rows = index.indexed_events(sid).unwrap();
    assert!(
        rows.iter().all(|r| r.seq <= 4),
        "no event for the torn line"
    );
    assert_eq!(
        rows.iter().map(|r| r.line_len).sum::<u64>(),
        committed,
        "the indexed lines tile the committed prefix"
    );
    assert!(index.audit(&path).unwrap().consistent);

    // Truncate the torn tail and append one message, exactly as a resume would.
    let journal = SessionJournal::resume(&path, sid).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), committed);
    journal
        .append(agent_core::JournalRecord::Message {
            index: 2,
            message: agent_core::Message::assistant("after the tear"),
        })
        .await
        .unwrap();
    drop(journal);

    index.rebuild(std::slice::from_ref(&path)).unwrap();
    assert!(index.audit(&path).unwrap().consistent);
    let list = index.list(None).unwrap();
    assert_eq!(list[0].file_bytes, std::fs::metadata(&path).unwrap().len());
    assert_eq!(list[0].event_count, 6);
    let rows = index.indexed_events(sid).unwrap();
    assert_eq!(
        rows.iter().filter(|r| r.event == "MessageAppended").count(),
        3
    );
}

// ---------- concurrency (req 5) and workspace isolation (req 6) ----------

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

/// Writes a synthetic session file: `SessionCreated` plus `count` user messages.
fn write_synthetic(path: &Path, sid: &str, workspace: &str, count: usize) {
    let mut out = String::new();
    out.push_str(
        &serde_json::json!({
            "schema_version": 1, "session_id": sid, "seq": 0,
            "ts": "2026-10-12T10:00:00.000000Z", "event": "SessionCreated",
            "critical": true,
            "data": {"workspace": workspace, "config_snapshot": {}}
        })
        .to_string(),
    );
    out.push('\n');
    for i in 0..count {
        out.push_str(
            &serde_json::json!({
                "schema_version": 1, "session_id": sid, "seq": i + 1,
                "ts": format!("2026-10-12T10:00:{:02}.000000Z", i % 60),
                "event": "MessageAppended", "critical": true,
                "data": {"index": i, "message": {"user": {"content": [
                    {"text": {"text": format!("message number {i}")}}
                ]}}}
            })
            .to_string(),
        );
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
}

/// WAL and a busy timeout are what let readers and a writer share the index; assert both
/// are actually configured (this is what the concurrency mutations break).
#[test]
fn the_index_uses_wal_and_a_busy_timeout() {
    let dir = TempDir::new("pragma");
    let index = Index::open(dir.file("index.db")).unwrap();
    assert_eq!(index.journal_mode().unwrap().to_ascii_lowercase(), "wal");
    assert!(
        index.busy_timeout().unwrap() >= 1000,
        "busy timeout must be set, got {}",
        index.busy_timeout().unwrap()
    );
}

#[test]
fn two_readers_share_one_index_without_busy_failures() {
    let dir = TempDir::new("readers");
    let db = dir.file("index.db");
    let path = dir.file("s.jsonl");
    write_synthetic(&path, "sid-readers", "/w/a", 20);
    {
        let mut index = Index::open(&db).unwrap();
        index.rebuild(std::slice::from_ref(&path)).unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let db = db.clone();
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let index = Index::open(&db).unwrap();
                barrier.wait();
                assert_eq!(index.list(None).unwrap().len(), 1);
            });
        }
    });
}

/// One writer rebuilding while one reader lists and searches: neither may fail with a busy
/// error. No sleeps; the reader loops until the writer signals completion.
#[test]
fn a_writer_and_a_reader_share_one_index_without_busy_failures() {
    let dir = TempDir::new("writer-reader");
    let db = dir.file("index.db");
    let path = dir.file("s.jsonl");
    write_synthetic(&path, "sid-wr", "/w/a", 4000);
    {
        let mut index = Index::open(&db).unwrap();
        index.rebuild(std::slice::from_ref(&path)).unwrap();
    }
    let done = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let writer = {
            let db = db.clone();
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let mut index = Index::open(&db).unwrap();
                barrier.wait();
                index.rebuild(std::slice::from_ref(&path)).unwrap();
            })
        };
        {
            let db = db.clone();
            let barrier = Arc::clone(&barrier);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                let index = Index::open(&db).unwrap();
                barrier.wait();
                while !done.load(Ordering::SeqCst) {
                    assert_eq!(index.list(None).unwrap().len(), 1);
                    let _ = index.search("message", None, 10).unwrap();
                }
                // One last read after the write committed.
                assert_eq!(index.list(None).unwrap().len(), 1);
            });
        }
        writer.join().unwrap();
        done.store(true, Ordering::SeqCst);
    });
}

#[test]
fn sessions_are_isolated_by_workspace() {
    let dir = TempDir::new("workspace");
    let a = dir.file("a.jsonl");
    let b = dir.file("b.jsonl");
    write_synthetic(&a, "sid-a", "/work/alpha", 2);
    write_synthetic(&b, "sid-b", "/work/beta", 3);
    let mut index = Index::open(dir.file("index.db")).unwrap();
    index.rebuild(&[a, b]).unwrap();

    let alpha = index.list(Some("/work/alpha")).unwrap();
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].session_id, "sid-a");
    assert_eq!(alpha[0].workspace.as_deref(), Some("/work/alpha"));
    assert_eq!(alpha[0].event_count, 3);

    let beta = index.list(Some("/work/beta")).unwrap();
    assert_eq!(beta.len(), 1);
    assert_eq!(beta[0].session_id, "sid-b");

    assert_eq!(index.list(Some("/work/gamma")).unwrap().len(), 0);
    assert_eq!(index.list(None).unwrap().len(), 2);
}

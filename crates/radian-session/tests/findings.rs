//! Phase 7 findings: places where the code and ADR 0006 disagreed, and their regressions.
//!
//! The fuzz and crash tests found these; all four are now fixed and their tests are normal
//! regressions here (the replacement-segment tests live in `replace.rs`).
//!
//! - a marker whose `call_id` no earlier assistant message declares is refused (§7);
//! - `resume` refuses an unparseable line, empty ones included (§3.2-3);
//! - a crash mid-replacement recovers the written prefix (ADR 0005).

mod common;

use agent_core::Message;
use common::TempDir;
use radian_session::{
    ReadError, RecoveredReplacement, Session, SessionCreatedData, SessionJournal,
};
use serde_json::json;

const SID: &str = "finding-sid";

fn envelope(seq: u64, event: &str, critical: bool, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"{SID}","seq":{seq},"ts":"2026-10-12T00:00:0{seq}.000000Z","event":"{event}","critical":{critical},"data":{data}}}"#
    )
}

/// ADR 0006 §7 pseudocode: the event-level validation list says a
/// `ToolExecutionStarted`/`ToolExecutionSettled` `call_id` "必须已出现在更早的 assistant
/// 消息" (must have appeared in an earlier assistant message). A marker for a call no message
/// ever declared is now refused, naming the call_id and the line.
#[test]
fn a_marker_whose_call_id_never_appeared_is_refused() {
    let dir = TempDir::new("finding-stray-marker");
    let path = dir.file("s.jsonl");
    let body = format!(
        "{}\n{}\n",
        envelope(
            0,
            "SessionCreated",
            true,
            json!({"workspace": "/w", "config_snapshot": {}})
        ),
        envelope(
            1,
            "ToolExecutionStarted",
            true,
            json!({"call_id": "ghost", "name": "echo", "arguments": {}})
        ),
    );
    std::fs::write(&path, body).unwrap();

    let error = Session::open(&path).unwrap_err();
    assert!(
        matches!(
            error,
            ReadError::OrphanToolMarker {
                line: 2,
                ref event,
                ref call_id,
                ..
            } if event == "ToolExecutionStarted" && call_id == "ghost"
        ),
        "expected the orphan marker to be named: {error:?}"
    );
}

/// ADR 0006 §3.2-3: a line that fails to parse is corruption, and the append path must refuse
/// it too ("拒绝打开（或拒绝追加）"). `Session::open` refused an interior empty line but
/// `SessionJournal::resume` skipped empty lines while scanning and then appended after them,
/// producing a file the reader would refuse forever. `scan_lines` now parses every
/// LF-terminated line, empty ones included.
#[test]
fn resume_refuses_an_interior_empty_line() {
    let dir = TempDir::new("finding-empty-line");
    let path = dir.file("s.jsonl");
    let journal =
        SessionJournal::create(&path, SID, SessionCreatedData::new("/w", json!({}))).unwrap();
    drop(journal); // release the single-writer lock
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"\n").unwrap();
    }

    // The reader refuses it...
    assert!(
        Session::open(&path).is_err(),
        "the reader accepted an empty line"
    );
    // ...and so does the append path, naming the line.
    let error = match SessionJournal::resume(&path, SID) {
        Ok(_) => panic!("resume accepted a file whose line 2 is not JSON"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            radian_session::WriteError::Policy(ReadError::Corrupt { line: 2, .. })
        ),
        "resume must refuse the unparseable line: {error:?}"
    );
}

/// A crash between `TranscriptReplaced` and its messages leaves a committed marker whose
/// replacement is short. ADR 0005 line 92 makes the new replacement's written prefix the
/// recoverable state, so the read succeeds with that prefix and reports the diagnostic. The
/// torn tail (the half-written second message) is dropped first.
#[test]
fn a_crash_mid_replacement_is_recovered_as_the_written_prefix() {
    let dir = TempDir::new("finding-mid-replacement");
    let path = dir.file("s.jsonl");
    let marker = envelope(
        1,
        "TranscriptReplaced",
        true,
        json!({"revision": 1, "len": 2}),
    );
    let first = envelope(
        2,
        "MessageAppended",
        true,
        json!({"index": 0, "message": {"user": {"content": [{"text": {"text": "a"}}]}}}),
    );
    // The second replacement message was being written when the process died: a valid-JSON
    // prefix with no LF. It is a torn tail and is dropped, leaving 1 of the declared 2.
    let second = envelope(
        3,
        "MessageAppended",
        true,
        json!({"index": 1, "message": {"user": {"content": [{"text": {"text": "b"}}]}}}),
    );
    let torn = &second[..second.len() - 5];
    let body = format!(
        "{}\n{marker}\n{first}\n{torn}",
        envelope(
            0,
            "SessionCreated",
            true,
            json!({"workspace": "/w", "config_snapshot": {}})
        )
    );
    std::fs::write(&path, body).unwrap();

    let session = Session::open(&path).unwrap();
    assert_eq!(session.history().unwrap(), vec![Message::user("a")]);
    assert_eq!(
        session.recovered_replacements(),
        &[RecoveredReplacement {
            revision: 1,
            declared: 2,
            written: 1,
        }]
    );
    // The half-written message was dropped, not counted.
    assert_eq!(session.events().len(), 3);
}

/// A short segment followed by another `TranscriptReplaced` is the crash-recovery flow: the
/// host recovered the written prefix and replaced again. The read must succeed, report the
/// short segment, and fold to the last revision plus what follows it.
#[test]
fn a_short_replacement_followed_by_another_replacement_reads_back() {
    let dir = TempDir::new("finding-short-then-marker");
    let path = dir.file("s.jsonl");
    let body = [
        envelope(
            0,
            "SessionCreated",
            true,
            json!({"workspace": "/w", "config_snapshot": {}}),
        ),
        envelope(
            1,
            "TranscriptReplaced",
            true,
            json!({"revision": 1, "len": 2}),
        ),
        envelope(
            2,
            "MessageAppended",
            true,
            json!({"index": 0, "message": {"user": {"content": [{"text": {"text": "prefix"}}]}}}),
        ),
        envelope(
            3,
            "TranscriptReplaced",
            true,
            json!({"revision": 2, "len": 1}),
        ),
        envelope(
            4,
            "MessageAppended",
            true,
            json!({"index": 0, "message": {"user": {"content": [{"text": {"text": "second"}}]}}}),
        ),
        envelope(
            5,
            "MessageAppended",
            true,
            json!({"index": 1, "message": {"assistant": {"content": [{"text": {"text": "grown"}}]}}}),
        ),
    ]
    .join("\n");
    std::fs::write(&path, format!("{body}\n")).unwrap();

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        vec![Message::user("second"), Message::assistant("grown")]
    );
    assert_eq!(
        session.recovered_replacements(),
        &[RecoveredReplacement {
            revision: 1,
            declared: 2,
            written: 1,
        }]
    );
}

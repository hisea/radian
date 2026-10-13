//! Phase 7 findings: places where the code and ADR 0006 disagreed, and their regressions.
//!
//! The fuzz and crash tests found these; all four are now fixed and their tests are normal
//! regressions here (the replacement-segment tests live in `replace.rs`).
//!
//! - a marker whose `call_id` no earlier assistant message declares is refused (§7);
//! - `resume` refuses an unparseable line, empty ones included (§3.2-3);
//! - a crash mid-replacement recovers the **old history** and a second marker inside a pending
//!   segment is corruption (ADR 0019, which supersedes ADR 0005's written-prefix rule).

mod common;

use agent_core::Message;
use common::TempDir;
use radian_session::{
    ReadError, ReplacementEnd, Session, SessionCreatedData, SessionJournal, UncommittedReplacement,
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
    std::fs::write(&path, &body).unwrap();

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

/// A crash between `TranscriptReplaced` and its messages leaves a **pending** segment. ADR
/// 0019: an uncommitted segment is dropped and the history stays the one before it (the file
/// is the only place a crash can leave a pending segment). The torn tail is dropped first.
#[test]
fn a_crash_mid_replacement_recovers_the_old_history() {
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
    std::fs::write(&path, &body).unwrap();

    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.history().unwrap(),
        Vec::<Message>::new(),
        "an uncommitted replacement is dropped; the old history (empty here) stands"
    );
    assert_eq!(
        session.uncommitted_replacements(),
        &[UncommittedReplacement {
            revision: 1,
            declared_len: 2,
            written: 1,
            line: 2,
            end: ReplacementEnd::EndOfFile,
        }]
    );
    // The half-written message was dropped, not counted.
    assert_eq!(session.events().len(), 3);
    // A read-only open never writes: the bytes are exactly what the test wrote.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
}

/// A second `TranscriptReplaced` inside a pending segment is corruption (ADR 0019 §2): the
/// boundary must be written to disk as a commit or an abort, not inferred from "an event
/// appeared". The old rule accepted this as crash recovery.
#[test]
fn a_second_replacement_inside_a_pending_segment_is_refused() {
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
    ]
    .join("\n");
    std::fs::write(&path, format!("{body}\n")).unwrap();

    let error = Session::open(&path).unwrap_err();
    assert!(
        matches!(&error, ReadError::Corrupt { line: 4, .. }),
        "a second marker without a commit/abort is corruption at line 4: {error:?}"
    );
}

//! Phase 7 findings: places where the code and ADR 0006 disagree.
//!
//! The fuzz and crash tests found these; they are reproduced here minimally. The two
//! `#[ignore]`d tests assert what the ADR requires and currently **fail** — they are the
//! reproduction, not a regression guard. Run them with `--ignored`:
//!
//! ```sh
//! cargo test -p radian-session --test findings -- --ignored
//! ```

mod common;

use common::TempDir;
use radian_session::{ReadError, Session, SessionCreatedData, SessionJournal};
use serde_json::json;

const SID: &str = "finding-sid";

fn envelope(seq: u64, event: &str, critical: bool, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"{SID}","seq":{seq},"ts":"2026-10-12T00:00:0{seq}.000000Z","event":"{event}","critical":{critical},"data":{data}}}"#
    )
}

/// Finding 1 (ADR 0006 §7 pseudocode): the event-level validation list says a
/// `ToolExecutionStarted`/`ToolExecutionSettled` `call_id` "必须已出现在更早的 assistant
/// 消息" (must have appeared in an earlier assistant message). The reader does not check it:
/// a marker for a call no message ever declared is accepted, not reported as corruption.
#[test]
#[ignore = "finding: ADR 0006 §7 requires rejecting a marker whose call_id never appeared; the reader accepts it"]
fn a_marker_whose_call_id_never_appeared_should_be_refused() {
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

    let result = Session::open(&path);
    assert!(
        result.is_err(),
        "the reader accepted a marker whose call_id no assistant message declares: {result:?}"
    );
}

/// Finding 2 (ADR 0006 §3.2-3): a line that fails to parse is corruption, and the append path
/// must refuse too ("拒绝打开（或拒绝追加）"). `Session::open` refuses an interior empty line,
/// but `SessionJournal::resume` skips empty lines while scanning and then appends after them,
/// producing a file the reader will refuse forever.
#[test]
#[ignore = "finding: ADR 0006 §3.2-3 requires refusing to append to a file with an unparseable line; resume ignores an empty line"]
fn resume_should_refuse_an_interior_empty_line() {
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

    // The reader already refuses it...
    assert!(
        Session::open(&path).is_err(),
        "the reader accepted an empty line"
    );
    // ...so the append path must refuse it too, instead of appending after the corruption.
    let resume = SessionJournal::resume(&path, SID);
    assert!(
        resume.is_err(),
        "resume accepted a file whose line 2 is not JSON and would append after it"
    );
}

/// Finding 3 (design question, ADR 0005): `replace_transcript` journals one
/// `TranscriptReplaced { len }` marker and then `len` messages. A crash between them leaves a
/// committed marker whose replacement is short. The reader refuses the whole file with
/// `BadReplacementLen` rather than recovering the previous history or the partial
/// replacement. This test pins the current behavior; whether it should recover is the owner's
/// call (it is not an ADR violation, the ADR is silent on a crash mid-replacement).
#[test]
fn a_crash_mid_replacement_is_refused_as_an_incomplete_replacement() {
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

    let error = Session::open(&path).unwrap_err();
    assert!(
        matches!(
            error,
            ReadError::BadReplacementLen {
                len: 2,
                count: 1,
                ..
            }
        ),
        "expected the incomplete replacement to be named: {error:?}"
    );
}

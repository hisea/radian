//! ADR 0043 slice ① (the observer half): the read-only observer of committed facts, built on
//! `radian-session`'s `JournalTail` (ADR 0023 U6).
//!
//! Each test names the mutation it catches where the ADR's test list requires one.

use std::io::Write;
use std::path::{Path, PathBuf};

use radian_core::Message;
use radian_host::{JournalObserver, SessionCapability, SessionObserver};
use radian_session::Session;

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "radian-host-observer-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One envelope line, LF-terminated by the caller's `join("\n")`.
fn envelope(seq: u64, ts: &str, event: &str, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"observer-sid","seq":{seq},"ts":"{ts}","event":"{event}","critical":true,"data":{data}}}"#
    )
}

fn user(text: &str) -> serde_json::Value {
    serde_json::json!({"user": {"content": [{"text": {"text": text}}]}})
}

fn assistant_with_calls(text: &str, calls: &[(&str, &str)]) -> serde_json::Value {
    let tool_calls: Vec<serde_json::Value> = calls
        .iter()
        .map(|(id, name)| serde_json::json!({"id": id, "name": name, "arguments": {}}))
        .collect();
    serde_json::json!({"assistant": {
        "content": [{"text": {"text": text}}],
        "tool_calls": tool_calls,
    }})
}

fn tool_result(call_id: &str, is_error: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "call_id": call_id,
        "content": [{"text": {"text": "done"}}],
        "is_error": is_error,
    });
    if is_error {
        value["error_code"] = serde_json::json!("tool_failed");
    }
    value
}

fn session_created() -> String {
    envelope(
        0,
        "2026-10-15T00:00:00.000000Z",
        "SessionCreated",
        serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
    )
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

/// The fixtures that are valid sessions (not the deliberate corruption/torn-tail ones); the
/// same list as `radian-session/tests/tail.rs`.
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

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/schema/fixtures")
        .join(name)
}

/// The core equivalence, through the observer: appending a valid session one byte at a time and
/// polling leaves the observer's committed history equal to a fresh `Session::open` at every
/// split.
///
/// Catches a mutation that would let the observer diverge from the reader (for example a
/// hand-rolled history fold that does not apply the ADR 0019 pending-segment rule).
#[test]
fn incremental_polls_equal_a_fresh_open_at_every_split() {
    let dir = Scratch::new("every-split");
    for name in VALID_FIXTURES {
        let body = std::fs::read(fixture(name)).unwrap();
        let path = dir.file("s.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mut observer = JournalObserver::open(&path).unwrap();

        for (written, byte) in body.iter().enumerate() {
            append(&path, &[*byte]);
            observer.poll().unwrap();
            let fresh = Session::open(&path).unwrap().history().unwrap();
            assert_eq!(
                observer.history().to_vec(),
                fresh,
                "{name}: at byte {}",
                written + 1
            );
        }
    }
}

/// A pending (uncommitted) replacement is not shown (ADR 0019/0023 U6).
///
/// Catches a mutation that shows a pending segment (a fold that applies `MessageAppended`
/// inside a segment instead of waiting for the commit).
#[test]
fn a_pending_replacement_is_not_shown() {
    let dir = Scratch::new("pending");
    let path = dir.file("s.jsonl");
    let body = format!(
        "{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:01.000000Z",
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("old")})
        ),
    );
    std::fs::write(&path, body).unwrap();
    let mut observer = JournalObserver::open(&path).unwrap();
    assert_eq!(observer.history().to_vec(), [Message::user("old")]);

    // Open a replacement segment and its first message. The segment has not committed, so the
    // old history stands.
    let pending = format!(
        "{}\n{}\n",
        envelope(
            2,
            "2026-10-15T00:00:02.000000Z",
            "TranscriptReplaced",
            serde_json::json!({"revision": 1, "len": 2})
        ),
        envelope(
            3,
            "2026-10-15T00:00:03.000000Z",
            "MessageAppended",
            serde_json::json!({"index": 0, "message": user("pending")})
        ),
    );
    append(&path, pending.as_bytes());
    let update = observer.poll().unwrap();
    assert_eq!(observer.history().to_vec(), [Message::user("old")]);
    assert_eq!(
        update.history,
        [Message::user("old")],
        "the tail and the observer agree"
    );

    // Committing it swaps the history in.
    let commit = format!(
        "{}\n{}\n",
        envelope(
            4,
            "2026-10-15T00:00:04.000000Z",
            "MessageAppended",
            serde_json::json!({"index": 1, "message": user("committed")})
        ),
        envelope(
            5,
            "2026-10-15T00:00:05.000000Z",
            "TranscriptReplacementCommitted",
            serde_json::json!({"revision": 1, "len": 2})
        ),
    );
    append(&path, commit.as_bytes());
    observer.poll().unwrap();
    assert_eq!(
        observer.history().to_vec(),
        [Message::user("pending"), Message::user("committed")]
    );
}

/// A torn tail is not read until its line is complete.
#[test]
fn a_torn_tail_is_not_read_until_complete() {
    let dir = Scratch::new("torn");
    let path = dir.file("s.jsonl");
    std::fs::write(&path, format!("{}\n", session_created())).unwrap();
    let mut observer = JournalObserver::open(&path).unwrap();
    let base = observer.offset();

    let line = envelope(
        1,
        "2026-10-15T00:00:01.000000Z",
        "MessageAppended",
        serde_json::json!({"index": 0, "message": user("hello")}),
    );
    append(&path, &line.as_bytes()[..line.len() / 2]);
    let update = observer.poll().unwrap();
    assert!(update.events.is_empty());
    assert_eq!(observer.offset(), base, "a torn tail does not advance");
    assert!(observer.history().is_empty());

    append(&path, format!("{}\n", &line[line.len() / 2..]).as_bytes());
    observer.poll().unwrap();
    assert_eq!(observer.history().to_vec(), [Message::user("hello")]);
}

/// A replaced or truncated file resets the reader **and the facts**.
///
/// Catches a mutation that ignores the reset: the old file's usage would survive into the new
/// one (`13` instead of `3`).
#[test]
fn a_replaced_or_truncated_file_resets() {
    let dir = Scratch::new("reset");
    let path = dir.file("s.jsonl");

    // Session A: turn started, usage 10.
    let a = format!(
        "{}\n{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:01.000000Z",
            "TurnStarted",
            serde_json::json!({"turn_id": "t-a"})
        ),
        envelope(
            2,
            "2026-10-15T00:00:02.000000Z",
            "ModelUsageRecorded",
            serde_json::json!({"usage": {"input": 10}})
        ),
    );
    std::fs::write(&path, a).unwrap();
    let mut observer = JournalObserver::open(&path).unwrap();
    assert_eq!(observer.projection().usage_input(), Some(10));
    assert_eq!(
        observer.last_event_ts(),
        Some("2026-10-15T00:00:02.000000Z")
    );

    // Truncating the file resets: every fact is gone.
    std::fs::write(&path, b"").unwrap();
    let update = observer.poll().unwrap();
    assert!(update.reset);
    assert!(observer.history().is_empty());
    assert_eq!(observer.projection().usage_input(), None);
    assert_eq!(observer.projection().turn(), radian_host::TurnState::Idle);
    assert_eq!(observer.last_event_ts(), None);
    assert_eq!(observer.last_write_ts(), None);

    // A different inode renamed over the path resets and rebuilds from the new file.
    let b = format!(
        "{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:09.000000Z",
            "ModelUsageRecorded",
            serde_json::json!({"usage": {"input": 3}})
        ),
    );
    let other = dir.file("other.jsonl");
    std::fs::write(&other, b).unwrap();
    std::fs::rename(&other, &path).unwrap();
    let update = observer.poll().unwrap();
    assert!(update.reset, "a new inode must reset");
    assert_eq!(
        observer.projection().usage_input(),
        Some(3),
        "the new file's facts, not the old file's"
    );
    assert_eq!(
        observer.last_event_ts(),
        Some("2026-10-15T00:00:09.000000Z")
    );
}

/// The two stall inputs are the last committed event and the last **successful** `write`/`patch`
/// (the observer stall inputs).
#[test]
fn the_last_event_and_last_write_times_are_committed_facts() {
    let dir = Scratch::new("last-write");
    let path = dir.file("s.jsonl");

    // An assistant message declaring two writes; the first settles ok, the second fails.
    let head = format!(
        "{}\n{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:01.000000Z",
            "TurnStarted",
            serde_json::json!({"turn_id": "t"})
        ),
        envelope(
            2,
            "2026-10-15T00:00:02.000000Z",
            "MessageAppended",
            serde_json::json!({"index": 0, "message": assistant_with_calls("editing",
                &[("c1", "write"), ("c2", "write")])})
        ),
    );
    std::fs::write(&path, head).unwrap();
    let mut observer = JournalObserver::open(&path).unwrap();
    assert_eq!(observer.last_write_ts(), None, "no write has settled yet");

    let first = format!(
        "{}\n{}\n",
        envelope(
            3,
            "2026-10-15T00:00:03.000000Z",
            "ToolExecutionStarted",
            serde_json::json!({"call_id": "c1", "name": "write", "arguments": {}})
        ),
        envelope(
            4,
            "2026-10-15T00:00:04.000000Z",
            "ToolExecutionSettled",
            serde_json::json!({"call_id": "c1", "result": tool_result("c1", false)})
        ),
    );
    append(&path, first.as_bytes());
    let update = observer.poll().unwrap();
    assert!(!update.events.is_empty());
    assert_eq!(
        observer.last_write_ts(),
        Some("2026-10-15T00:00:04.000000Z"),
        "a successful write sets it"
    );
    assert_eq!(
        observer.last_event_ts(),
        Some("2026-10-15T00:00:04.000000Z")
    );

    // A failed write does not: only the event time moves.
    let failed = format!(
        "{}\n{}\n",
        envelope(
            5,
            "2026-10-15T00:00:05.000000Z",
            "ToolExecutionStarted",
            serde_json::json!({"call_id": "c2", "name": "write", "arguments": {}})
        ),
        envelope(
            6,
            "2026-10-15T00:00:06.000000Z",
            "ToolExecutionSettled",
            serde_json::json!({"call_id": "c2", "result": tool_result("c2", true)})
        ),
    );
    append(&path, failed.as_bytes());
    observer.poll().unwrap();
    assert_eq!(
        observer.last_write_ts(),
        Some("2026-10-15T00:00:04.000000Z"),
        "a failed write leaves the last successful write alone"
    );
    assert_eq!(
        observer.last_event_ts(),
        Some("2026-10-15T00:00:06.000000Z")
    );

    // A successful non-write tool does not either.
    let shell = format!(
        "{}\n{}\n{}\n",
        envelope(
            7,
            "2026-10-15T00:00:07.000000Z",
            "MessageAppended",
            serde_json::json!({"index": 1, "message": assistant_with_calls("checking",
                &[("c3", "shell")])})
        ),
        envelope(
            8,
            "2026-10-15T00:00:08.000000Z",
            "ToolExecutionStarted",
            serde_json::json!({"call_id": "c3", "name": "shell", "arguments": {}})
        ),
        envelope(
            9,
            "2026-10-15T00:00:09.000000Z",
            "ToolExecutionSettled",
            serde_json::json!({"call_id": "c3", "result": tool_result("c3", false)})
        ),
    );
    append(&path, shell.as_bytes());
    observer.poll().unwrap();
    assert_eq!(
        observer.last_write_ts(),
        Some("2026-10-15T00:00:04.000000Z"),
        "only write/patch count as a write"
    );
    assert_eq!(
        observer.last_event_ts(),
        Some("2026-10-15T00:00:09.000000Z")
    );
}

/// An incremental poll folds only the **new** events; a poll with nothing new changes nothing.
///
/// Catches a mutation that folds the whole journal from scratch on every poll, which would add
/// the same usage again (`20` instead of `10`).
#[test]
fn an_incremental_poll_does_not_refold_the_projection() {
    let dir = Scratch::new("no-refold");
    let path = dir.file("s.jsonl");
    let a = format!(
        "{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:01.000000Z",
            "ModelUsageRecorded",
            serde_json::json!({"usage": {"input": 10}})
        ),
    );
    std::fs::write(&path, a).unwrap();
    let mut observer = JournalObserver::open(&path).unwrap();
    assert_eq!(observer.projection().usage_input(), Some(10));

    let update = observer.poll().unwrap();
    assert!(update.events.is_empty());
    assert_eq!(
        observer.projection().usage_input(),
        Some(10),
        "a poll with no new events does not fold again"
    );

    append(
        &path,
        format!(
            "{}\n",
            envelope(
                2,
                "2026-10-15T00:00:02.000000Z",
                "ModelUsageRecorded",
                serde_json::json!({"usage": {"input": 5}})
            )
        )
        .as_bytes(),
    );
    observer.poll().unwrap();
    assert_eq!(observer.projection().usage_input(), Some(15));

    // And again with nothing new: still 15, not 30.
    observer.poll().unwrap();
    assert_eq!(observer.projection().usage_input(), Some(15));
}

/// The observer is the read-only half of the facade: it declares only `Observe`, and it folds
/// the committed facts into the projection.
#[test]
fn the_observer_declares_only_observe_and_folds_the_facts() {
    let dir = Scratch::new("observe-only");
    let path = dir.file("s.jsonl");
    let body = format!(
        "{}\n{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:01.000000Z",
            "TurnStarted",
            serde_json::json!({"turn_id": "t"})
        ),
        envelope(
            2,
            "2026-10-15T00:00:02.000000Z",
            "MessageAppended",
            serde_json::json!({"index": 0, "message": assistant_with_calls("working",
                &[("c1", "read")])})
        ),
    );
    std::fs::write(&path, body).unwrap();
    let mut observer = JournalObserver::open(&path).unwrap();
    assert!(matches!(
        observer.capabilities(),
        [SessionCapability::Observe]
    ));
    assert_eq!(
        observer.projection().turn(),
        radian_host::TurnState::Running
    );

    append(
        &path,
        format!(
            "{}\n{}\n",
            envelope(
                3,
                "2026-10-15T00:00:03.000000Z",
                "ToolExecutionStarted",
                serde_json::json!({"call_id": "c1", "name": "read", "arguments": {}})
            ),
            envelope(
                4,
                "2026-10-15T00:00:04.000000Z",
                "TurnFinished",
                serde_json::json!({"turn_id": "t", "outcome": "completed"})
            ),
        )
        .as_bytes(),
    );
    observer.poll().unwrap();
    let read = observer.projection();
    assert_eq!(read.turn(), radian_host::TurnState::Idle);
    let tool = read.last_tool().expect("the last tool is a fact");
    assert_eq!(tool.name, "read");
    assert_eq!(tool.call_id, "c1");
    assert_eq!(tool.index, 0);
}

/// A usage record's own fee is authoritative (ADR 0007): the observer folds it, and a
/// record without one adds tokens only.
///
/// Catches a mutation that drops the recorded fee (the cost would stay `None`).
#[test]
fn a_recorded_fee_is_folded_and_an_old_record_adds_tokens_only() {
    let dir = Scratch::new("recorded-fee");
    let path = dir.file("s.jsonl");
    let body = format!(
        "{}\n{}\n{}\n",
        session_created(),
        envelope(
            1,
            "2026-10-15T00:00:01.000000Z",
            "ModelUsageRecorded",
            serde_json::json!({"usage": {"input": 10}, "fee": {"micros": 42}})
        ),
        envelope(
            2,
            "2026-10-15T00:00:02.000000Z",
            "ModelUsageRecorded",
            serde_json::json!({"usage": {"input": 5}})
        ),
    );
    std::fs::write(&path, body).unwrap();
    let observer = JournalObserver::open(&path).unwrap();
    assert_eq!(observer.projection().usage_input(), Some(15));
    assert_eq!(observer.projection().cost_micros(), Some(42));
    assert!(!observer.projection().cost_estimated());
}

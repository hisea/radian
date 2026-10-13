//! ADR 0021 M1 #4 (crash): a torn tail is not committed, a malformed line names its line, and
//! the unknown-event policy matches ADR 0006 §7.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};

use common::TempDir;
use radian_memory::{Kind, MemoryError, MemoryStore, Scope, Source};
use serde_json::json;

fn global() -> Scope {
    Scope::Global
}

fn log_path(dir: &TempDir) -> PathBuf {
    dir.path().join("global").join("log.jsonl")
}

fn append_raw(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
}

fn record(store: &MemoryStore, key: &str, content: &str) -> radian_memory::MemoryRecord {
    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some(key.into()),
            content,
            radian_memory::Provenance::default(),
        )
        .unwrap()
}

#[test]
fn a_torn_tail_is_not_committed() {
    let dir = TempDir::new("crash-torn");
    let store = MemoryStore::open(dir.path());
    let first = record(&store, "k", "keep");

    // A crash mid-write: a valid-looking prefix with no LF. Even though it could parse, it is
    // not committed.
    append_raw(
        &log_path(&dir),
        br#"{"schema_version":1,"seq":1,"ts":"TORN_MARKER","event":"MemoryForgotten"#,
    );

    let reopened = MemoryStore::open(dir.path());
    let records = reopened.records(&global()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, first.id);

    // The next append does not follow the half line.
    record(&reopened, "k2", "next");
    let again = MemoryStore::open(dir.path());
    assert_eq!(again.records(&global()).unwrap().len(), 2);
    let raw = std::fs::read_to_string(log_path(&dir)).unwrap();
    assert!(
        !raw.contains("TORN_MARKER"),
        "the torn bytes were kept: {raw}"
    );
}

#[test]
fn a_corrupt_line_names_its_line() {
    let dir = TempDir::new("crash-corrupt");
    let store = MemoryStore::open(dir.path());
    record(&store, "k", "body");
    append_raw(&log_path(&dir), b"{not json}\n");

    let error = store.records(&global()).unwrap_err();
    assert!(
        matches!(error, MemoryError::Corrupt { line: 2, .. }),
        "{error}"
    );
}

#[test]
fn an_unknown_critical_event_refuses_the_log() {
    let dir = TempDir::new("crash-unknown-critical");
    let store = MemoryStore::open(dir.path());
    record(&store, "k", "body");
    let line = json!({
        "schema_version": 1,
        "seq": 1,
        "ts": "2026-10-13T00:00:00.000000Z",
        "event": "FutureCritical",
        "critical": true,
        "data": {}
    });
    append_raw(&log_path(&dir), format!("{line}\n").as_bytes());

    let error = store.records(&global()).unwrap_err();
    assert!(
        matches!(
            &error,
            MemoryError::UnknownCritical { line: 2, event, .. } if event == "FutureCritical"
        ),
        "{error}"
    );
}

#[test]
fn an_unknown_noncritical_event_is_skipped_and_the_seq_continues() {
    let dir = TempDir::new("crash-unknown-noncritical");
    let store = MemoryStore::open(dir.path());
    record(&store, "k", "body");
    let line = json!({
        "schema_version": 1,
        "seq": 1,
        "ts": "2026-10-13T00:00:00.000000Z",
        "event": "FutureAudit",
        "critical": false,
        "data": {"note": "future"}
    });
    append_raw(&log_path(&dir), format!("{line}\n").as_bytes());

    // The skipped line is not an error and does not change the state...
    assert_eq!(store.records(&global()).unwrap().len(), 1);
    // ...but it still occupies its seq: the next append is seq 2.
    record(&store, "k2", "next");
    let raw = std::fs::read_to_string(log_path(&dir)).unwrap();
    let last: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    assert_eq!(last["seq"], json!(2), "{last}");
}

#[test]
fn a_bad_seq_is_refused() {
    let dir = TempDir::new("crash-bad-seq");
    let store = MemoryStore::open(dir.path());
    record(&store, "k", "body");
    let line = json!({
        "schema_version": 1,
        "seq": 5,
        "ts": "2026-10-13T00:00:00.000000Z",
        "event": "MemoryForgotten",
        "critical": true,
        "data": {"id": "x", "at": "2026-10-13T00:00:00.000000Z"}
    });
    append_raw(&log_path(&dir), format!("{line}\n").as_bytes());
    let error = store.records(&global()).unwrap_err();
    assert!(
        matches!(
            &error,
            MemoryError::BadSeq {
                line: 2,
                expected: 1,
                found: 5,
                ..
            }
        ),
        "{error}"
    );
}

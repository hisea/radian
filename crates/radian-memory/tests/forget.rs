//! ADR 0021 M1 #5/#7 (Forget and Purge): a forgotten record disappears from every view, does
//! not come back on replay, and Purge physically removes its content atomically.

mod common;

use common::TempDir;
use radian_memory::{Kind, MemoryStore, PurgeStep, Scope, SearchQuery, Source, search};
use serde_json::json;

fn global() -> Scope {
    Scope::Global
}

fn log_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("global").join("log.jsonl")
}

#[test]
fn forget_removes_the_record_from_every_view() {
    let dir = TempDir::new("forget-view");
    let store = MemoryStore::open(dir.path());
    let record = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "TOP-SECRET-CONTENT",
            radian_memory::Provenance::default(),
        )
        .unwrap();
    store.forget(&global(), Source::User, &record.id).unwrap();

    assert!(store.records(&global()).unwrap().is_empty());
    assert!(
        !store
            .export(&global())
            .unwrap()
            .contains("TOP-SECRET-CONTENT")
    );
    let view = store.view(&[global()]).unwrap();
    assert!(search(&view, &SearchQuery::default()).is_empty());
}

#[test]
fn a_forgotten_record_stays_gone_after_reopening() {
    let dir = TempDir::new("forget-reopen");
    let id = {
        let store = MemoryStore::open(dir.path());
        let record = store
            .create(
                &global(),
                Source::User,
                Kind::Fact,
                Some("k".into()),
                "gone",
                radian_memory::Provenance::default(),
            )
            .unwrap();
        store.forget(&global(), Source::User, &record.id).unwrap();
        record.id
    };
    // A fresh store replays the same log: `MemoryForgotten` still removes it.
    let reopened = MemoryStore::open(dir.path());
    assert!(reopened.records(&global()).unwrap().is_empty());
    assert!(
        search(
            &reopened.view(&[global()]).unwrap(),
            &SearchQuery::default()
        )
        .iter()
        .all(|record| record.id != id)
    );
}

#[test]
fn purge_removes_the_forgotten_content_bytes() {
    let dir = TempDir::new("forget-purge-bytes");
    let store = MemoryStore::open(dir.path());
    let record = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "PURGED-SECRET-CONTENT",
            radian_memory::Provenance::default(),
        )
        .unwrap();
    store.forget(&global(), Source::User, &record.id).unwrap();
    assert!(
        std::fs::read_to_string(log_path(&dir))
            .unwrap()
            .contains("PURGED-SECRET-CONTENT"),
        "before purging, the log still has the content"
    );

    let removed = store.purge(&global()).unwrap();
    assert_eq!(removed, 1);
    let after = std::fs::read_to_string(log_path(&dir)).unwrap();
    assert!(
        !after.contains("PURGED-SECRET-CONTENT"),
        "purge must remove the content bytes: {after}"
    );
    assert!(after.contains("StorePurged"), "{after}");
}

#[test]
fn purge_is_atomic_at_every_step() {
    for step in [
        PurgeStep::TempWritten,
        PurgeStep::TempFsynced,
        PurgeStep::Renamed,
        PurgeStep::DirFsynced,
    ] {
        let dir = TempDir::new("forget-purge-atomic");
        let store = MemoryStore::open(dir.path());
        let record = store
            .create(
                &global(),
                Source::User,
                Kind::Fact,
                Some("k".into()),
                "ATOMIC-SECRET",
                radian_memory::Provenance::default(),
            )
            .unwrap();
        store.forget(&global(), Source::User, &record.id).unwrap();

        store.purge_with_fault(&global(), Some(step)).unwrap();

        // Whatever the crash point, the log at the canonical path is intact and replayable.
        let reopened = MemoryStore::open(dir.path());
        assert!(
            reopened.records(&global()).unwrap().is_empty(),
            "step {step:?}: the forgotten record must not be in the state"
        );
        let raw = std::fs::read_to_string(log_path(&dir)).unwrap();
        match step {
            // Before the rename the old log is still in place.
            PurgeStep::TempWritten | PurgeStep::TempFsynced => {
                assert!(
                    raw.contains("ATOMIC-SECRET"),
                    "step {step:?}: the old log must be the one at the path"
                );
            }
            // After the rename the new, content-free log is at the path.
            PurgeStep::Renamed | PurgeStep::DirFsynced => {
                assert!(
                    !raw.contains("ATOMIC-SECRET"),
                    "step {step:?}: the new log must be the one at the path"
                );
            }
        }
    }
}

#[test]
fn a_purged_log_replays_the_live_records() {
    let dir = TempDir::new("forget-purge-live");
    let store = MemoryStore::open(dir.path());
    let keep = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("keep".into()),
            "kept",
            radian_memory::Provenance::default(),
        )
        .unwrap();
    let drop = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("drop".into()),
            "dropped",
            radian_memory::Provenance::default(),
        )
        .unwrap();
    store.forget(&global(), Source::User, &drop.id).unwrap();
    store.purge(&global()).unwrap();

    let reopened = MemoryStore::open(dir.path());
    let records = reopened.records(&global()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, keep.id);
    assert_eq!(records[0].content, "kept");
    // The rewritten log ends with the audit line.
    let raw = std::fs::read_to_string(log_path(&dir)).unwrap();
    let last: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], json!("StorePurged"));
    assert!(last["ts"].as_str().is_some());
}

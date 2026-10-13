//! ADR 0021 M1 #11 (Markdown): export → edit → import; a revision mismatch is a conflict and
//! writes nothing; a malformed block is refused with its position; a new id becomes an
//! `Import` record.

mod common;

use common::TempDir;
use radian_memory::{Kind, MemoryError, MemoryStore, Provenance, Scope, Source};

fn global() -> Scope {
    Scope::Global
}

#[test]
fn export_then_import_applies_matching_revisions() {
    let dir = TempDir::new("markdown-roundtrip");
    let store = MemoryStore::open(dir.path());
    store
        .create(
            &global(),
            Source::User,
            Kind::Preference,
            Some("style".into()),
            "indent with tabs",
            Provenance::default(),
        )
        .unwrap();

    let text = store.export(&global()).unwrap();
    assert!(text.contains("revision: 1"), "{text}");
    assert!(text.contains("kind: preference"), "{text}");
    assert!(text.contains("key: style"), "{text}");
    assert!(text.contains("source: user"), "{text}");
    assert!(text.contains("verification: confirmed"), "{text}");
    assert!(text.contains("status: active"), "{text}");

    let edited = text.replace("indent with tabs", "indent with spaces");
    let report = store.import(&global(), &edited).unwrap();
    assert_eq!(report.revised.len(), 1, "{report:?}");
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert!(report.created.is_empty(), "{report:?}");

    let records = store.records(&global()).unwrap();
    assert_eq!(records[0].content, "indent with spaces");
    assert_eq!(records[0].revision, 2);
    // The original source is preserved.
    assert_eq!(records[0].source, Source::User);
}

#[test]
fn a_revision_mismatch_is_a_conflict_and_writes_nothing() {
    let dir = TempDir::new("markdown-conflict");
    let store = MemoryStore::open(dir.path());
    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "original",
            Provenance::default(),
        )
        .unwrap();

    let mut text = store.export(&global()).unwrap();
    text = text.replace("revision: 1", "revision: 99");
    text = text.replace("original", "clobbered");
    let report = store.import(&global(), &text).unwrap();
    assert_eq!(report.revised.len(), 0, "{report:?}");
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert_eq!(report.conflicts[0].expected, 99);
    assert_eq!(report.conflicts[0].found, 1);

    let records = store.records(&global()).unwrap();
    assert_eq!(records[0].content, "original", "a conflict must not write");
    assert_eq!(records[0].revision, 1);
}

#[test]
fn a_malformed_block_is_refused_with_a_position() {
    let dir = TempDir::new("markdown-malformed");
    let store = MemoryStore::open(dir.path());

    // Missing `revision`.
    let err = store
        .import(&global(), "---\nid: mem-x\nkind: fact\n---\nbody\n")
        .unwrap_err();
    assert!(
        matches!(&err, MemoryError::Markdown { line: 1, message, .. } if message.contains("revision")),
        "{err}"
    );

    // A front-matter line without a colon.
    let err = store
        .import(&global(), "---\nid: mem-x\nnot-a-pair\n---\nbody\n")
        .unwrap_err();
    assert!(
        matches!(&err, MemoryError::Markdown { line: 3, message, .. } if message.contains("key: value")),
        "{err}"
    );

    // An unclosed front matter.
    let err = store
        .import(&global(), "---\nid: mem-x\nrevision: 1\n")
        .unwrap_err();
    assert!(
        matches!(&err, MemoryError::Markdown { message, .. } if message.contains("closed")),
        "{err}"
    );

    // Nothing was written by any of the failed imports.
    assert!(store.records(&global()).unwrap().is_empty());
}

#[test]
fn an_imported_new_id_creates_an_import_record() {
    let dir = TempDir::new("markdown-new");
    let store = MemoryStore::open(dir.path());
    let text =
        "---\nid: mem-imported-1\nrevision: 1\nkind: fact\nkey: imported\n---\nhello from a file\n";
    let report = store.import(&global(), text).unwrap();
    assert_eq!(report.created.len(), 1, "{report:?}");

    let records = store.records(&global()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id.as_str(), "mem-imported-1");
    assert_eq!(records[0].source, Source::Import);
    assert_eq!(records[0].content, "hello from a file");
    assert_eq!(records[0].revision, 1);

    // Re-importing the same text now matches the revision and becomes a revision, not a
    // duplicate.
    let again = store.import(&global(), text).unwrap();
    assert_eq!(again.revised.len(), 1, "{again:?}");
    assert_eq!(store.records(&global()).unwrap().len(), 1);
}

//! ADR 0021 D3: the one-time migration of an old `log.jsonl` into record files and an index.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use common::TempDir;
use radian_memory::{
    FileStore, Kind, ListEntry, MemoryError, MemoryRecord, MemoryStore, MovePhase, Provenance,
    Scope, Source, migrate_log,
};

fn global() -> Scope {
    Scope::Global
}

fn create(store: &MemoryStore, key: Option<&str>, content: &str) -> MemoryRecord {
    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            key.map(str::to_owned),
            content,
            Provenance::default(),
        )
        .unwrap()
}

fn names(store: &FileStore) -> Vec<String> {
    store
        .list(&global())
        .unwrap()
        .into_iter()
        .map(|entry| match entry {
            ListEntry::Record(frontmatter) => frontmatter.name,
            ListEntry::Unreadable { path, reason } => {
                panic!("unreadable record {}: {reason}", path.display())
            }
        })
        .collect()
}

fn snapshot(dir: &std::path::Path, names: &[&str]) -> Vec<Vec<u8>> {
    names
        .iter()
        .map(|name| std::fs::read(dir.join(name)).unwrap())
        .collect()
}

fn snapshot_with_times(
    dir: &std::path::Path,
    names: &[&str],
) -> Vec<(Vec<u8>, Option<SystemTime>)> {
    names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            (
                std::fs::read(&path).unwrap(),
                std::fs::metadata(&path).unwrap().modified().ok(),
            )
        })
        .collect()
}

#[test]
fn two_active_one_archived_and_one_forgotten_become_two_files_and_an_index() {
    let dir = TempDir::new("migrate-basic");
    let old = MemoryStore::open(dir.path());
    create(&old, Some("alpha"), "Alpha body");
    create(&old, Some("beta"), "Beta body");
    let archived = create(&old, Some("archived"), "Archived body");
    old.archive(&global(), Source::User, &archived.id, archived.revision)
        .unwrap();
    let forgotten = create(&old, Some("forgotten"), "Forgotten body");
    old.forget(&global(), Source::User, &forgotten.id).unwrap();

    let scope_dir = dir.path().join("global");
    migrate_log(&scope_dir).unwrap();

    let store = FileStore::open(dir.path());
    assert_eq!(names(&store), ["alpha", "beta"]);
    assert!(!store.record_path(&global(), "archived").exists());
    assert!(!store.record_path(&global(), "forgotten").exists());
    assert!(!scope_dir.join("log.jsonl").exists());
    assert!(scope_dir.join("log.jsonl.migrated").exists());

    let index = store.read_index(&global()).unwrap();
    let lines: Vec<&str> = index.lines().collect();
    assert_eq!(lines.len(), 2, "{index}");
    assert!(
        lines.iter().any(|line| line.contains("[alpha](alpha.md)")),
        "{index}"
    );
    assert!(
        lines.iter().any(|line| line.contains("[beta](beta.md)")),
        "{index}"
    );

    // A second call finds the marker and changes nothing, mtimes included.
    let files = ["alpha.md", "beta.md", "MEMORY.md"];
    let before = snapshot_with_times(&scope_dir, &files);
    migrate_log(&scope_dir).unwrap();
    let after = snapshot_with_times(&scope_dir, &files);
    assert_eq!(before, after, "a second migration must be a no-op");
}

#[test]
fn two_keys_that_map_to_the_same_name_get_a_suffix() {
    let dir = TempDir::new("migrate-collision");
    let old = MemoryStore::open(dir.path());
    // The key does not fit the name rule, so the content supplies the slug `a-b`; the other
    // key fits and is already `a-b`.
    create(&old, Some("a b"), "a b");
    create(&old, Some("a-b"), "another body");

    let scope_dir = dir.path().join("global");
    migrate_log(&scope_dir).unwrap();

    let store = FileStore::open(dir.path());
    let listed: BTreeSet<String> = names(&store).into_iter().collect();
    assert_eq!(
        listed,
        BTreeSet::from(["a-b".to_owned(), "a-b-2".to_owned()]),
        "the second record gets the -2 suffix"
    );
    assert_eq!(store.read_index(&global()).unwrap().lines().count(), 2);
    for name in &listed {
        let record = store.read(&global(), name).unwrap();
        assert_eq!(record.frontmatter.name, *name);
    }
}

#[test]
fn an_interrupted_migration_writes_the_same_files_on_the_next_call() {
    let dir = TempDir::new("migrate-crash");
    let old = MemoryStore::open(dir.path());
    create(&old, Some("alpha"), "Alpha body");
    create(&old, Some("beta"), "Beta body");
    let scope_dir = dir.path().join("global");

    // Crash after the files and the index exist, before the log rename.
    let fired = Arc::new(AtomicBool::new(false));
    {
        let fired = Arc::clone(&fired);
        radian_memory::set_move_hook(Box::new(move |phase| {
            phase == MovePhase::MigrationBeforeLogRename && !fired.swap(true, Ordering::SeqCst)
        }));
    }
    let error = migrate_log(&scope_dir).unwrap_err();
    assert!(matches!(error, MemoryError::SimulatedCrash), "{error}");
    assert!(scope_dir.join("log.jsonl").exists());
    assert!(!scope_dir.join("log.jsonl.migrated").exists());
    let files = ["alpha.md", "beta.md", "MEMORY.md"];
    let first = snapshot(&scope_dir, &files);

    // The hook has fired once; the next call completes and writes the same bytes.
    migrate_log(&scope_dir).unwrap();
    assert!(!scope_dir.join("log.jsonl").exists());
    assert!(scope_dir.join("log.jsonl.migrated").exists());
    let second = snapshot(&scope_dir, &files);
    assert_eq!(first, second, "the interrupted writes are idempotent");
}

#[test]
fn an_old_agent_source_becomes_model() {
    let dir = TempDir::new("migrate-source");
    let old = MemoryStore::open(dir.path());
    old.create(
        &global(),
        Source::Agent,
        Kind::Lesson,
        Some("lesson".to_owned()),
        "Lesson body",
        Provenance::default(),
    )
    .unwrap();

    migrate_log(dir.path().join("global")).unwrap();

    let store = FileStore::open(dir.path());
    let record = store.read(&global(), "lesson").unwrap();
    assert_eq!(record.frontmatter.source, "model");
    assert_eq!(record.frontmatter.record_type, "feedback");
}

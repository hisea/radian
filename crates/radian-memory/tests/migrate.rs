//! ADR 0021 D3: the one-time migration of an old `log.jsonl` into record files and an index.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::TempDir;
use radian_memory::{FileStore, ListEntry, MemoryError, Scope, migrate_log};
use serde_json::{Value, json};

fn global() -> Scope {
    Scope::Global
}

fn record(id: &str, key: Option<&str>, content: &str) -> Value {
    json!({
        "id": id,
        "scope": "global",
        "kind": "fact",
        "key": key,
        "content": content,
        "source": "user",
        "provenance": {},
        "verification": "unverified",
        "status": "active",
        "created_at": "2026-10-01T00:00:00.000000Z",
        "updated_at": "2026-10-01T00:00:00.000000Z",
        "revision": 1,
    })
}

fn event(seq: u64, name: &str, data: Value) -> String {
    json!({
        "schema_version": 1,
        "seq": seq,
        "ts": "2026-10-01T00:00:00.000000Z",
        "event": name,
        "critical": name != "StorePurged",
        "data": data,
    })
    .to_string()
}

fn write_log(dir: &std::path::Path, lines: &[String]) {
    std::fs::create_dir_all(dir).unwrap();
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(dir.join("log.jsonl"), text).unwrap();
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

#[test]
fn two_active_one_archived_and_one_forgotten_become_two_files_and_an_index() {
    let dir = TempDir::new("migrate-basic");
    let scope_dir = dir.path().join("global");
    write_log(
        &scope_dir,
        &[
            event(
                0,
                "MemoryCreated",
                json!({"record": record("m1", Some("alpha"), "Alpha body")}),
            ),
            event(
                1,
                "MemoryCreated",
                json!({"record": record("m2", Some("beta"), "Beta body")}),
            ),
            event(
                2,
                "MemoryCreated",
                json!({"record": record("m3", Some("archived"), "Archived body")}),
            ),
            event(
                3,
                "MemoryArchived",
                json!({"id": "m3", "revision": 2, "at": "2026-10-02T00:00:00.000000Z"}),
            ),
            event(
                4,
                "MemoryCreated",
                json!({"record": record("m4", Some("forgotten"), "Forgotten body")}),
            ),
            event(
                5,
                "MemoryForgotten",
                json!({"id": "m4", "at": "2026-10-02T00:00:00.000000Z"}),
            ),
        ],
    );
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

    // A second call finds the marker and changes nothing.
    let files = ["alpha.md", "beta.md", "MEMORY.md"];
    let before = snapshot(&scope_dir, &files);
    migrate_log(&scope_dir).unwrap();
    let after = snapshot(&scope_dir, &files);
    assert_eq!(before, after, "a second migration must be a no-op");
}

#[test]
fn two_keys_that_map_to_the_same_name_get_a_suffix() {
    let dir = TempDir::new("migrate-collision");
    let scope_dir = dir.path().join("global");
    // The key does not fit the name rule, so the content supplies the slug `a-b`; the other
    // key fits and is already `a-b`.
    write_log(
        &scope_dir,
        &[
            event(
                0,
                "MemoryCreated",
                json!({"record": record("m1", Some("a b"), "a b")}),
            ),
            event(
                1,
                "MemoryCreated",
                json!({"record": record("m2", Some("a-b"), "another body")}),
            ),
        ],
    );
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
    let scope_dir = dir.path().join("global");
    write_log(
        &scope_dir,
        &[
            event(
                0,
                "MemoryCreated",
                json!({"record": record("m1", Some("alpha"), "Alpha body")}),
            ),
            event(
                1,
                "MemoryCreated",
                json!({"record": record("m2", Some("beta"), "Beta body")}),
            ),
        ],
    );

    // Crash after the files and the index exist, before the log rename.
    let fired = Arc::new(AtomicBool::new(false));
    {
        let fired = Arc::clone(&fired);
        radian_memory::set_migration_crash_hook(Box::new(move || {
            assert!(!fired.swap(true, Ordering::SeqCst), "the hook fires once");
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
fn a_description_is_the_first_content_line_cut_at_120_characters() {
    let dir = TempDir::new("migrate-description");
    let scope_dir = dir.path().join("global");
    let long = format!("{}z\nsecond line", "x".repeat(130));
    write_log(
        &scope_dir,
        &[
            event(
                0,
                "MemoryCreated",
                json!({"record": record("m1", None, "  padded line  \nsecond line")}),
            ),
            event(
                1,
                "MemoryCreated",
                json!({"record": record("m2", None, &long)}),
            ),
        ],
    );
    migrate_log(&scope_dir).unwrap();

    let store = FileStore::open(dir.path());
    let descriptions: Vec<String> = store
        .list(&global())
        .unwrap()
        .into_iter()
        .map(|entry| match entry {
            ListEntry::Record(frontmatter) => frontmatter.description,
            ListEntry::Unreadable { path, reason } => {
                panic!("unreadable record {}: {reason}", path.display())
            }
        })
        .collect();
    assert!(
        descriptions.iter().any(|line| line == "  padded line  "),
        "the first line keeps its leading and trailing spaces: {descriptions:?}"
    );
    let truncated: String = long.chars().take(120).collect();
    assert!(
        descriptions.contains(&truncated),
        "the first line is cut at 120 characters: {descriptions:?}"
    );
}

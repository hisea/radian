//! ADR 0021 M1 #3 (multi-writer): two real processes write the same scope; the single-writer
//! lock serializes them, so a concurrent create of one key yields exactly one `KeyConflict`
//! and a concurrent revise yields exactly one `RevisionConflict`, with no lost updates.
//!
//! The child is this same test binary re-invoked with `--exact child_worker`; it writes its
//! outcome to a file (never a pipe).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use common::TempDir;
use radian_memory::{Kind, MemoryError, MemoryStore, Provenance, Scope, Source};

fn global() -> Scope {
    Scope::Global
}

fn spawn_child(op: &str, role: &str, root: &Path, out: &Path, extra: &[(&str, &str)]) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "child_worker", "--nocapture"])
        .env("MEM_TEST_ROOT", root)
        .env("MEM_TEST_OP", op)
        .env("MEM_TEST_ROLE", role)
        .env("MEM_TEST_OUT", out)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in extra {
        command.env(name, value);
    }
    command.spawn().unwrap()
}

fn outcome(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("no child outcome at {}: {e}", path.display()))
        .trim()
        .to_owned()
}

#[test]
fn two_processes_creating_distinct_keys_do_not_lose_updates() {
    let dir = TempDir::new("mp-distinct");
    let out_a = dir.path().join("a.out");
    let out_b = dir.path().join("b.out");
    let mut a = spawn_child("create_distinct", "a", dir.path(), &out_a, &[]);
    let mut b = spawn_child("create_distinct", "b", dir.path(), &out_b, &[]);
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert_eq!(outcome(&out_a), "created 10");
    assert_eq!(outcome(&out_b), "created 10");

    let store = MemoryStore::open(dir.path());
    let records = store.records(&global()).unwrap();
    assert_eq!(
        records.len(),
        20,
        "every create from both processes must land"
    );
}

#[test]
fn two_processes_creating_the_same_key_yield_one_conflict() {
    let dir = TempDir::new("mp-same-key");
    let out_a = dir.path().join("a.out");
    let out_b = dir.path().join("b.out");
    let mut a = spawn_child("create_same_key", "a", dir.path(), &out_a, &[]);
    let mut b = spawn_child("create_same_key", "b", dir.path(), &out_b, &[]);
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    let results = [outcome(&out_a), outcome(&out_b)];
    assert_eq!(
        results.iter().filter(|r| r.starts_with("ok")).count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results.iter().filter(|r| *r == "conflict").count(),
        1,
        "{results:?}"
    );

    let store = MemoryStore::open(dir.path());
    let records = store.records(&global()).unwrap();
    assert_eq!(records.len(), 1, "one Active record holds the key");
    assert_eq!(records[0].key.as_deref(), Some("shared"));
}

#[test]
fn two_processes_revising_the_same_revision_yield_one_conflict() {
    let dir = TempDir::new("mp-same-revision");
    let store = MemoryStore::open(dir.path());
    let record = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "v1",
            Provenance::default(),
        )
        .unwrap();
    let out_a = dir.path().join("a.out");
    let out_b = dir.path().join("b.out");
    let extra = [("MEM_TEST_ID", record.id.as_str()), ("MEM_TEST_REV", "1")];
    let mut a = spawn_child("revise_same_revision", "a", dir.path(), &out_a, &extra);
    let mut b = spawn_child("revise_same_revision", "b", dir.path(), &out_b, &extra);
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    let results = [outcome(&out_a), outcome(&out_b)];
    assert_eq!(
        results.iter().filter(|r| r.starts_with("ok")).count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results.iter().filter(|r| *r == "conflict").count(),
        1,
        "{results:?}"
    );

    let current = store.records(&global()).unwrap();
    assert_eq!(current[0].revision, 2, "exactly one revision landed");
    assert!(current[0].content.starts_with("revised by "), "{current:?}");
}

/// Child process for the multi-writer tests. No-op unless spawned with `MEM_TEST_ROOT`.
#[test]
fn child_worker() {
    let Some(root) = std::env::var_os("MEM_TEST_ROOT") else {
        return;
    };
    let op = std::env::var("MEM_TEST_OP").unwrap();
    let role = std::env::var("MEM_TEST_ROLE").unwrap();
    let out = PathBuf::from(std::env::var("MEM_TEST_OUT").unwrap());
    let store = MemoryStore::open(root);
    let result = match op.as_str() {
        "create_distinct" => {
            for i in 0..10 {
                store
                    .create(
                        &global(),
                        Source::Agent,
                        Kind::Fact,
                        Some(format!("{role}-{i}")),
                        format!("{role} {i}"),
                        Provenance::default(),
                    )
                    .unwrap();
            }
            "created 10".to_owned()
        }
        "create_same_key" => match store.create(
            &global(),
            Source::Agent,
            Kind::Fact,
            Some("shared".into()),
            format!("by {role}"),
            Provenance::default(),
        ) {
            Ok(record) => format!("ok {}", record.id),
            Err(MemoryError::KeyConflict { .. }) => "conflict".to_owned(),
            Err(error) => format!("error {error}"),
        },
        "revise_same_revision" => {
            let id = radian_memory::MemoryId(std::env::var("MEM_TEST_ID").unwrap());
            let revision: u64 = std::env::var("MEM_TEST_REV").unwrap().parse().unwrap();
            match store.revise(
                &global(),
                Source::User,
                &id,
                revision,
                format!("revised by {role}"),
            ) {
                Ok(record) => format!("ok {}", record.revision),
                Err(MemoryError::RevisionConflict { .. }) => "conflict".to_owned(),
                Err(error) => format!("error {error}"),
            }
        }
        other => format!("unknown op {other}"),
    };
    std::fs::write(out, result).unwrap();
}

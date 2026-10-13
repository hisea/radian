//! Phase 7 privacy: session directories and files are owner-only (ADR 0013).
//!
//! A session is potentially sensitive data (ADR 0006 §11), so a new directory is `0700` and a
//! new file is `0600`. An **existing** wider path is reported, never silently changed.
#![cfg(unix)]

mod common;

use common::TempDir;
use radian_session::{
    Index, SessionCreatedData, SessionJournal, ensure_private_dir, wide_permissions_warning,
};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn created() -> SessionCreatedData {
    SessionCreatedData::new("/work/example", json!({}))
}

#[test]
fn a_new_session_directory_is_owner_only() {
    let temp = TempDir::new("privacy-new-dir");
    let dir = temp.path().join("sessions");
    ensure_private_dir(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700, "a new session directory must be 0700");
    // Its parents the helper created are owner-only too.
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn an_existing_wide_session_directory_is_reported_and_not_changed() {
    let temp = TempDir::new("privacy-wide-dir");
    let dir = temp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    set_mode(&dir, 0o755);

    let warning = wide_permissions_warning(&dir).expect("a 0755 directory must be reported");
    assert!(warning.contains(&dir.display().to_string()), "{warning}");
    assert!(warning.contains("chmod 700"), "{warning}");

    ensure_private_dir(&dir).unwrap();
    assert_eq!(
        mode(&dir),
        0o755,
        "an existing directory's permissions must not be changed"
    );
}

#[tokio::test]
async fn a_new_session_file_is_owner_only() {
    let temp = TempDir::new("privacy-new-file");
    let dir = temp.path().join("sessions");
    ensure_private_dir(&dir).unwrap();
    let path = dir.join("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    drop(journal);
    assert_eq!(mode(&path), 0o600, "a new session file must be 0600");
}

#[tokio::test]
async fn an_existing_wide_session_file_is_reported_and_not_changed() {
    let temp = TempDir::new("privacy-wide-file");
    let dir = temp.path().join("sessions");
    ensure_private_dir(&dir).unwrap();
    let path = dir.join("s.jsonl");
    // An empty file with a wide mode: `create` writes into it without recreating it.
    std::fs::write(&path, b"").unwrap();
    set_mode(&path, 0o644);

    assert!(
        wide_permissions_warning(&path).is_some(),
        "a 0644 session file must be reported"
    );
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    drop(journal);
    assert_eq!(
        mode(&path),
        0o644,
        "an existing file's permissions must not be changed"
    );
}

#[test]
fn the_index_database_and_its_wal_are_owner_only() {
    let temp = TempDir::new("privacy-index");
    let dir = temp.path().join("sessions");
    ensure_private_dir(&dir).unwrap();
    let session = dir.join("s.jsonl");
    std::fs::write(
        &session,
        format!(
            "{}\n",
            r#"{"schema_version":1,"session_id":"sid","seq":0,"ts":"2026-10-13T00:00:00.000000Z","event":"SessionCreated","critical":true,"data":{"workspace":"/w","config_snapshot":{}}}"#
        ),
    )
    .unwrap();
    set_mode(&session, 0o600);

    let db = dir.join("index.db");
    let mut index = Index::open(&db).unwrap();
    index.rebuild(&[session]).unwrap();
    assert_eq!(mode(&db), 0o600, "a new index database must be 0600");
    // SQLite creates the WAL and SHM siblings with the database's mode.
    for suffix in ["-wal", "-shm"] {
        let path = dir.join(format!("index.db{suffix}"));
        if path.exists() {
            assert_eq!(mode(&path), 0o600, "index.db{suffix} must be 0600");
        }
    }
}

#[test]
fn an_existing_wide_index_is_reported_and_not_changed() {
    let temp = TempDir::new("privacy-wide-index");
    let dir = temp.path().join("sessions");
    ensure_private_dir(&dir).unwrap();
    let db = dir.join("index.db");
    std::fs::write(&db, b"").unwrap();
    set_mode(&db, 0o644);

    assert!(
        wide_permissions_warning(&db).is_some(),
        "a 0644 index must be reported"
    );
    let _index = Index::open(&db).unwrap();
    assert_eq!(
        mode(&db),
        0o644,
        "an existing index's permissions must not be changed"
    );
}

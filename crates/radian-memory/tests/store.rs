//! ADR 0021 M1 #2/#3/#8 (store): the source rules, key/revision conflicts, confirm, and scope
//! isolation.

mod common;

use common::TempDir;
use radian_memory::{
    Confirmer, Kind, MemoryError, MemoryStore, ProjectId, Provenance, Scope, Source, Status,
    Verification,
};

fn global() -> Scope {
    Scope::Global
}

fn project(id: &str) -> Scope {
    Scope::Project(ProjectId(id.into()))
}

fn no_provenance() -> Provenance {
    Provenance::default()
}

#[test]
fn a_user_record_is_confirmed_on_write() {
    let dir = TempDir::new("store-user");
    let store = MemoryStore::open(dir.path());
    let record = store
        .create(
            &global(),
            Source::User,
            Kind::Preference,
            Some("style".into()),
            "tabs, not spaces",
            no_provenance(),
        )
        .unwrap();
    assert_eq!(record.source, Source::User);
    assert!(record.verification.is_confirmed(), "{record:?}");
    assert_eq!(record.status, Status::Active);
    assert_eq!(record.revision, 1);
}

#[test]
fn an_agent_create_on_a_user_key_is_a_key_conflict() {
    let dir = TempDir::new("store-agent-key");
    let store = MemoryStore::open(dir.path());
    let user = store
        .create(
            &global(),
            Source::User,
            Kind::Preference,
            Some("style".into()),
            "tabs",
            no_provenance(),
        )
        .unwrap();

    let error = store
        .create(
            &global(),
            Source::Agent,
            Kind::Preference,
            Some("style".into()),
            "spaces",
            no_provenance(),
        )
        .unwrap_err();
    assert!(
        matches!(
            &error,
            MemoryError::KeyConflict { key, id, revision }
                if key == "style" && *id == user.id && *revision == 1
        ),
        "{error}"
    );
    // The user record is untouched.
    let records = store.records(&global()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].content, "tabs");
}

#[test]
fn the_same_key_cannot_be_created_twice_in_a_scope() {
    let dir = TempDir::new("store-dup-key");
    let store = MemoryStore::open(dir.path());
    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "one",
            no_provenance(),
        )
        .unwrap();
    let error = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "two",
            no_provenance(),
        )
        .unwrap_err();
    assert!(matches!(error, MemoryError::KeyConflict { .. }), "{error}");
}

#[test]
fn an_agent_cannot_revise_archive_or_forget_a_user_record() {
    let dir = TempDir::new("store-protected");
    let store = MemoryStore::open(dir.path());
    let user = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "original",
            no_provenance(),
        )
        .unwrap();

    let revise = store
        .revise(&global(), Source::Agent, &user.id, user.revision, "changed")
        .unwrap_err();
    assert!(
        matches!(&revise, MemoryError::SourceProtected { id, actor: Source::Agent, existing: Source::User } if *id == user.id),
        "{revise}"
    );
    let archive = store
        .archive(&global(), Source::Agent, &user.id, user.revision)
        .unwrap_err();
    assert!(
        matches!(archive, MemoryError::SourceProtected { .. }),
        "{archive}"
    );
    let forget = store
        .forget(&global(), Source::Agent, &user.id)
        .unwrap_err();
    assert!(
        matches!(forget, MemoryError::SourceProtected { .. }),
        "{forget}"
    );

    // A user revision is still allowed.
    let revised = store
        .revise(&global(), Source::User, &user.id, user.revision, "changed")
        .unwrap();
    assert_eq!(revised.revision, 2);
}

#[test]
fn confirm_keeps_the_source_and_records_who_and_when() {
    let dir = TempDir::new("store-confirm");
    let store = MemoryStore::open(dir.path());
    let agent = store
        .create(
            &global(),
            Source::Agent,
            Kind::Fact,
            Some("k".into()),
            "agent wrote this",
            no_provenance(),
        )
        .unwrap();
    assert!(!agent.verification.is_confirmed());

    let confirmed = store
        .confirm(&global(), &agent.id, Confirmer::User, agent.revision)
        .unwrap();
    assert_eq!(confirmed.source, Source::Agent, "source must not change");
    match &confirmed.verification {
        Verification::Confirmed { by, at } => {
            assert_eq!(*by, Confirmer::User);
            assert!(!at.is_empty());
        }
        other => panic!("expected Confirmed, got {other:?}"),
    }
    assert_eq!(confirmed.revision, agent.revision + 1);
    assert_eq!(confirmed.content, agent.content);
}

#[test]
fn a_stale_revision_does_not_overwrite() {
    let dir = TempDir::new("store-revision");
    let store = MemoryStore::open(dir.path());
    let first = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "v1",
            no_provenance(),
        )
        .unwrap();
    let second = store
        .revise(&global(), Source::User, &first.id, 1, "v2")
        .unwrap();
    assert_eq!(second.revision, 2);

    let error = store
        .revise(&global(), Source::User, &first.id, 1, "v3")
        .unwrap_err();
    assert!(
        matches!(
            &error,
            MemoryError::RevisionConflict {
                expected: 1,
                found: 2,
                ..
            }
        ),
        "{error}"
    );
    let current = store.records(&global()).unwrap();
    assert_eq!(current[0].content, "v2", "the stale write must not land");
}

#[test]
fn archive_hides_and_restore_brings_back() {
    let dir = TempDir::new("store-archive");
    let store = MemoryStore::open(dir.path());
    let record = store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "body",
            no_provenance(),
        )
        .unwrap();

    let archived = store
        .archive(&global(), Source::User, &record.id, record.revision)
        .unwrap();
    assert_eq!(archived.status, Status::Archived);
    assert_eq!(archived.revision, 2);
    assert!(!store.records(&global()).unwrap()[0].is_active());

    let restored = store
        .restore(&global(), Source::User, &record.id, archived.revision)
        .unwrap();
    assert_eq!(restored.status, Status::Active);
    assert_eq!(restored.revision, 3);
    assert!(store.records(&global()).unwrap()[0].is_active());
}

#[test]
fn scopes_are_isolated() {
    let dir = TempDir::new("store-scopes");
    let store = MemoryStore::open(dir.path());
    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "global body",
            no_provenance(),
        )
        .unwrap();
    store
        .create(
            &project("p1"),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "project body",
            no_provenance(),
        )
        .unwrap();
    store
        .create(
            &project("p2"),
            Source::User,
            Kind::Fact,
            Some("k".into()),
            "other body",
            no_provenance(),
        )
        .unwrap();

    let global_records = store.records(&global()).unwrap();
    let p1 = store.records(&project("p1")).unwrap();
    let p2 = store.records(&project("p2")).unwrap();
    assert_eq!(global_records[0].content, "global body");
    assert_eq!(p1[0].content, "project body");
    assert_eq!(p2[0].content, "other body");

    // The combined view keeps both; Project overrides Global only for a *shared key*.
    let view = store.view(&[global(), project("p1")]).unwrap();
    assert_eq!(view.len(), 1, "{view:?}");
    assert_eq!(view[0].content, "project body");
}

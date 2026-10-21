//! ADR 0021 #1 (registry): an identity key resolves to one stable `ProjectId`; relink
//! attaches a key to an existing project; different keys stay independent.

mod common;

use common::TempDir;
use radian_memory::{MemoryError, ProjectId, ProjectRegistry};

#[test]
fn the_same_identity_key_always_resolves_to_the_same_project() {
    let dir = TempDir::new("registry-stable");
    let registry = ProjectRegistry::open(dir.path());

    let project = registry.register("/work/repo/.git").unwrap();
    assert_eq!(
        registry.resolve("/work/repo/.git").unwrap(),
        Some(project.clone())
    );
    assert_eq!(registry.register("/work/repo/.git").unwrap(), project);

    // A fresh registry over the same files resolves the same id (the map is durable).
    let reopened = ProjectRegistry::open(dir.path());
    assert_eq!(reopened.resolve("/work/repo/.git").unwrap(), Some(project));
}

#[test]
fn different_keys_do_not_interfere() {
    let dir = TempDir::new("registry-distinct");
    let registry = ProjectRegistry::open(dir.path());

    let a = registry.register("/work/a").unwrap();
    let b = registry.register("/work/b").unwrap();
    assert_ne!(a, b);
    assert_eq!(registry.resolve("/work/a").unwrap(), Some(a.clone()));
    assert_eq!(registry.resolve("/work/b").unwrap(), Some(b.clone()));
    assert_eq!(registry.resolve("/work/c").unwrap(), None);
    let mut projects = registry.projects().unwrap();
    projects.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(projects, expected);
}

#[test]
fn relink_points_a_key_at_an_existing_project() {
    let dir = TempDir::new("registry-relink");
    let registry = ProjectRegistry::open(dir.path());

    let project = registry.register("/work/original").unwrap();
    let other = registry.register("/work/other").unwrap();
    assert_ne!(project, other);

    // The repository moved: the new key is attached to the existing project instead of
    // silently creating a new identity.
    registry.relink("/work/moved", &project).unwrap();
    assert_eq!(
        registry.resolve("/work/moved").unwrap(),
        Some(project.clone())
    );
    // The old key still points at the same project.
    assert_eq!(
        registry.resolve("/work/original").unwrap(),
        Some(project.clone())
    );
    assert_eq!(registry.identity_keys(&project).unwrap().len(), 2);
}

#[test]
fn relinking_to_an_unknown_project_is_refused() {
    let dir = TempDir::new("registry-unknown");
    let registry = ProjectRegistry::open(dir.path());
    let error = registry
        .relink("/work/a", &ProjectId("prj-does-not-exist".into()))
        .unwrap_err();
    assert!(matches!(error, MemoryError::UnknownProject(_)), "{error}");
}

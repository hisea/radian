//! ADR 0021 D2: a split/merge reads the source record, checks the target conflict and writes
//! both appends under **both** locks; a crash between the two appends is completed on a re-run.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::TempDir;
use fs2::FileExt;
use radian_memory::{
    Kind, MemoryError, MemoryStore, MovePhase, ProjectId, Provenance, Scope, Source,
};

fn project(id: &str) -> Scope {
    Scope::Project(ProjectId(id.into()))
}

fn create(
    store: &MemoryStore,
    scope: &Scope,
    key: &str,
    content: &str,
) -> radian_memory::MemoryRecord {
    store
        .create(
            scope,
            Source::User,
            Kind::Fact,
            Some(key.into()),
            content,
            Provenance::default(),
        )
        .unwrap()
}

#[test]
fn a_move_holds_the_target_lock_between_the_check_and_the_write() {
    let dir = TempDir::new("split-lock");
    let store = MemoryStore::open(dir.path());
    let a = ProjectId("pa".into());
    let b = ProjectId("pb".into());
    let record = create(&store, &project("pa"), "k", "body");
    let target_path = store.log_path(&project("pb"));
    let locked = Arc::new(AtomicBool::new(false));
    {
        let target_path = target_path.clone();
        let locked = Arc::clone(&locked);
        radian_memory::set_move_hook(Box::new(move |phase| {
            if phase == MovePhase::BeforeTargetAppend {
                // The move must hold the target writer lock here, so no second writer can be
                // in the check-to-write window. `flock` conflicts across open descriptions,
                // including in this same process (the purge-race repro relies on that too).
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&target_path)
                    .unwrap();
                let acquired = file.try_lock_exclusive().is_ok();
                if acquired {
                    let _ = file.unlock();
                }
                locked.store(!acquired, Ordering::SeqCst);
            }
            false
        }));
    }
    let report = store
        .split(&a, std::slice::from_ref(&record.id), &b)
        .unwrap();
    assert_eq!(report.moved, vec![record.id]);
    assert!(
        locked.load(Ordering::SeqCst),
        "the target lock must be held while the move writes"
    );
}

#[test]
fn a_half_finished_move_is_completed_on_a_rerun() {
    let dir = TempDir::new("split-halfdone");
    let store = MemoryStore::open(dir.path());
    let a = ProjectId("pa".into());
    let b = ProjectId("pb".into());
    let record = create(&store, &project("pa"), "k", "body");

    // Crash between the target `Created` and the source `Forgotten`.
    radian_memory::set_move_hook(Box::new(|phase| phase == MovePhase::AfterTargetAppend));
    let error = store
        .split(&a, std::slice::from_ref(&record.id), &b)
        .unwrap_err();
    assert!(matches!(error, MemoryError::SimulatedCrash), "{error}");
    assert_eq!(store.records(&project("pb")).unwrap().len(), 1);
    assert_eq!(store.records(&project("pa")).unwrap().len(), 1);

    // The combined view keeps exactly one copy: the target (Project b passed first).
    let view = store.view(&[project("pb"), project("pa")]).unwrap();
    assert_eq!(view.len(), 1, "{view:?}");
    assert_eq!(view[0].scope, project("pb"));

    // Re-running the same move completes it instead of making a conflict.
    let report = store
        .split(&a, std::slice::from_ref(&record.id), &b)
        .unwrap();
    assert_eq!(report.moved, vec![record.id.clone()]);
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert!(store.records(&project("pa")).unwrap().is_empty());
    assert_eq!(store.records(&project("pb")).unwrap().len(), 1);

    // The source's `Forgotten` names where it went, so an audit can tell the two apart.
    let raw = std::fs::read_to_string(store.log_path(&project("pa"))).unwrap();
    assert!(raw.contains("moved_to"), "{raw}");
    assert!(raw.contains("pb"), "{raw}");
}

#[test]
fn a_move_never_leaves_two_active_records_with_one_key() {
    let dir = TempDir::new("split-conflict");
    let store = MemoryStore::open(dir.path());
    let a = ProjectId("pa".into());
    let b = ProjectId("pb".into());
    let source_record = create(&store, &project("pa"), "k", "from a");
    create(&store, &project("pb"), "k", "from b");

    let report = store.split(&a, &[source_record.id], &b).unwrap();
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    let target = store.records(&project("pb")).unwrap();
    assert_eq!(target.len(), 1);
    assert_eq!(target[0].content, "from b");
    assert_eq!(store.records(&project("pa")).unwrap().len(), 1);
}

#[test]
fn merge_moves_every_record_and_reports_conflicts() {
    let dir = TempDir::new("merge");
    let store = MemoryStore::open(dir.path());
    let a = ProjectId("pa".into());
    let b = ProjectId("pb".into());
    create(&store, &project("pa"), "one", "a one");
    create(&store, &project("pa"), "two", "a two");
    create(&store, &project("pb"), "two", "b two");

    let report = store.merge(&b, &a).unwrap();
    assert_eq!(report.moved.len(), 1, "{report:?}");
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert_eq!(store.records(&project("pb")).unwrap().len(), 2);
    assert_eq!(store.records(&project("pa")).unwrap().len(), 1);
}

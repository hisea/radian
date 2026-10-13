//! ADR 0021 D3: a writer that opened the log before a Purge replaced it must not write into
//! the replaced (unlinked) inode. `open_locked` revalidates `(dev, ino)` after taking the
//! lock and retries.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::TempDir;
use radian_memory::{Kind, MemoryStore, Provenance, Scope, Source};

fn global() -> Scope {
    Scope::Global
}

/// Deterministic version of the reviewer's repro: the open-hook fires on the writer's thread
/// between `open()` and the lock, and does exactly what Purge's rename does. Without the
/// inode revalidation the writer replays the old content and fsyncs into a file with no path.
#[test]
fn a_writer_that_opened_the_old_inode_retries_after_a_replace() {
    let dir = TempDir::new("purge-race");
    let store = MemoryStore::open(dir.path());
    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("a".into()),
            "A",
            Provenance::default(),
        )
        .unwrap();
    let path = store.log_path(&global());
    let replacement = path.with_extension("tmp");
    std::fs::copy(&path, &replacement).unwrap();

    let fired = Arc::new(AtomicBool::new(false));
    {
        let path = path.clone();
        let fired = Arc::clone(&fired);
        radian_memory::set_open_hook(Box::new(move || {
            std::fs::rename(&replacement, &path).unwrap();
            fired.store(true, Ordering::SeqCst);
        }));
    }

    store
        .create(
            &global(),
            Source::User,
            Kind::Fact,
            Some("b".into()),
            "B-content",
            Provenance::default(),
        )
        .unwrap();
    assert!(
        fired.load(Ordering::SeqCst),
        "the replace hook must have fired"
    );

    let reopened = MemoryStore::open(dir.path());
    let keys: Vec<_> = reopened
        .records(&global())
        .unwrap()
        .into_iter()
        .filter_map(|record| record.key)
        .collect();
    assert!(keys.contains(&"a".to_string()), "{keys:?}");
    assert!(
        keys.contains(&"b".to_string()),
        "the acknowledged write was lost: {keys:?}"
    );
}

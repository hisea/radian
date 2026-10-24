//! The gate's stage locks (ADR 0048 decision 10).
//!
//! `commands` and `mutation` hold `<repository>/.radian/arc/gates/exclusive.lock` for the
//! whole stage; a second exclusive stage waits. `integration` holds its own
//! `<repository>/.radian/arc/gates/integration.lock` for its whole stage, so two integration
//! runs never share the `gates/integration/` checkout. `flock` makes the second stage wait,
//! and a crashed process releases the lock when its descriptor closes.

use std::fs::File;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;

use super::checkout::gates_dir;
use super::error::GateError;

/// The lock file `commands` and `mutation` take.
pub(crate) const EXCLUSIVE_LOCK: &str = "exclusive.lock";

/// The lock a candidate preparation holds while it fetches and creates or updates its stage
/// checkout. The `commands` and `review` stages of one verification run at the same time
/// (ADR 0048 decision 10), and `git fetch`, `git worktree add` and `git worktree prune` update
/// the shared repository, so their preparations take turns and the stage work then runs at the
/// same time.
pub(crate) const PREPARE_LOCK: &str = "prepare.lock";

/// A held `flock` on one file in `gates/`. Dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct DirLock {
    file: File,
}

impl DirLock {
    /// Takes `name` under `gates/` of `repository`, blocking until it is free.
    ///
    /// # Errors
    ///
    /// The `gates/` directory or the lock file could not be created or opened.
    pub(crate) fn acquire(repository: &Path, name: &str) -> Result<Self, GateError> {
        let (path, file) = open_lock_file(repository, name)?;
        file.lock_exclusive().map_err(|source| GateError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self { file })
    }

    /// Takes `name` under `gates/` of `repository` if it is free, without waiting.
    ///
    /// # Errors
    ///
    /// The `gates/` directory or the lock file could not be created or opened, or the lock
    /// attempt failed for a reason other than another process holding it.
    pub(crate) fn try_acquire(repository: &Path, name: &str) -> Result<Option<Self>, GateError> {
        let (path, file) = open_lock_file(repository, name)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(source) => Err(GateError::Io { path, source }),
        }
    }
}

fn open_lock_file(repository: &Path, name: &str) -> Result<(PathBuf, File), GateError> {
    let dir = gates_dir(repository);
    std::fs::create_dir_all(&dir).map_err(|source| GateError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join(name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|source| GateError::Io {
            path: path.clone(),
            source,
        })?;
    Ok((path, file))
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the flock; the explicit unlock keeps the intent.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// The held exclusive lock of `commands` and `mutation`. Dropping it releases the lock.
#[derive(Debug)]
pub struct ExclusiveLock {
    // Held for its `Drop`: closing the descriptor releases the `flock`.
    _lock: DirLock,
}

impl ExclusiveLock {
    /// Takes the exclusive stage lock of `repository`, blocking until it is free.
    ///
    /// # Errors
    ///
    /// The `gates/` directory or the lock file could not be created or opened.
    pub fn acquire(repository: &Path) -> Result<Self, GateError> {
        Ok(Self {
            _lock: DirLock::acquire(repository, EXCLUSIVE_LOCK)?,
        })
    }
}

/// How often a review waits before it looks for a free slot again.
const SLOT_RETRY: Duration = Duration::from_millis(50);

/// One of `slots` locks named `<name>-0.lock` … `<name>-<slots-1>.lock` (ADR 0048
/// decision 10). A caller takes the first free one and holds it for its whole stage; the
/// slot number also names that stage's own checkout, so two holders never share one.
#[derive(Debug)]
pub(crate) struct SlotLock {
    // Held for its `Drop`: closing the descriptor releases the `flock`.
    _lock: DirLock,
    slot: u32,
}

impl SlotLock {
    /// Takes the first free slot, blocking until one is free.
    ///
    /// # Errors
    ///
    /// A slot's lock file could not be created or opened.
    pub(crate) fn acquire(repository: &Path, name: &str, slots: u32) -> Result<Self, GateError> {
        let slots = slots.max(1);
        loop {
            for slot in 0..slots {
                let file = format!("{name}-{slot}.lock");
                if let Some(lock) = DirLock::try_acquire(repository, &file)? {
                    return Ok(Self { _lock: lock, slot });
                }
            }
            std::thread::sleep(SLOT_RETRY);
        }
    }

    /// The slot this lock holds.
    pub(crate) fn slot(&self) -> u32 {
        self.slot
    }
}

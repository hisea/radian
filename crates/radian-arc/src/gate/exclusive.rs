//! The gate's stage locks (ADR 0048 decision 10).
//!
//! `commands` and `mutation` hold `<repository>/.radian/arc/gates/exclusive.lock` for the
//! whole stage; a second exclusive stage waits. `integration` holds its own
//! `<repository>/.radian/arc/gates/integration.lock` for its whole stage, so two integration
//! runs never share the `gates/integration/` checkout. `flock` makes the second stage wait,
//! and a crashed process releases the lock when its descriptor closes.

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;

use super::checkout::gates_dir;
use super::error::GateError;

/// The lock file `commands` and `mutation` take.
pub(crate) const EXCLUSIVE_LOCK: &str = "exclusive.lock";

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
        file.lock_exclusive().map_err(|source| GateError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self { file })
    }
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

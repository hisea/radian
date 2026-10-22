//! The exclusive stage lock (ADR 0048 decision 10).
//!
//! `commands` and `mutation` hold `<repository>/.radian/arc/gates/exclusive.lock` for the
//! whole stage. `flock` makes a second exclusive stage wait, and a crashed process releases
//! the lock when its descriptor closes.

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;

use super::checkout::gates_dir;
use super::error::GateError;

/// The held exclusive lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct ExclusiveLock {
    file: File,
}

impl ExclusiveLock {
    /// Takes the exclusive stage lock of `repository`, blocking until it is free.
    ///
    /// # Errors
    ///
    /// The `gates/` directory or the lock file could not be created or opened.
    pub fn acquire(repository: &Path) -> Result<Self, GateError> {
        let dir = gates_dir(repository);
        std::fs::create_dir_all(&dir).map_err(|source| GateError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join("exclusive.lock");
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

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the flock; the explicit unlock keeps the intent.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

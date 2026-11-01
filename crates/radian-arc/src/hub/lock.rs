//! `$RADIAN_HOME/hub/hub.lock`: the flock one hub process holds for its life.
//!
//! The file has the shape and the helper of a repository's `runner.lock` (ADR 0048
//! decisions 13 and 16): the flock is the lock, the holder writes its pid inside, and a view
//! that cannot take the lock reads the pid from the file. A probe takes the lock and releases
//! it at once and never creates the file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::store::Error;

use super::registry::HUB_LOCK;

/// The `hub.lock` guard: the flock is released when this drops.
#[derive(Debug)]
pub struct HubLock {
    file: File,
}

impl HubLock {
    /// The `hub.lock` path in `dir`.
    #[must_use]
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(HUB_LOCK)
    }

    /// Tries to take `hub.lock` without waiting. `Ok(None)` means another hub holds it: the
    /// caller names [`HubLock::holder`] and exits. The hub directory is created `0700` first.
    ///
    /// # Errors
    ///
    /// The directory or the lock file cannot be created or opened.
    pub fn try_take(dir: &Path) -> Result<Option<Self>, Error> {
        radian_host::ensure_private_dir(dir)
            .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
        let path = Self::path(dir);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| Error::Failed(format!("cannot open {}: {error}", path.display())))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                record_pid(&mut file);
                Ok(Some(Self { file }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(Error::Failed(format!(
                "cannot lock {}: {error}",
                path.display()
            ))),
        }
    }

    /// The pid the holder of `hub.lock` recorded, when it did. Only meaningful while the lock
    /// is held. `None` for a lock file an older radian wrote, or one whose pid could not be
    /// written.
    #[must_use]
    pub fn holder(dir: &Path) -> Option<u32> {
        read_pid(&Self::path(dir))
    }

    /// Whether a hub holds `hub.lock`, and its pid when the holder wrote one. The lock is
    /// taken and released at once; the probe never creates the file. `None` is a free or
    /// absent lock.
    #[must_use]
    pub fn probe(dir: &Path) -> Option<u32> {
        let path = Self::path(dir);
        if !path.is_file() {
            return None;
        }
        let file = OpenOptions::new().read(true).write(true).open(&path).ok()?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                None
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => read_pid(&path),
            Err(_) => None,
        }
    }
}

impl Drop for HubLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Writes this process's pid into `hub.lock`, for a probe that cannot take the lock. A failure
/// is ignored: the lock is the flock, and the pid only feeds a status line.
fn record_pid(file: &mut File) {
    if file.set_len(0).is_err() || file.seek(SeekFrom::Start(0)).is_err() {
        return;
    }
    let _ = file.write_all(std::process::id().to_string().as_bytes());
    let _ = file.flush();
}

/// The pid a lock file carries, trimmed and parsed.
fn read_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

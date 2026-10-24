//! Scratch directories for tests. This module is compiled only under `#[cfg(test)]` in the
//! library; the integration tests include it by path.
//!
//! A per-test directory sits directly under the system temp dir and is removed when its guard
//! drops. A fixture that a `OnceLock` shares for the whole test binary uses the process-exit
//! registry instead: it is `'static`, so a `Drop` guard cannot own it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn named(prefix: &str) -> Self {
        let path = unique(prefix);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// A guard for a path the test creates itself.
    pub fn uncreated(prefix: &str) -> Self {
        Self(unique(prefix))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unique(prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// A unique directory removed when the test process exits.
pub fn process_dir(prefix: &str) -> PathBuf {
    let path = unique(prefix);
    std::fs::create_dir_all(&path).unwrap();
    EXIT_DIRS.lock().unwrap().push(path.clone());
    EXIT_HOOK.call_once(|| unsafe {
        // SAFETY: the callback only touches this module's registry.
        atexit(remove_exit_dirs);
    });
    path
}

static EXIT_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static EXIT_HOOK: Once = Once::new();

extern "C" fn remove_exit_dirs() {
    if let Ok(dirs) = EXIT_DIRS.lock() {
        for dir in dirs.iter().rev() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}

#[cfg(not(unix))]
unsafe fn atexit(_callback: extern "C" fn()) -> i32 {
    0
}

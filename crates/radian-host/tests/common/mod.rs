//! Test-only scratch directories, shared by the integration tests of this crate.
//!
//! Every directory sits directly under the system temp dir (never in a shared per-process
//! root) and keeps its caller's prefix, so a leftover can still be traced to its test family.
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

    /// A guard for an explicit path (a short `/tmp` base keeps a Unix socket under the length
    /// cap), created here.
    pub fn at(path: PathBuf) -> Self {
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(&path).unwrap_or(path))
    }

    /// A created directory, stored through its canonical path (the temp root is a symlink on
    /// macOS).
    pub fn canonical(prefix: &str) -> Self {
        let path = unique(prefix);
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(&path).unwrap())
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

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
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

/// A unique directory removed when the test process exits, for a fixture a `OnceLock` shares
/// for the whole test binary. Such a fixture is `'static`, so a `Drop` guard cannot own it;
/// under nextest every test is its own process, so process exit is the end of the test.
pub fn process_dir(prefix: &str) -> PathBuf {
    process_dir_at(unique(prefix))
}

/// [`process_dir`] at an explicit path (a short `/tmp` base keeps a control socket under the
/// length cap).
pub fn process_dir_at(path: PathBuf) -> PathBuf {
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
    // Tests run on macOS and Linux; elsewhere the directory is left to the OS.
    0
}

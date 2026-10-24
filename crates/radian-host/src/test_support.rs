//! Scratch directories for unit tests. This module is compiled only under `#[cfg(test)]`.
//!
//! A directory sits directly under the system temp dir and is removed when its guard drops,
//! so a passing test and a panicking one both clean up.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under the system temp dir, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        Self::named_with("radian-host-unit", label)
    }

    /// A scratch directory whose own prefix identifies the test family.
    pub fn named(prefix: &str) -> Self {
        Self::named_with(prefix, "scratch")
    }

    pub fn with_label(prefix: &str, label: &str) -> Self {
        Self::named_with(prefix, label)
    }

    fn named_with(prefix: &str, label: &str) -> Self {
        let path = Self::unique(prefix, label);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// A guard for a path the test creates itself (it may need it absent first).
    pub fn uncreated(prefix: &str, label: &str) -> Self {
        Self {
            path: Self::unique(prefix, label),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    fn unique(prefix: &str, label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("{prefix}-{}-{label}-{n}", std::process::id()))
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A temporary file (a Unix socket leaves its inode behind) removed on drop.
pub struct TempPath(PathBuf);

impl TempPath {
    pub fn new(path: PathBuf) -> Self {
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempPath {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

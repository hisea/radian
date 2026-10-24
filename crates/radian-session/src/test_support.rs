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
        Self::named_with("radian-session-unit", label)
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

    /// A guard for a path the test creates itself (a permission test may need it absent).
    pub fn uncreated(prefix: &str, label: &str) -> Self {
        Self {
            path: Self::unique(prefix, label),
        }
    }

    fn unique(prefix: &str, label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("{prefix}-{}-{label}-{n}", std::process::id()))
    }

    pub fn path(&self) -> &Path {
        &self.path
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

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

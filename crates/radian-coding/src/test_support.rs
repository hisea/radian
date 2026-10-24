//! Scratch directories for unit tests. This module is compiled only under `#[cfg(test)]`.
//!
//! Every directory sits directly under the system temp dir and is removed when its guard
//! drops, so a passing test and a panicking test both clean up (ADR 0043's one-session rule
//! keeps the runtime hermetic; the prefix keeps a leftover traceable to this crate).

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
        Self::named_with("radian-coding-unit", label)
    }

    /// A scratch directory whose own prefix identifies the test family.
    pub fn named(prefix: &str) -> Self {
        Self::with_label(prefix, "scratch")
    }

    pub fn with_label(prefix: &str, label: &str) -> Self {
        Self::named_with(prefix, label)
    }

    fn named_with(prefix: &str, label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{label}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
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

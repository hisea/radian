//! The memory scope directories the file tools may read **and write** outside the workspace
//! (ADR 0039 decision 4): `<memory root>/global/` and, when the workspace maps to a project,
//! `<memory root>/projects/<ProjectId>/`.
//!
//! The host passes the directories that apply to the session (the host's `MemoryService`
//! knows both; a read never registers a project). A model names one as an absolute path or
//! with `~`, and
//! the path is opened from that directory's own fd with the same per-component
//! `O_NOFOLLOW` walk the workspace uses ([`RootDir`]): no symlink is followed at any level,
//! and a link that leaves the root is refused exactly like a link that leaves the workspace.
//!
//! Unlike the read-only roots ([`crate::read_roots`]), these are writable: `write` and
//! `patch` accept them too. A scope holds one file per record and its `MEMORY.md` index and
//! nothing else, so a write never creates a subdirectory, and a file `write` creates is
//! `0600` (ADR 0021 D3: the memory directory's mode, not the workspace's `0644`).
//!
//! Everything else under the memory directory (`projects.jsonl`, `log.jsonl.migrated`, a
//! project this workspace does not map to) is **not** a root. In the `full` tier the deny set
//! refuses it; in the `workspace` tier it is an ordinary outside-workspace path. The deny set
//! itself keeps `memory/` for the sandbox (the shell), which has no such exception.

use std::path::{Path, PathBuf};

use crate::read_roots::normalise_absolute;
use crate::safe_fs::{OpenMode, RootDir, SafeFsError};

/// The host-maintained session index file name. The model never writes it, even though its
/// directory is a writable memory scope (ADR 0021 D3).
pub const SESSIONS_INDEX_FILE: &str = "SESSIONS.md";

/// One writable memory scope, held as a directory fd plus the absolute spellings a model path
/// may use to name it.
#[derive(Debug)]
pub(crate) struct MemoryRoot {
    dir: RootDir,
    /// The canonical path (what the fd points at); used in messages.
    canonical: PathBuf,
    /// The absolute spellings that select this root (the canonical path and, when it differs,
    /// the configured one).
    prefixes: Vec<PathBuf>,
}

/// A path that resolved under one of the [`MemoryRoots`].
pub(crate) struct FoundMemoryPath {
    /// Index into [`MemoryRoots::roots`].
    pub(crate) index: usize,
    /// The path relative to the root.
    pub(crate) relative: PathBuf,
    /// The spelling of the root the model used; joining it with `relative` gives a path the
    /// file tools accept again.
    pub(crate) prefix: PathBuf,
}

/// The writable memory scopes, resolved once when the tool set is built.
#[derive(Debug, Default)]
pub struct MemoryRoots {
    roots: Vec<MemoryRoot>,
}

impl MemoryRoots {
    /// No memory scopes: the file tools reach the workspace and the read-only roots only
    /// (an embedded tool set, a test that does not exercise memory, and a host with no
    /// memory directory).
    pub fn none() -> Self {
        Self { roots: Vec::new() }
    }

    /// One root per directory the host passed. A directory that is missing, is not a
    /// directory, or cannot be opened is skipped, so the file tools simply do not reach it
    /// (the host creates the scope directories 0700 before it passes them, ADR 0021 D3).
    /// Order is kept: the lookup takes the longest matching prefix.
    pub fn from_dirs(dirs: &[PathBuf]) -> Self {
        let roots = dirs.iter().filter_map(|dir| MemoryRoot::at(dir)).collect();
        Self { roots }
    }

    /// The roots, for opening a path and for the error that names the root.
    pub(crate) fn roots(&self) -> &[MemoryRoot] {
        &self.roots
    }

    /// The canonical paths of the roots that opened, for the `full`-tier deny set's one
    /// exception (ADR 0039 decision 4). A directory [`MemoryRoots::from_dirs`] skipped
    /// contributes nothing, so a scope that was never created, or whose creation failed,
    /// cannot widen the exception to the memory directory. Borrowed, so a caller adding them
    /// to a set does not build a throw-away `Vec` only to copy it again.
    pub(crate) fn canonical_paths(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|root| root.canonical.as_path())
    }

    /// The root `path` (already absolute and lexically normalised) falls under, the longest
    /// matching prefix winning. `None` means it is under no memory scope.
    pub(crate) fn lookup(&self, path: &Path) -> Option<FoundMemoryPath> {
        let mut best: Option<(usize, FoundMemoryPath)> = None;
        for (index, root) in self.roots.iter().enumerate() {
            for prefix in &root.prefixes {
                if let Ok(relative) = path.strip_prefix(prefix) {
                    let width = prefix.components().count();
                    if best
                        .as_ref()
                        .is_none_or(|(best_width, _)| width > *best_width)
                    {
                        best = Some((
                            width,
                            FoundMemoryPath {
                                index,
                                relative: relative.to_path_buf(),
                                prefix: prefix.clone(),
                            },
                        ));
                    }
                }
            }
        }
        best.map(|(_, found)| found)
    }
}

impl MemoryRoot {
    /// Opens `configured` as a memory root, or `None` when it is missing or not a directory.
    /// The configured spelling is canonicalized once here; the root's parent is host-owned,
    /// so following a link at the root itself is intended (like every other root).
    fn at(configured: &Path) -> Option<Self> {
        let canonical = std::fs::canonicalize(configured).ok()?;
        if !canonical.is_dir() {
            return None;
        }
        let dir = RootDir::open(&canonical).ok()?;
        let mut prefixes = vec![canonical.clone()];
        if let Some(configured) = normalise_absolute(configured)
            && configured != canonical
        {
            prefixes.push(configured);
        }
        Some(Self {
            dir,
            canonical,
            prefixes,
        })
    }

    /// Opens `relative` (already relative to this root) as a file with `mode` and
    /// `create_mode` (only `write`'s create modes use it). Missing parent levels are refused:
    /// a scope is flat.
    pub(crate) fn file(
        &self,
        relative: &Path,
        mode: OpenMode,
        create_mode: u32,
    ) -> Result<std::fs::File, SafeFsError> {
        self.dir.file_with_mode(relative, mode, create_mode)
    }

    /// Opens `relative` (already relative to this root) as a directory.
    pub(crate) fn dir(&self, relative: &Path) -> Result<std::fs::File, SafeFsError> {
        self.dir.dir(relative)
    }

    /// The canonical path of the root (never the outside target of a link).
    pub(crate) fn canonical(&self) -> &Path {
        &self.canonical
    }

    /// The root's directory fd, for the search tools' above-base ignore matchers.
    pub(crate) fn root_dir(&self) -> &RootDir {
        &self.dir
    }
}

/// The components of a memory-scope path, for the flat-scope check: a scope holds files
/// directly, never a subdirectory. `relative` is already relative to the root.
pub(crate) fn is_flat(relative: &Path) -> bool {
    let mut components = relative.components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

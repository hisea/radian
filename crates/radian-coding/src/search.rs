//! What the two search tools (`glob`, `grep`) share: the gitignore-aware walk and the glob
//! compiler.
//!
//! Both walk from a base directory the caller opened through the workspace root's fd
//! ([`WorkspaceRoot::open_dir`](crate::workspace::WorkspaceRoot::open_dir)), so the base is
//! inside the workspace and no component of it is a symlink. The walk itself is fd-relative
//! too (ADR 0039 decision 2): every subdirectory is opened with `openat(dirfd, name,
//! O_NOFOLLOW | O_DIRECTORY)`, every entry is classified with `fstatat(AT_SYMLINK_NOFOLLOW)`,
//! and a matched file is handed back as an **open fd**, never as a path to re-open. A parent
//! swapped for a link while the walk runs is refused at the `openat`, not followed.
//!
//! The per-entry rules are the ones the `ignore` crate's walker applied before this module
//! became fd-relative (its `WalkBuilder` opens directories by path, which is the window this
//! closes); the ignore **matching** still uses `ignore::gitignore`:
//!
//! - `.gitignore` and `.ignore` files **inside** the walked tree are respected, with or
//!   without a git repository, so a plain directory is searched the same way a checkout is;
//!   a directory that holds a `.git` directory also contributes its `.git/info/exclude`;
//! - no ignore file **above** the base is read and the user's global gitignore is not
//!   consulted, so the same tree gives the same result on every machine;
//! - the `.git` directory is pruned, not merely hidden;
//! - `.radian-worktrees/` is pruned too: it can hold one git worktree per checkout, each a
//!   full tree, so descending into it would return every file once per worktree (and search
//!   other branches' contents). The rule is structural, not a `.gitignore` coincidence;
//! - symlinks are never followed: a symlinked file is not a match and a symlinked directory is
//!   not descended, so a link pointing out of the workspace is never read;
//! - dot-files other than `.git` are included: the model asked for a tree, not for hidden-file
//!   filtering, and a directory `read` shows them too.
//!
//! Precedence inside one directory follows git: `.ignore` beats `.gitignore` beats
//! `.git/info/exclude`; a deeper directory's rules beat its parents'; the last matching
//! pattern in a file wins.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};

use agent_core::ToolFailure;
use globset::{GlobBuilder, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::fail;
use crate::safe_fs::{self, EntryKind, OpenMode};

pub(crate) const SKIPPED_DIR_NAMES: &[&str] = &[".git", RADIAN_WORKTREES];

/// The workspace-local root of one-off git worktrees. The walk prunes it; a directory
/// `read` omits the entry, so a listing is not cluttered by N full checkouts of the same tree.
pub(crate) const RADIAN_WORKTREES: &str = ".radian-worktrees";

/// One regular file the walk found: its workspace-relative path and the open, read-only fd
/// (`O_NOFOLLOW`; the entry was a regular file at `fstatat` time and the open refuses a link
/// that replaced it since).
pub(crate) struct Found {
    pub relative: PathBuf,
    pub file: File,
}

/// Walks `base_dir` (an open directory fd whose workspace-relative path is `base_rel`) and
/// calls `visit` for every regular file that the ignore rules keep, in sorted order per
/// directory. `visit` returns `false` to stop early.
pub(crate) fn walk(base_dir: &File, base_rel: &Path, visit: &mut dyn FnMut(Found) -> bool) -> bool {
    let mut matchers: Vec<Gitignore> = Vec::new();
    walk_dir(base_dir, base_rel, &mut matchers, visit)
}

fn walk_dir(
    dir: &File,
    rel: &Path,
    matchers: &mut Vec<Gitignore>,
    visit: &mut dyn FnMut(Found) -> bool,
) -> bool {
    let Ok(entries) = safe_fs::read_dir(dir) else {
        return true;
    };
    let pushed = match ignore_rules(dir, rel, &entries) {
        Some(matcher) => {
            matchers.push(matcher);
            true
        }
        None => false,
    };
    let mut keep_going = true;
    for entry in entries {
        if entry.kind == EntryKind::Symlink || entry.kind == EntryKind::Other {
            continue;
        }
        let is_dir = entry.kind == EntryKind::Dir;
        if is_dir && is_pruned(&entry.name) {
            continue;
        }
        let entry_rel = rel.join(&entry.name);
        if ignored(matchers, &entry_rel, is_dir) {
            continue;
        }
        if is_dir {
            // Descend through the fd: a symlink here is refused by `O_NOFOLLOW` (it was not
            // one at `fstatat` time, so this only matters for a swap in between).
            let Ok(child) =
                safe_fs::walk_dirs(dir, Path::new(""), std::slice::from_ref(&entry.name))
            else {
                continue;
            };
            if !walk_dir(&child, &entry_rel, matchers, visit) {
                keep_going = false;
                break;
            }
        } else {
            let Ok(file) = open_child_file(dir, &entry.name) else {
                continue;
            };
            if !visit(Found {
                relative: entry_rel,
                file,
            }) {
                keep_going = false;
                break;
            }
        }
    }
    if pushed {
        matchers.pop();
    }
    keep_going
}

fn is_pruned(name: &OsStr) -> bool {
    SKIPPED_DIR_NAMES
        .iter()
        .any(|skip| name == OsStr::new(skip))
}

/// Whether any matcher, deepest first, ignores `path` (a whitelist match stops the search
/// and keeps the entry, exactly as git does).
fn ignored(matchers: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for matcher in matchers.iter().rev() {
        let decision = matcher.matched(path, is_dir);
        if decision.is_ignore() {
            return true;
        }
        if decision.is_whitelist() {
            return false;
        }
    }
    false
}

/// The ignore rules one directory contributes: `.git/info/exclude` (when `.git` is a directory
/// here), then `.gitignore`, then `.ignore`, added in that order so the later file's patterns
/// win. Each file is opened through the directory's fd with `O_NOFOLLOW`. `None` when the
/// directory has no rules.
fn ignore_rules(dir: &File, rel: &Path, entries: &[safe_fs::DirEntry]) -> Option<Gitignore> {
    let has = |name: &str, kind: EntryKind| {
        entries
            .iter()
            .any(|entry| entry.name == OsStr::new(name) && entry.kind == kind)
    };
    let mut sources: Vec<String> = Vec::new();
    if has(".git", EntryKind::Dir)
        && let Ok(git) = safe_fs::walk_dirs(dir, Path::new(""), &[OsString::from(".git")])
        && let Ok(info) = safe_fs::walk_dirs(&git, Path::new(""), &[OsString::from("info")])
        && let Ok(file) = open_child_file(&info, OsStr::new("exclude"))
        && let Ok(text) = safe_fs::read_to_string(&file)
    {
        sources.push(text);
    }
    for name in [".gitignore", ".ignore"] {
        if has(name, EntryKind::File)
            && let Ok(file) = open_child_file(dir, OsStr::new(name))
            && let Ok(text) = safe_fs::read_to_string(&file)
        {
            sources.push(text);
        }
    }
    if sources.is_empty() {
        return None;
    }
    // The matcher's root is the directory's workspace-relative path, so patterns anchor to
    // this directory and the entry paths handed to `matched` are relative to the workspace.
    let mut builder = GitignoreBuilder::new(rel);
    for text in &sources {
        for line in text.lines() {
            // An invalid pattern is skipped, as the `ignore` walker does.
            let _ = builder.add_line(None, line);
        }
    }
    builder.build().ok()
}

fn open_child_file(dir: &File, name: &OsStr) -> Result<File, ()> {
    safe_fs::open_file(dir, Path::new(""), &[name.to_os_string()], OpenMode::Read).map_err(|_| ())
}

pub(crate) fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolFailure> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| fail("invalid_glob", format!("`{pattern}`: {error}")))
}

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
//! - `.gitignore` and `.ignore` files are respected with or without a git repository, so a
//!   plain directory is searched the same way a checkout is; the files **above** the base —
//!   every directory from the workspace root down to the base's parent, outermost first —
//!   apply too (a nearer directory's rules win, as in git), so a repository-root ignore
//!   keeps working when the search starts in a subdirectory; a directory that holds a `.git`
//!   directory also contributes its `.git/info/exclude`;
//! - the user's global gitignore is not consulted, so the same tree gives the same result
//!   on every machine;
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

use globset::{GlobBuilder, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use radian_core::ToolFailure;

use crate::fail;
use crate::safe_fs::{self, EntryKind, OpenMode};
use crate::workspace::{OpenedFile, ReadBase, WorkspaceRoot};

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

/// The ignore matchers for every directory from the filesystem root down to (but not
/// including) the search base, outermost first. `root_fd` is the open fd of the root
/// `base_rel` is relative to (the workspace root, or a read-only root); each level is opened
/// fd-relative from it exactly as the walk does (ADR 0039 decision 2: no path re-open,
/// `O_NOFOLLOW | O_DIRECTORY`), so the matchers the walk starts with are the ones git itself
/// would apply above the base. A level that cannot be opened or listed is skipped, like an
/// unreadable directory inside the walk.
pub(crate) fn ancestor_matchers(root_fd: &File, base_rel: &Path) -> Vec<Gitignore> {
    let parts = safe_fs::components(base_rel).unwrap_or_default();
    let mut matchers = Vec::with_capacity(parts.len());
    // Every strict parent of the base, outermost first: the root, then each prefix.
    for index in 0..parts.len() {
        let prefix: PathBuf = parts[..index].iter().collect();
        let dir: Option<File> = if index == 0 {
            None
        } else {
            safe_fs::walk_dirs(root_fd, Path::new(""), &parts[..index]).ok()
        };
        let dir_ref = dir.as_ref().unwrap_or(root_fd);
        let Ok(entries) = safe_fs::read_dir(dir_ref) else {
            continue;
        };
        if let Some(matcher) = ignore_rules(dir_ref, &prefix, &entries) {
            matchers.push(matcher);
        }
    }
    matchers
}

/// Walks `base_dir` (an open directory fd whose workspace-relative path is `base_rel`) and
/// calls `visit` for every regular file that the ignore rules keep, in sorted order per
/// directory. `visit` returns `false` to stop early. `ancestors` are the matchers for the
/// directories above the base ([`ancestor_matchers`]), outermost first, so a repository-root
/// ignore keeps working when the search starts in a subdirectory.
pub(crate) fn walk(
    base_dir: &File,
    base_rel: &Path,
    ancestors: Vec<Gitignore>,
    visit: &mut dyn FnMut(Found) -> bool,
) -> bool {
    let mut matchers: Vec<Gitignore> = ancestors;
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

/// One `path` segment resolved the way a single-base call resolves it today: either a
/// directory to walk ([`WorkspaceRoot::read_base`]) or a lone file
/// ([`WorkspaceRoot::open_read`]). Each directory base keeps its own ancestor ignore
/// matchers and display prefix, so mixed workspace and read-only-root bases each render
/// the way a lone call would.
pub(crate) enum SingleBase {
    Dir(ReadBase),
    File {
        opened: OpenedFile,
        relative: PathBuf,
        prefix: Option<PathBuf>,
        /// The segment spelling, for errors that name the base.
        segment: String,
    },
}

/// What one `path` argument resolved to: one base as today, or several after the
/// whitespace split. `header` is the `[searched N paths: …]` first line, present only
/// for several bases.
pub(crate) struct SearchBases {
    pub bases: Vec<SingleBase>,
    pub header: Option<String>,
}

/// Tries one segment exactly as a single-base call does: a directory first, then a lone
/// file when the base is not a directory. Any other failure is the segment's own error.
pub(crate) fn try_single_base(
    root: &WorkspaceRoot,
    segment: &str,
) -> Result<SingleBase, ToolFailure> {
    match root.read_base(segment) {
        Ok(base) => Ok(SingleBase::Dir(base)),
        Err(failure) if failure.code == "not_a_directory" => {
            let opened = root.open_read(segment)?;
            let (relative, prefix) = root.read_relative(segment)?;
            Ok(SingleBase::File {
                opened,
                relative,
                prefix,
                segment: segment.to_owned(),
            })
        }
        Err(failure) => Err(failure),
    }
}

/// Resolves `requested` to one base or, when the whole string names nothing and holds
/// whitespace, to one base per whitespace-separated segment. Every segment is resolved
/// exactly as a single-base call (workspace-relative, or an absolute/`~` path under a
/// read-only root; a directory or a lone file). When any segment fails, the error joins
/// each failing segment's own message and says `path` takes one path per call; a `path`
/// without whitespace fails exactly as before. A `path` that names one thing (including
/// a directory whose name holds a space) is never split.
pub(crate) fn resolve_bases(
    root: &WorkspaceRoot,
    requested: &str,
) -> Result<SearchBases, ToolFailure> {
    match try_single_base(root, requested) {
        Ok(base) => Ok(SearchBases {
            bases: vec![base],
            header: None,
        }),
        Err(single) => {
            if !requested.chars().any(char::is_whitespace) {
                return Err(single);
            }
            let segments: Vec<&str> = requested.split_whitespace().collect();
            if segments.is_empty() {
                return Err(single);
            }
            let mut bases = Vec::with_capacity(segments.len());
            let mut failures: Vec<ToolFailure> = Vec::new();
            for segment in &segments {
                match try_single_base(root, segment) {
                    Ok(base) => bases.push(base),
                    Err(failure) => failures.push(failure),
                }
            }
            if !failures.is_empty() {
                let mut message = failures
                    .iter()
                    .map(|failure| failure.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                message.push_str(
                    "; `path` takes one directory or file; to search several, call the \
                     tool once per path in the same reply",
                );
                let code = failures
                    .into_iter()
                    .next()
                    .map(|failure| failure.code)
                    .unwrap_or_else(|| "not_found".to_owned());
                return Err(ToolFailure::new(code, message));
            }
            if bases.len() == 1 {
                // Only surrounding whitespace: the one base, without a header.
                return Ok(SearchBases {
                    bases,
                    header: None,
                });
            }
            Ok(SearchBases {
                header: Some(format!(
                    "[searched {} paths: {}]\n",
                    bases.len(),
                    segments.join(", ")
                )),
                bases,
            })
        }
    }
}

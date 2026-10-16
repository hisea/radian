//! What the two search tools (`glob`, `grep`) share: the gitignore-aware walk and the glob
//! compiler.
//!
//! Both walk from a base directory the caller already resolved with
//! [`WorkspaceRoot::resolve_existing`](crate::workspace::WorkspaceRoot::resolve_existing), so
//! the base is canonical and inside the workspace. The walk adds the per-**entry** rules:
//!
//! - `.gitignore` (and `.ignore`) files **inside** the walked tree are respected, with or
//!   without a git repository (`require_git(false)`), so a plain directory is searched the
//!   same way a checkout is;
//! - no ignore file **above** the base is read (`parents(false)`) and the user's global
//!   gitignore is not consulted (`git_global(false)`), so the same tree gives the same result
//!   on every machine;
//! - the `.git` directory is pruned, not merely hidden;
//! - `.radian-worktrees/` is pruned too: it holds one git worktree per checkout, each a full
//!   tree, so descending into it would return every file once per worktree (and search other
//!   branches' contents). Task agents' worktrees no longer live there (ADR 0039 decision 4:
//!   they moved to `~/.radian/worktrees/<repo-key>/`), but the one-off and integration
//!   worktrees of ADR 0030 O7 still do. The rule is structural, not a `.gitignore` coincidence;
//! - symlinks are never followed: a symlinked file is not a match and a symlinked directory is
//!   not descended, so a link pointing out of the workspace is never read;
//! - dot-files other than `.git` are included: the model asked for a tree, not for hidden-file
//!   filtering, and `list` shows them too.

use std::ffi::OsStr;
use std::path::Path;

use agent_core::ToolFailure;
use globset::{GlobBuilder, GlobMatcher};
use ignore::{DirEntry, Walk, WalkBuilder};

use crate::fail;

/// The one walk both tools use. See the module docs for every rule it sets.
pub(crate) fn walk(base: &Path) -> Walk {
    WalkBuilder::new(base)
        .hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(is_not_pruned)
        .build()
}

/// Directory names the walk prunes **and their whole subtree**: `.git` (never searchable) and
/// `.radian-worktrees/` (one full git worktree per checkout lives there, so walking it would
/// return the tree once per worktree). `filter_entry` drops a directory *and its subtree*,
/// which is why this is not an after-the-fact name test.
pub(crate) const SKIPPED_DIR_NAMES: &[&str] = &[".git", RADIAN_WORKTREES];

/// The workspace-local root of one-off git worktrees (ADR 0030 O7: the lead's integration
/// worktree). The walk prunes it entirely; `list` omits the entry, so a directory listing is
/// not cluttered by N full checkouts of the same tree. A **task** agent's worktree is not
/// here any more (ADR 0039 decision 4: `~/.radian/worktrees/<repo-key>/<task-id>`).
pub(crate) const RADIAN_WORKTREES: &str = ".radian-worktrees";

fn is_not_pruned(entry: &DirEntry) -> bool {
    let name = entry.file_name();
    !SKIPPED_DIR_NAMES
        .iter()
        .any(|skip| name == OsStr::new(skip))
}

/// Is this entry a regular file (not a directory and **not a symlink**)? Symlinks are skipped
/// so the search never reads through one.
pub(crate) fn is_regular_file(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|kind| kind.is_file())
}

/// The entry's path relative to the workspace root, or `None` if it is not under it (which the
/// base resolution already rules out).
pub(crate) fn relative<'a>(root: &'a Path, path: &'a Path) -> Option<&'a Path> {
    path.strip_prefix(root).ok()
}

/// Compiles a model-supplied glob into a matcher. `*` does not cross a `/` and `**` does, so
/// `src/**/*.rs` means what it looks like. A bad pattern is a named tool error, never a panic.
pub(crate) fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolFailure> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| fail("invalid_glob", format!("`{pattern}`: {error}")))
}

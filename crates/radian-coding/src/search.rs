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
        .filter_entry(is_not_dot_git)
        .build()
}

/// Prunes `.git` and everything under it. `filter_entry` drops a directory *and its subtree*,
/// which is why this is not an after-the-fact name test.
fn is_not_dot_git(entry: &DirEntry) -> bool {
    entry.file_name() != OsStr::new(".git")
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

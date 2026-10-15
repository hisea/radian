use std::path::{Component, Path, PathBuf};

use agent_core::ToolFailure;

use crate::{CODE_OUTSIDE, fail};

/// Why a workspace root could not be used.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{path}: cannot use as a workspace root: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: the workspace root is not a directory")]
    NotADirectory(PathBuf),
}

/// The canonical workspace root. Constructed once per tool provider, so a symlinked root
/// is resolved before any path check compares against it.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    root: PathBuf,
}

impl WorkspaceRoot {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let path = root.as_ref().to_path_buf();
        let canonical = std::fs::canonicalize(&path).map_err(|source| WorkspaceError::Io {
            path: path.clone(),
            source,
        })?;
        if !canonical.is_dir() {
            return Err(WorkspaceError::NotADirectory(canonical));
        }
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a workspace-relative path that must exist and stay inside the workspace, for a
    /// host tool that needs the same boundary the file tools use (ADR 0030 R10's `git_commit`).
    /// Rejects absolute paths, `..` and symlinks out of the workspace; returns the canonical
    /// path or the reason as text.
    pub fn resolve_in_workspace(&self, user: &str) -> Result<PathBuf, String> {
        self.resolve_existing(user)
            .map_err(|failure| failure.message)
    }

    /// Rejects absolute paths and Windows prefixes before any filesystem access, then joins
    /// the workspace-relative path onto the root.
    fn joined(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        let relative = Path::new(user);
        if relative.is_absolute() {
            return Err(fail(
                CODE_OUTSIDE,
                format!("`{user}` is an absolute path; only workspace-relative paths are allowed"),
            ));
        }
        // Reject `..` before touching the filesystem rather than relying on
        // canonicalization to notice: it keeps the boundary a path-syntax property (and
        // gives the boundary code even when the escaped target does not exist).
        if relative.components().any(|c| {
            matches!(
                c,
                Component::RootDir | Component::Prefix(_) | Component::ParentDir
            )
        }) {
            return Err(fail(
                CODE_OUTSIDE,
                format!("`{user}` escapes the workspace (absolute paths and `..` are refused)"),
            ));
        }
        Ok(self.root.join(relative))
    }

    /// Resolves a path that must already exist: canonicalizes it and requires the result to
    /// stay under the root. This is what catches `..`, symlinks, and symlinked parents for
    /// `read`/`list`/`patch` (and for `git_diff`'s pathspec).
    pub(crate) fn resolve_existing(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        let joined = self.joined(user)?;
        let real = std::fs::canonicalize(&joined).map_err(|e| io_failure(user, e))?;
        self.ensure_inside(user, &real)?;
        Ok(real)
    }

    /// Resolves the directory a new or existing file lives in, for `write`/`patch`: the
    /// parent is canonicalized and checked, then the final name is appended. A file that
    /// does not exist yet has no canonical path, which is exactly why the parent is used.
    pub(crate) fn resolve_parent_for_write(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        let joined = self.joined(user)?;
        let Some(name) = joined.file_name() else {
            return Err(fail(CODE_OUTSIDE, format!("`{user}` does not name a file")));
        };
        let parent = joined.parent().unwrap_or(self.root.as_path());
        let parent_real = std::fs::canonicalize(parent).map_err(|e| io_failure(user, e))?;
        self.ensure_inside(user, &parent_real)?;
        Ok(parent_real.join(name))
    }

    /// Refuses a `write`/`patch` target that is a symlink (never followed) or is
    /// hard-linked (nlink > 1; see the crate docs on why writes refuse that).
    pub(crate) fn guard_mutation_target(
        &self,
        user: &str,
        target: &Path,
    ) -> Result<(), ToolFailure> {
        let Ok(metadata) = std::fs::symlink_metadata(target) else {
            return Ok(()); // does not exist yet: a fresh file
        };
        if metadata.file_type().is_symlink() {
            match std::fs::canonicalize(target) {
                Ok(real) if !real.starts_with(&self.root) => {
                    return Err(self.outside(user, "a symlink pointing outside the workspace"));
                }
                // Includes a dangling link: it is still a link, and we do not follow it.
                _ => {
                    return Err(fail(
                        "path_is_symlink",
                        format!("`{user}` is a symlink; this tool never writes through a link"),
                    ));
                }
            }
        }
        if hard_link_count(&metadata) > 1 {
            return Err(fail(
                "hard_link_refused",
                format!(
                    "`{user}` has more than one hard link, so writing it could change a file \
                     outside the workspace; the tool refuses"
                ),
            ));
        }
        Ok(())
    }

    fn ensure_inside(&self, user: &str, real: &Path) -> Result<(), ToolFailure> {
        if real.starts_with(&self.root) {
            Ok(())
        } else {
            Err(self.outside(user, "it resolves outside the workspace"))
        }
    }

    fn outside(&self, user: &str, why: &str) -> ToolFailure {
        fail(
            CODE_OUTSIDE,
            format!(
                "`{user}` is not inside the workspace `{}`: {why}",
                self.root.display()
            ),
        )
    }
}

#[cfg(unix)]
fn hard_link_count(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(unix))]
fn hard_link_count(_metadata: &std::fs::Metadata) -> u64 {
    // No portable link count; the hard-link refusal is Unix-only and documented as such.
    1
}

/// Maps an io error to a stable tool failure code, naming the user's path (never the
/// resolved absolute path, which the model did not ask for).
pub(crate) fn io_failure(user: &str, error: std::io::Error) -> ToolFailure {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::NotADirectory => "not_a_directory",
        std::io::ErrorKind::IsADirectory => "is_a_directory",
        _ => "io_error",
    };
    fail(code, format!("`{user}`: {error}"))
}

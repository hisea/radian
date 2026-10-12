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

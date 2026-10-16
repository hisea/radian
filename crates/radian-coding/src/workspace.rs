use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_core::ToolFailure;

use crate::safe_fs::{self, EntryKind, OpenMode, RootDir, SafeFsError, Stat};
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

/// The canonical workspace root, held as a directory fd (ADR 0039 decision 2). Constructed
/// once per tool provider, so a symlinked root is resolved before anything is opened; every
/// later open walks from this fd and never re-opens a path string.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    root: PathBuf,
    dir: Arc<RootDir>,
}

/// An open workspace file with the facts the tools check before using it.
pub(crate) struct OpenedFile {
    pub file: File,
    pub stat: Stat,
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
        let dir = RootDir::open(&canonical).map_err(|source| WorkspaceError::Io {
            path: canonical.clone(),
            source,
        })?;
        Ok(Self {
            root: canonical,
            dir: Arc::new(dir),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks a workspace-relative path for a host tool that needs the same boundary the file
    /// tools use (ADR 0030 R10's `git_commit`): no absolute path, no `..`, no symlink in any
    /// component (the parents are walked by fd; the leaf is `lstat`ed, never opened). Returns
    /// the path under the canonical root, or the reason as text. The leaf need not exist.
    pub fn resolve_in_workspace(&self, user: &str) -> Result<PathBuf, String> {
        let relative = self
            .relative_checked(user)
            .map_err(|failure| failure.message)?;
        Ok(self.root.join(relative))
    }

    /// Rejects absolute paths and Windows prefixes before any filesystem access, then joins
    /// the workspace-relative path onto the root. Lexical only: the fd walk does the rest.
    fn joined(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        let relative = Path::new(user);
        if relative.is_absolute() {
            return Err(fail(
                CODE_OUTSIDE,
                format!("`{user}` is an absolute path; only workspace-relative paths are allowed"),
            ));
        }
        // Reject `..` before touching the filesystem rather than relying on the walk to
        // notice: it keeps the boundary a path-syntax property (and gives the boundary code
        // even when the escaped target does not exist).
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

    /// The lexical check only, as a relative path (`.` and empty components dropped).
    fn checked_relative(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        self.joined(user)?;
        safe_fs::components(Path::new(user))
            .map(|parts| parts.iter().collect())
            .map_err(|error| self.map_error(user, error))
    }

    /// Opens `user` for reading through the fd walk and reports what it is. Any symlink
    /// component is refused; the caller checks `stat.kind`.
    pub(crate) fn open_read(&self, user: &str) -> Result<OpenedFile, ToolFailure> {
        let relative = self.checked_relative(user)?;
        let file = self
            .dir
            .file(&relative, OpenMode::Read)
            .map_err(|error| self.map_error(user, error))?;
        let stat = safe_fs::fstat(&file).map_err(|e| io_failure(user, e))?;
        Ok(OpenedFile { file, stat })
    }

    /// Opens the directory `user` names (`.` is the root) through the fd walk.
    pub(crate) fn open_dir(&self, user: &str) -> Result<File, ToolFailure> {
        let relative = self.checked_relative(user)?;
        self.dir
            .dir(&relative)
            .map_err(|error| self.map_error(user, error))
    }

    /// Opens `user` for an in-place overwrite: parents walked by fd, the leaf opened with
    /// `O_NOFOLLOW` (created when `create`), then checked to be a regular file with one hard
    /// link **before** anything is truncated. `write` passes `create = true`; `patch` requires
    /// the file to exist.
    pub(crate) fn open_write(&self, user: &str, create: bool) -> Result<OpenedFile, ToolFailure> {
        let relative = self.checked_relative(user)?;
        if relative.as_os_str().is_empty() {
            return Err(fail(CODE_OUTSIDE, format!("`{user}` does not name a file")));
        }
        let mode = if create {
            OpenMode::WriteCreate
        } else {
            OpenMode::ReadWrite
        };
        let file = self
            .dir
            .file(&relative, mode)
            .map_err(|error| self.map_error(user, error))?;
        let stat = safe_fs::fstat(&file).map_err(|e| io_failure(user, e))?;
        if stat.kind != EntryKind::File {
            return Err(fail(
                "not_a_file",
                format!("`{user}` is not a regular file"),
            ));
        }
        if stat.nlink > 1 {
            return Err(fail(
                "hard_link_refused",
                format!(
                    "`{user}` has more than one hard link, so writing it could change a file \
                     outside the workspace; the tool refuses"
                ),
            ));
        }
        Ok(OpenedFile { file, stat })
    }

    /// A workspace-relative path whose parents exist and hold no symlink (walked by fd) and
    /// whose leaf, if it exists, is not a symlink (`lstat`, never opened). For `git_diff`'s
    /// pathspec and `git_commit`: the path is handed to git relative to the root, so the
    /// check is what keeps a link out of the pathspec.
    pub(crate) fn relative_checked(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        let relative = self.checked_relative(user)?;
        if relative.as_os_str().is_empty() {
            return Ok(relative);
        }
        let (dir, name) = self
            .dir
            .parent_of(&relative)
            .map_err(|error| self.map_error(user, error))?;
        match safe_fs::fstatat(&dir, &name) {
            Ok(stat) if stat.kind == EntryKind::Symlink => {
                let error = self.dir.file(&relative, OpenMode::Read).err().unwrap_or(
                    SafeFsError::Symlink {
                        component: relative.clone(),
                        inside: None,
                    },
                );
                Err(self.map_error(user, error))
            }
            _ => Ok(relative),
        }
    }

    /// The path of a directory the `shell` tool will spawn a child in: walked by fd (no
    /// symlink in any component, must be a directory), then returned under the canonical root
    /// for the spawn. The spawn itself is by path — the sandbox policy, not this check, is what
    /// confines the child; this keeps a link out of the `cwd` argument.
    pub(crate) fn checked_dir_path(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        let relative = self.checked_relative(user)?;
        let _dir = self.open_dir(user)?;
        Ok(self.root.join(relative))
    }

    /// Up to `limit` workspace-relative names in the same directory as `user` that look like a
    /// typo of its last component, closest first; a weak model that misspells a path gets the
    /// real one back instead of a bare "not found".
    ///
    /// Only the requested file's directory is listed, through the fd walk, so a suggestion can
    /// never point outside the workspace (a symlinked directory yields nothing). A path that
    /// is absolute, holds `..`, or names something in a directory that does not exist yields
    /// nothing.
    pub(crate) fn suggest_similar(&self, user: &str, limit: usize) -> Vec<String> {
        let Ok(relative) = self.checked_relative(user) else {
            return Vec::new();
        };
        let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
            return Vec::new();
        };
        let Ok((dir, _)) = self.dir.parent_of(&relative) else {
            return Vec::new();
        };
        let Ok(entries) = safe_fs::read_dir(&dir) else {
            return Vec::new();
        };
        let threshold = suggestion_threshold(name);
        let mut scored: Vec<(usize, String)> = Vec::new();
        for entry in entries {
            let Some(candidate) = entry.name.to_str().map(str::to_owned) else {
                continue;
            };
            if candidate == name {
                continue;
            }
            let distance = edit_distance_ci(name, &candidate);
            if distance <= threshold {
                scored.push((distance, candidate));
            }
        }
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        scored.truncate(limit);
        let relative_parent = relative.parent().unwrap_or(Path::new(""));
        scored
            .into_iter()
            .map(|(_, candidate)| {
                relative_parent
                    .join(candidate)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// Maps a walk error to the tool failure the model sees. A symlink that stays inside the
    /// workspace is named with its target (`path_is_symlink`); one that leaves it, or whose
    /// target cannot be read, is a boundary refusal that never prints the outside path.
    pub(crate) fn map_error(&self, user: &str, error: SafeFsError) -> ToolFailure {
        match error {
            SafeFsError::Absolute => fail(
                CODE_OUTSIDE,
                format!("`{user}` is an absolute path; only workspace-relative paths are allowed"),
            ),
            SafeFsError::ParentDir => fail(
                CODE_OUTSIDE,
                format!("`{user}` escapes the workspace (absolute paths and `..` are refused)"),
            ),
            SafeFsError::Symlink {
                component,
                inside: Some(target),
            } => fail(
                "path_is_symlink",
                format!(
                    "`{}` is a symlink to `{}`; use that path (the file tools never follow a link)",
                    component.display(),
                    target.display()
                ),
            ),
            SafeFsError::Symlink {
                component,
                inside: None,
            } => fail(
                CODE_OUTSIDE,
                format!(
                    "`{}` is a symlink leaving the workspace `{}`; the file tools never follow a link",
                    component.display(),
                    self.root.display()
                ),
            ),
            SafeFsError::NotADirectory { component } => fail(
                "not_a_directory",
                format!("`{}` is not a directory", component.display()),
            ),
            SafeFsError::IsADirectory { component } => fail(
                "is_a_directory",
                format!("`{}` is a directory", component.display()),
            ),
            SafeFsError::Io { component, source } => {
                io_failure(&component.to_string_lossy(), source)
            }
        }
    }
}

/// How far a name may differ (character edits, case-ignoring) and still be a suggestion.
/// Short names allow one edit; longer names allow up to four. This bounds the noise: a
/// `Cargo.toml` does not "suggest" an unrelated `LICENSE`.
fn suggestion_threshold(name: &str) -> usize {
    (name.chars().count() / 3).clamp(1, 4)
}

/// Levenshtein distance over characters, case-insensitive. Names are short, so the simple
/// two-row dynamic program is enough.
pub(crate) fn edit_distance_ci(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = if ca == cb { 0 } else { 1 };
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
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

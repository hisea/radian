use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_core::ToolFailure;

use crate::read_roots::{ReadRoots, normalise_absolute, user_home};
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
///
/// Alongside the workspace it holds the read-only roots (ADR 0039: the cargo
/// registry and toolchain sources). `read`/`grep`/`glob`/`list` resolve an absolute or `~`
/// path against those roots; every other tool uses the workspace only.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    root: PathBuf,
    dir: Arc<RootDir>,
    read_roots: Arc<ReadRoots>,
}

/// An open workspace file with the facts the tools check before using it.
pub(crate) struct OpenedFile {
    pub file: File,
    pub stat: Stat,
    /// Whether the fd was opened read-write and can be read back. Only `write`'s count needs it:
    /// its fallback open is write-only (review fix 1).
    pub readable: bool,
}

/// The directory a search tool (`grep`/`glob`) walks: the open fd, the path relative to the
/// root it was opened from, and, for a read-only root, the prefix to render a result path the
/// model can pass back to `read`.
pub(crate) struct ReadBase {
    pub dir: File,
    pub relative: PathBuf,
    pub prefix: Option<PathBuf>,
}

/// Where a read tool's `path` resolved to.
enum Resolved {
    /// A workspace-relative path.
    Workspace(PathBuf),
    /// A path under one of the read-only roots (ADR 0039).
    Root {
        index: usize,
        relative: PathBuf,
        prefix: PathBuf,
    },
}

impl WorkspaceRoot {
    /// The workspace only, with no read-only roots.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        Self::with_read_roots(root, ReadRoots::none())
    }

    /// The workspace plus the read-only roots the file tools may read (ADR 0039).
    pub fn with_read_roots(
        root: impl AsRef<Path>,
        read_roots: ReadRoots,
    ) -> Result<Self, WorkspaceError> {
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
            read_roots: Arc::new(read_roots),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks a workspace-relative path for a host tool that needs the same boundary the file
    /// tools use: no absolute path, no `..`, no symlink in any
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

    /// Opens `user` for reading through the fd walk and reports what it is. A relative path
    /// is workspace-relative; an absolute or `~` path is resolved against the read-only roots
    /// (ADR 0039) and refused when it is under none of them. Any symlink component
    /// is refused; the caller checks `stat.kind`.
    pub(crate) fn open_read(&self, user: &str) -> Result<OpenedFile, ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => self.open_workspace_file(user, &relative),
            Resolved::Root {
                index, relative, ..
            } => self.open_root_file(user, index, &relative),
        }
    }

    /// Opens the directory `user` names (`.` is the root) through the fd walk. Same resolution
    /// as [`WorkspaceRoot::open_read`].
    pub(crate) fn open_dir(&self, user: &str) -> Result<File, ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => self.open_workspace_dir(user, &relative),
            Resolved::Root {
                index, relative, ..
            } => self.open_root_dir(user, index, &relative),
        }
    }

    /// Opens the base directory a search tool walks, with the path relative to its root and,
    /// for a read-only root, the prefix to render result paths with.
    pub(crate) fn read_base(&self, user: &str) -> Result<ReadBase, ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => {
                let dir = self.open_workspace_dir(user, &relative)?;
                Ok(ReadBase {
                    dir,
                    relative,
                    prefix: None,
                })
            }
            Resolved::Root {
                index,
                relative,
                prefix,
            } => {
                let dir = self.open_root_dir(user, index, &relative)?;
                Ok(ReadBase {
                    dir,
                    relative,
                    prefix: Some(prefix),
                })
            }
        }
    }

    /// The path `user` resolves to relative to its root, and the read-only root's prefix when
    /// it is not a workspace path. For a search tool's file fallback and for rendering paths.
    pub(crate) fn read_relative(
        &self,
        user: &str,
    ) -> Result<(PathBuf, Option<PathBuf>), ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => Ok((relative, None)),
            Resolved::Root {
                relative, prefix, ..
            } => Ok((relative, Some(prefix))),
        }
    }

    /// Resolves a read tool's `path` to the workspace or one of the read-only roots (ADR 0039
    /// revision 1). A relative path is workspace-relative and keeps the existing checks; an
    /// absolute path, or one written with `~`, is refused unless it falls under a read-only
    /// root, and the refusal names the readable roots.
    fn resolve_read(&self, user: &str) -> Result<Resolved, ToolFailure> {
        if is_absolute_or_home(user) {
            let candidate = expand_home(user)
                .and_then(|path| normalise_absolute(&path))
                .ok_or_else(|| outside_read_failure(user))?;
            let found = self
                .read_roots
                .lookup(&candidate)
                .ok_or_else(|| outside_read_failure(user))?;
            return Ok(Resolved::Root {
                index: found.index,
                relative: found.relative,
                prefix: found.prefix,
            });
        }
        Ok(Resolved::Workspace(self.checked_relative(user)?))
    }

    /// Opens a workspace-relative path for reading (the existing boundary, unchanged).
    fn open_workspace_file(&self, user: &str, relative: &Path) -> Result<OpenedFile, ToolFailure> {
        let file = self
            .dir
            .file(relative, OpenMode::Read)
            .map_err(|error| self.map_error(user, error))?;
        opened_file(user, file, true)
    }

    /// Opens a workspace-relative directory (the existing boundary, unchanged).
    fn open_workspace_dir(&self, user: &str, relative: &Path) -> Result<File, ToolFailure> {
        self.dir
            .dir(relative)
            .map_err(|error| self.map_error(user, error))
    }

    /// Opens a path under a read-only root through that root's fd.
    fn open_root_file(
        &self,
        user: &str,
        index: usize,
        relative: &Path,
    ) -> Result<OpenedFile, ToolFailure> {
        let root = &self.read_roots.roots()[index];
        let file = root
            .file(relative, OpenMode::Read)
            .map_err(|error| self.map_root_error(index, user, error))?;
        opened_file(user, file, true)
    }

    /// Opens a directory under a read-only root through that root's fd.
    fn open_root_dir(
        &self,
        user: &str,
        index: usize,
        relative: &Path,
    ) -> Result<File, ToolFailure> {
        let root = &self.read_roots.roots()[index];
        root.dir(relative)
            .map_err(|error| self.map_root_error(index, user, error))
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
        // `write` wants to count the old file's lines on the very fd it truncates, which needs a
        // read-write open; a file the user may write but not read refuses that with EACCES. The
        // write is the point and the count is a display nicety, so on that one error fall back
        // to the write-only create and let `write` skip the count (review fix 1). `patch` edits
        // an existing file and always reads it.
        let (file, readable) = if create {
            match self.dir.file(&relative, OpenMode::WriteCreateReadWrite) {
                Ok(file) => (file, true),
                Err(error) if is_permission_denied(&error) => {
                    let file = self
                        .dir
                        .file(&relative, OpenMode::WriteCreate)
                        .map_err(|error| self.map_error(user, error))?;
                    (file, false)
                }
                Err(error) => return Err(self.map_error(user, error)),
            }
        } else {
            let file = self
                .dir
                .file(&relative, OpenMode::ReadWrite)
                .map_err(|error| self.map_error(user, error))?;
            (file, true)
        };
        let opened = opened_file(user, file, readable)?;
        let stat = opened.stat;
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
        Ok(opened)
    }

    /// A workspace-relative path whose parents exist and hold no symlink (walked by fd) and
    /// whose leaf, if it exists, is not a symlink (`lstat`, never opened). For `git_diff`'s
    /// pathspec: the path is handed to git relative to the root, so the
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
        let _dir = self.open_workspace_dir(user, &relative)?;
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
        self.map_error_at(&self.root, "workspace", user, error)
    }

    /// [`WorkspaceRoot::map_error`] for a path under a read-only root (ADR 0039):
    /// the same failure codes, but "leaving the readable root" and the root's canonical path.
    fn map_root_error(&self, index: usize, user: &str, error: SafeFsError) -> ToolFailure {
        let root = &self.read_roots.roots()[index];
        self.map_error_at(root.canonical(), "readable root", user, error)
    }

    fn map_error_at(&self, root: &Path, noun: &str, user: &str, error: SafeFsError) -> ToolFailure {
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
                    "`{}` is a symlink leaving the {noun} `{}`; the file tools never follow a link",
                    component.display(),
                    root.display()
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

/// Where a read tool's path came from, for the message that refuses an absolute/`~` path under
/// no readable root (ADR 0039). It keeps naming the path kind, as the
/// workspace-only refusal did, and lists the two readable roots.
fn outside_read_failure(user: &str) -> ToolFailure {
    let kind = if user.starts_with('~') {
        "a `~` path"
    } else {
        "an absolute path"
    };
    fail(
        CODE_OUTSIDE,
        format!(
            "`{user}` is {kind} outside the workspace; the file tools also read, read-only, {}",
            crate::read_roots::READ_ROOTS_DESCRIPTION
        ),
    )
}

/// Whether the model wrote `user` as an absolute path or with `~` (the spellings a read-only
/// root is named with). `~user` is not a home reference here: it stays a relative path.
fn is_absolute_or_home(user: &str) -> bool {
    Path::new(user).is_absolute() || user == "~" || user.starts_with("~/")
}

/// The absolute path `user` names: its own path when absolute, or `~`/`~/…` expanded with
/// `HOME`. `None` for a relative path, or for `~` when `HOME` is unset (never guessed).
fn expand_home(user: &str) -> Option<PathBuf> {
    let path = Path::new(user);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let rest = user.strip_prefix('~')?;
    Some(user_home()?.join(rest.trim_start_matches('/')))
}

/// Wraps an open file with the facts the tools check, running the test seam first (ADR 0039
/// decision 2: the fd obtained here is the one used, never a path re-opened later). `readable`
/// is whether the fd's mode allows reading it back (every read path does; `write`'s write-only
/// fallback does not).
fn opened_file(user: &str, file: File, readable: bool) -> Result<OpenedFile, ToolFailure> {
    safe_fs::run_after_open_hook();
    let stat = safe_fs::fstat(&file).map_err(|e| io_failure(user, e))?;
    Ok(OpenedFile {
        file,
        stat,
        readable,
    })
}

/// Whether an open failed because the process was not allowed to (Unix `EACCES`), the one
/// error `write` falls back on when its read-write create is refused: the write-only open may
/// still be allowed (review fix 1). Every other failure — a symlink, a missing parent, an
/// outside path — is propagated, never retried.
fn is_permission_denied(error: &SafeFsError) -> bool {
    matches!(
        error,
        SafeFsError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// Renders a search result path: relative to its root for the workspace, prefixed with the
/// spelling of the read-only root the model used otherwise, so the path can be passed to
/// `read` again (ADR 0039).
pub(crate) fn display_path(prefix: Option<&Path>, relative: &Path) -> String {
    match prefix {
        Some(prefix) => prefix.join(relative).to_string_lossy().into_owned(),
        None => relative.to_string_lossy().into_owned(),
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

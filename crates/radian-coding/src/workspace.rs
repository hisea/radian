use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use radian_core::ToolFailure;

use crate::deny::DenySet;
use crate::docs;
use crate::read_roots::{READ_ROOTS_DESCRIPTION, ReadRoots, normalise_absolute, user_home};
use crate::safe_fs::{self, EntryKind, OpenMode, RootDir, SafeFsError, Stat};
use crate::{CODE_OUTSIDE, fail};
use radian_sandbox::{SandboxMode, SandboxSlot};

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
/// registry and toolchain sources, plus each discovered skill's directory), the `full`-tier
/// deny set (ADR 0039 decision 4), and the tier source: the shared [`SandboxSlot`] the host
/// swaps on `/access`, so the file tools read the current tier at every call, not the
/// startup one. `read`/`grep`/`glob` resolve an absolute or `~` path against the roots in
/// both tiers, and against any other path in the `full` tier except the deny set; every
/// other tool uses the workspace only. An absolute path under the workspace root, in either
/// spelling, is the workspace-relative path (ADR 0039 decision 6).
#[derive(Clone)]
pub struct WorkspaceRoot {
    /// The canonical workspace root (what the directory fd points at).
    root: PathBuf,
    /// The workspace path as the host gave it, lexically normalised, when it differs from
    /// [`Self::root`]: the second spelling an absolute workspace path may use (ADR 0039
    /// decision 6), as `/tmp/ws` beside the canonical `/private/tmp/ws` on macOS.
    given: Option<PathBuf>,
    dir: Arc<RootDir>,
    read_roots: Arc<ReadRoots>,
    /// The `full`-tier deny set: the sandbox's deny-read entries plus the host's denied
    /// `$RADIAN_HOME` paths. Consulted only when the tier is `full`.
    deny: Arc<DenySet>,
    /// The tier in force, read at every call (ADR 0038): the host's shared slot, so a
    /// mid-session tier switch changes what the file tools read at the next call.
    tier: SandboxSlot,
    /// The filesystem root's directory fd, for `full`-tier reads outside every other root.
    /// A canonical outside path is walked from here with the same per-component
    /// `O_NOFOLLOW` walk the workspace uses.
    fs: Arc<RootDir>,
}

/// An open workspace file with the facts the tools check before using it.
///
/// A file is either a real descriptor or an embedded docs page (`radian://docs/`).
/// The embedded pages are read-only memory; `write`/`patch` never open them.
pub(crate) enum OpenedSource {
    File(File),
    Embedded(&'static [u8]),
}

pub(crate) struct OpenedFile {
    pub source: OpenedSource,
    pub stat: Stat,
    /// Whether the fd was opened read-write and can be read back. Only `write`'s count needs it:
    /// its fallback open is write-only (review fix 1). Embedded pages are always readable.
    pub readable: bool,
}

/// The directory a search tool (`grep`/`glob`) walks: the open fd, the path relative to the
/// root it was opened from, and, for a read-only root, the prefix to render a result path the
/// model can pass back to `read`.
pub(crate) struct ReadBase {
    pub dir: File,
    pub relative: PathBuf,
    pub prefix: Option<PathBuf>,
    /// The ignore matchers for every directory from the root down to (but not including) the
    /// base ([`crate::search::ancestor_matchers`]), outermost first. The walk starts with
    /// these, so a repository-root ignore keeps working when the search starts below it.
    pub ancestors: Vec<ignore::gitignore::Gitignore>,
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
    /// A `full`-tier path outside every other root: `relative` is the `realpath`ed path
    /// without its leading `/`, walked from the filesystem root's fd (ADR 0039 decision 4).
    /// The deny set was already checked on the `realpath`ed path.
    Free { relative: PathBuf },
    /// A bundled docs page (`radian://docs/`). `None` is the root (a listing);
    /// `Some` is one page by file name.
    Embedded { page: Option<String> },
}

impl WorkspaceRoot {
    /// The workspace only, with no read-only roots and no deny set. The tier source is
    /// empty, which reads as the `workspace` tier (outside paths refuse).
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        Self::with_read_roots(root, ReadRoots::none())
    }

    /// The workspace plus the read-only roots the file tools may read (ADR 0039). No deny
    /// set and no tier source: outside paths outside the roots refuse, as in the
    /// `workspace` tier. For tests and embedders.
    pub fn with_read_roots(
        root: impl AsRef<Path>,
        read_roots: ReadRoots,
    ) -> Result<Self, WorkspaceError> {
        Self::with_access(root, read_roots, DenySet::empty(), SandboxSlot::empty())
    }

    /// The workspace plus the read-only roots, the `full`-tier deny set and the tier source
    /// (ADR 0039 decision 4): the host passes the skill roots from its skill discovery, its
    /// deny-read list, and its shared sandbox slot, so a tier switch changes what the file
    /// tools read at the next call.
    pub fn with_access(
        root: impl AsRef<Path>,
        read_roots: ReadRoots,
        deny: DenySet,
        tier: SandboxSlot,
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
        let fs = RootDir::open(Path::new("/")).map_err(|source| WorkspaceError::Io {
            path: PathBuf::from("/"),
            source,
        })?;
        let given = normalise_absolute(&path).filter(|given| given != &canonical);
        Ok(Self {
            root: canonical,
            given,
            dir: Arc::new(dir),
            read_roots: Arc::new(read_roots),
            deny: Arc::new(deny),
            tier,
            fs: Arc::new(fs),
        })
    }

    /// Whether the file tools are in the `full` tier right now: the shared slot holds a
    /// `FullAccess` sandbox. Read at every call (ADR 0038), so a mid-session tier switch
    /// changes the answer at the next call. An empty slot (no sandbox) reads as `workspace`.
    fn is_full_tier(&self) -> bool {
        self.tier
            .get()
            .is_some_and(|sandbox| sandbox.mode() == SandboxMode::FullAccess)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks a workspace path (relative, or absolute inside the workspace) for a host tool
    /// that needs the same boundary the file tools use: no `..`, no symlink in any
    /// component (the parents are walked by fd; the leaf is `lstat`ed, never opened). Returns
    /// the path under the canonical root, or the reason as text. The leaf need not exist.
    pub fn resolve_in_workspace(&self, user: &str) -> Result<PathBuf, String> {
        let relative = self
            .relative_checked(user)
            .map_err(|failure| failure.message)?;
        Ok(self.root.join(relative))
    }

    /// The workspace-relative remainder of an absolute path under the workspace root, in
    /// either spelling (the canonical root or the path the host gave, ADR 0039 decision 6).
    /// The prefix test compares path components, so `/app2/x` is not under `/app`; the root
    /// itself (`/app`, `/app/`) is `.`. `None` for a path that is not absolute, holds `..`
    /// (refused before any conversion), or is under neither spelling. Lexical only: the
    /// remainder then goes through exactly the checks a relative path gets.
    fn workspace_remainder(&self, user: &str) -> Option<PathBuf> {
        let path = Path::new(user);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return None;
        }
        let spellings = std::iter::once(&self.root).chain(self.given.iter());
        for spelling in spellings {
            if let Ok(remainder) = path.strip_prefix(spelling) {
                return Some(remainder.to_path_buf());
            }
        }
        None
    }

    /// The path the workspace checks run on: an absolute path under the workspace root in
    /// either spelling becomes its workspace-relative remainder (ADR 0039 decision 6); every
    /// other path, `..` included, is returned as written.
    fn workspace_relative(&self, user: &str) -> PathBuf {
        self.workspace_remainder(user)
            .unwrap_or_else(|| PathBuf::from(user))
    }

    /// Rejects absolute paths outside the workspace and Windows prefixes before any filesystem
    /// access, then joins the workspace-relative path onto the root. An absolute path under
    /// the workspace root in either spelling is first rewritten to its remainder (ADR 0039
    /// decision 6); one that holds `..` is refused as before, before any conversion. Lexical
    /// only: the fd walk does the rest. The bundled manual (`radian://`) is read-only and
    /// never a workspace path.
    fn joined(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        if docs::is_virtual(user) {
            return Err(docs_write_failure(user));
        }
        let relative = self.workspace_relative(user);
        if relative.is_absolute() {
            return Err(fail(
                CODE_OUTSIDE,
                format!(
                    "`{user}` is an absolute path outside the workspace; use a workspace-relative \
                     path or an absolute path inside `{}`",
                    self.root.display()
                ),
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
                format!("`{user}` escapes the workspace (`..` is refused)"),
            ));
        }
        Ok(self.root.join(relative))
    }

    /// The lexical check only, as a relative path (`.` and empty components dropped).
    fn checked_relative(&self, user: &str) -> Result<PathBuf, ToolFailure> {
        self.joined(user)?;
        safe_fs::components(&self.workspace_relative(user))
            .map(|parts| parts.iter().collect())
            .map_err(|error| self.map_error(user, error))
    }

    /// Opens `user` for reading through the fd walk and reports what it is. A relative path,
    /// and an absolute path inside the workspace in either spelling (ADR 0039 decision 6), is
    /// workspace-relative; any other absolute or `~` path is resolved against the read-only
    /// roots (ADR 0039) and refused when it is under none of them — except in the `full` tier,
    /// where any other absolute or `~` path reads too, unless the deny set denies it.
    /// `radian://docs/`
    /// names the bundled manual and is read from memory. Any symlink component
    /// is refused; the caller checks `stat.kind`.
    pub(crate) fn open_read(&self, user: &str) -> Result<OpenedFile, ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => self.open_workspace_file(user, &relative),
            Resolved::Root {
                index, relative, ..
            } => self.open_root_file(user, index, &relative),
            Resolved::Free { relative } => self.open_free_file(user, &relative),
            Resolved::Embedded { page } => self.open_embedded_file(user, page),
        }
    }

    /// Opens the directory `user` names (`.` is the root) through the fd walk. Same resolution
    /// as [`WorkspaceRoot::open_read`]. The bundled docs root has no fd; `read` lists it
    /// without calling this, so this refuses it with the docs list.
    pub(crate) fn open_dir(&self, user: &str) -> Result<File, ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => self.open_workspace_dir(user, &relative),
            Resolved::Root {
                index, relative, ..
            } => self.open_root_dir(user, index, &relative),
            Resolved::Free { relative } => self.open_free_dir(user, &relative),
            Resolved::Embedded { page } => Err(docs_dir_failure(user, page)),
        }
    }

    /// Opens the base directory a search tool walks, with the path relative to its root and,
    /// for a read-only root, the prefix to render result paths with. The ignore matchers for
    /// the directories above the base are loaded here too (outermost first), from the same
    /// root fd the walk descends from, so no path is re-opened (ADR 0039 decision 2).
    /// The bundled manual has no directory to walk; search tools refuse it and point to `read`.
    pub(crate) fn read_base(&self, user: &str) -> Result<ReadBase, ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => {
                let dir = self.open_workspace_dir(user, &relative)?;
                let ancestors = crate::search::ancestor_matchers(self.dir.fd(), &relative);
                Ok(ReadBase {
                    dir,
                    relative,
                    prefix: None,
                    ancestors,
                })
            }
            Resolved::Root {
                index,
                relative,
                prefix,
            } => {
                let dir = self.open_root_dir(user, index, &relative)?;
                let root_fd = self.read_roots.roots()[index].root_dir().fd();
                let ancestors = crate::search::ancestor_matchers(root_fd, &relative);
                Ok(ReadBase {
                    dir,
                    relative,
                    prefix: Some(prefix),
                    ancestors,
                })
            }
            Resolved::Free { relative } => {
                let dir = self.open_free_dir(user, &relative)?;
                // No above-base ignore matchers: they would be loaded by walking from the
                // filesystem root (e.g. listing the system temp directory, which can hold
                // hundreds of thousands of entries) for every search. Ignore files at and
                // below the base still apply through the walk itself.
                Ok(ReadBase {
                    dir,
                    relative,
                    prefix: Some(PathBuf::from("/")),
                    ancestors: Vec::new(),
                })
            }
            Resolved::Embedded { .. } => Err(docs_search_failure(user)),
        }
    }

    /// The path `user` resolves to relative to its root, and the read-only root's prefix when
    /// it is not a workspace path. For a search tool's file fallback and for rendering paths.
    /// The bundled manual has no relative path; search tools refuse it and point to `read`.
    pub(crate) fn read_relative(
        &self,
        user: &str,
    ) -> Result<(PathBuf, Option<PathBuf>), ToolFailure> {
        match self.resolve_read(user)? {
            Resolved::Workspace(relative) => Ok((relative, None)),
            Resolved::Root {
                relative, prefix, ..
            } => Ok((relative, Some(prefix))),
            Resolved::Free { relative } => Ok((relative, Some(PathBuf::from("/")))),
            Resolved::Embedded { .. } => Err(docs_search_failure(user)),
        }
    }

    /// Resolves a read tool's `path` to the bundled manual, the workspace, one of the
    /// read-only roots, or — in the `full` tier — any other absolute or `~` path outside the
    /// deny set (ADR 0039 decision 4). `radian://docs/` is checked before the
    /// absolute/`~` check. An absolute path under the workspace root in either spelling is a
    /// workspace path and takes the workspace route (ADR 0039 decision 6), never the outside
    /// one. A relative path is workspace-relative and keeps the existing checks; an absolute
    /// path, or one written with `~`, that is under no root is refused in the `workspace`
    /// tier (the refusal lists the readable roots, now including skill directories), and
    /// `realpath`ed and deny-checked in the `full` tier.
    fn resolve_read(&self, user: &str) -> Result<Resolved, ToolFailure> {
        if docs::is_root(user) {
            return Ok(Resolved::Embedded { page: None });
        }
        if let Some(name) = docs::page_name(user) {
            return Ok(Resolved::Embedded { page: Some(name) });
        }
        if docs::is_virtual(user) {
            return Err(fail(
                CODE_OUTSIDE,
                format!(
                    "`{user}` is not a bundled manual path; use `read` on `radian://docs/` ({}), \
                     or a workspace-relative path",
                    docs::names().join(", ")
                ),
            ));
        }
        if is_absolute_or_home(user) {
            // ADR 0039 decision 6: an absolute path under the workspace root in either
            // spelling is a workspace path. It takes the workspace route — the same checks a
            // relative path gets, and relative output naming — not the read-only-root or
            // `full`-tier outside route.
            if self.workspace_remainder(user).is_some() {
                return Ok(Resolved::Workspace(self.checked_relative(user)?));
            }
            let candidate = expand_home(user)
                .and_then(|path| normalise_absolute(&path))
                .ok_or_else(|| outside_read_failure(user, &self.skill_dirs()))?;
            if let Some(found) = self.read_roots.lookup(&candidate) {
                return Ok(Resolved::Root {
                    index: found.index,
                    relative: found.relative,
                    prefix: found.prefix,
                });
            }
            // ADR 0039 decision 4: in the `full` tier any other absolute or `~` path reads
            // too, except the deny set. The tier is read now, not at startup, so `/access`
            // changes the answer at the next call.
            if self.is_full_tier() {
                return self.resolve_free(user, &candidate);
            }
            return Err(outside_read_failure(user, &self.skill_dirs()));
        }
        Ok(Resolved::Workspace(self.checked_relative(user)?))
    }

    /// The skill roots' canonical paths, for the refusal that lists them.
    fn skill_dirs(&self) -> Vec<&Path> {
        self.read_roots.skill_dirs()
    }

    /// Resolves a `full`-tier path outside every other root: `realpath` it (a symlink into
    /// a denied directory refuses, even through several links or a missing tail), deny-check
    /// the result, and walk what is left from the filesystem root's fd.
    fn resolve_free(&self, user: &str, candidate: &Path) -> Result<Resolved, ToolFailure> {
        let real = std::fs::canonicalize(candidate)
            .ok()
            .or_else(|| radian_sandbox::canonicalize_existing_prefix(candidate))
            .ok_or_else(|| outside_read_failure(user, &self.skill_dirs()))?;
        if let Some(denied) = self.deny.denies(&real) {
            return Err(fail(
                CODE_OUTSIDE,
                format!(
                    "`{user}` is in the sandbox's deny set (`{}` is not readable by the file tools)",
                    denied.display()
                ),
            ));
        }
        let relative = real
            .strip_prefix("/")
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| real.clone());
        Ok(Resolved::Free { relative })
    }

    /// Opens a `full`-tier path outside every other root through the filesystem root's
    /// fd. The path was `realpath`ed before the deny check, so the walk below meets no
    /// symlink short of a concurrent swap (which the `O_NOFOLLOW` walk still refuses).
    fn open_free_file(&self, user: &str, relative: &Path) -> Result<OpenedFile, ToolFailure> {
        let file = self
            .fs
            .file(relative, OpenMode::Read)
            .map_err(|error| self.map_free_error(user, error))?;
        opened_file(user, file, true)
    }

    /// Opens a `full`-tier directory outside every other root through the filesystem
    /// root's fd.
    fn open_free_dir(&self, user: &str, relative: &Path) -> Result<File, ToolFailure> {
        self.fs
            .dir(relative)
            .map_err(|error| self.map_free_error(user, error))
    }

    /// Maps a free-path walk error to the tool failure the model sees. An io error names the
    /// absolute path (the walk's filesystem-relative component would drop its leading `/`);
    /// any other error maps like a readable-root error against the filesystem root.
    fn map_free_error(&self, user: &str, error: SafeFsError) -> ToolFailure {
        match error {
            SafeFsError::Io { component, source } => {
                let absolute = Path::new("/").join(component);
                io_failure(&absolute.to_string_lossy(), source)
            }
            error => self.map_error_at(Path::new("/"), "filesystem", user, error),
        }
    }

    /// Opens a workspace-relative path for reading (the fd walk, unchanged).
    fn open_workspace_file(&self, user: &str, relative: &Path) -> Result<OpenedFile, ToolFailure> {
        let file = self
            .dir
            .file(relative, OpenMode::Read)
            .map_err(|error| self.map_error(user, error))?;
        opened_file(user, file, true)
    }

    /// Opens a workspace-relative directory (the fd walk, unchanged).
    fn open_workspace_dir(&self, user: &str, relative: &Path) -> Result<File, ToolFailure> {
        self.dir
            .dir(relative)
            .map_err(|error| self.map_error(user, error))
    }

    /// Opens a bundled manual page from memory. `None` is the root, which has no file;
    /// `read` lists it without calling this. An unknown page names the available pages.
    fn open_embedded_file(
        &self,
        user: &str,
        page: Option<String>,
    ) -> Result<OpenedFile, ToolFailure> {
        let name = page.unwrap_or_default();
        match docs::find(&name) {
            Some(text) => Ok(OpenedFile {
                source: OpenedSource::Embedded(text.as_bytes()),
                stat: Stat {
                    kind: EntryKind::File,
                    size: text.len() as u64,
                    nlink: 1,
                },
                readable: true,
            }),
            None => Err(docs_missing_failure(user)),
        }
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
    /// the file to exist. With `create`, a missing parent level is created by `mkdirat` on the
    /// level above's fd and opened by the same `O_NOFOLLOW | O_DIRECTORY` rule (ADR 0039
    /// decision 2), so nothing is created through a link.
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
            match self
                .dir
                .file_creating_parents(&relative, OpenMode::WriteCreateReadWrite)
            {
                Ok(file) => (file, true),
                Err(error) if is_permission_denied(&error) => {
                    let file = self
                        .dir
                        .file_creating_parents(&relative, OpenMode::WriteCreate)
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
    /// whose leaf, if it exists, is not a symlink (`lstat`, never opened).
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
    /// for the spawn. An absolute path under the workspace root takes the same rewrite as every
    /// other workspace path (ADR 0039 decision 6). The spawn itself is by path — the sandbox
    /// policy, not this check, is what confines the child; this keeps a link out of the `cwd`
    /// argument.
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
    /// holds `..` or names something in a directory that does not exist yields nothing; an
    /// absolute path inside the workspace is rewritten like every other workspace path
    /// (ADR 0039 decision 6), an absolute path outside it yields nothing.
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
                format!(
                    "`{user}` is an absolute path; the file tools take a workspace path \
                     relative or absolute inside the workspace"
                ),
            ),
            SafeFsError::ParentDir => fail(
                CODE_OUTSIDE,
                format!("`{user}` escapes the workspace (`..` is refused)"),
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
/// workspace-only refusal did, and lists the readable roots including the bundled manual
/// and, when any skill roots exist, their paths.
fn outside_read_failure(user: &str, skills: &[&Path]) -> ToolFailure {
    let kind = if user.starts_with('~') {
        "a `~` path"
    } else {
        "an absolute path"
    };
    let mut message = format!(
        "`{user}` is {kind} outside the workspace; the file tools also read, read-only, {READ_ROOTS_DESCRIPTION}; ",
    );
    if !skills.is_empty() {
        let list = skills
            .iter()
            .map(|path| format!("`{}`", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        message.push_str(&format!("skill directories: {list}; "));
    }
    message.push_str(&format!(
        "the bundled manual lives at `radian://docs/` ({}), read with `read`",
        docs::names().join(", ")
    ));
    fail(CODE_OUTSIDE, message)
}

/// The bundled manual root has no directory fd; `read` lists it directly.
/// `open_dir` refuses it here so the error names the pages.
fn docs_dir_failure(user: &str, page: Option<String>) -> ToolFailure {
    match page {
        None => fail(
            "is_a_directory",
            format!(
                "`{user}` is the bundled manual root ({}); read one page with `read`",
                docs::names().join(", ")
            ),
        ),
        Some(_) => docs_missing_failure(user),
    }
}

/// A search tool (`grep`/`glob`) cannot walk the bundled manual; it has no directory fd.
/// The refusal points to `read` and the prompt index.
fn docs_search_failure(user: &str) -> ToolFailure {
    fail(
        CODE_OUTSIDE,
        format!(
            "`{user}` is a bundled manual page; `grep` and `glob` do not search it, \
             use `read radian://docs/<page>.md` (pages: {})",
            docs::names().join(", ")
        ),
    )
}

/// The bundled manual is read-only; `write`/`patch` refuse any virtual path.
fn docs_write_failure(user: &str) -> ToolFailure {
    fail(
        CODE_OUTSIDE,
        format!(
            "`{user}` is in the bundled manual (`radian://docs/`), which is read-only; \
             `write` and `patch` only write workspace paths (relative or absolute inside the \
             workspace)"
        ),
    )
}

/// An unknown manual page; the error lists the pages that `read` can open.
fn docs_missing_failure(user: &str) -> ToolFailure {
    fail(
        "not_found",
        format!(
            "`{user}` is not a bundled manual page; available pages: {}",
            docs::names().join(", ")
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
        source: OpenedSource::File(file),
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

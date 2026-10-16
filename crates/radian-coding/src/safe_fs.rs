//! fd-relative, symlink-free access to workspace paths (ADR 0039 decision 2).
//!
//! The host process is not sandboxed, and a background job (ADR 0037) can rewrite the
//! workspace while a tool runs. A tool that canonicalizes a path, checks it, and then opens
//! the path **string** again leaves a window in which a parent directory can be swapped for a
//! symlink pointing outside the workspace. This module closes that window: every component is
//! opened with `openat(dirfd, name, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)` from the workspace
//! root's own directory fd, the last one with `O_NOFOLLOW` (plus `O_CREAT`, and `O_EXCL` for a
//! file that must be new), and the tool reads or writes through the returned fd. Nothing is
//! ever re-opened by path.
//!
//! Rules, in the order they are checked:
//!
//! - absolute paths and any `..` component are refused before touching the filesystem;
//! - a component that is a symlink is refused **whether it points inside or outside** the
//!   workspace. When its target resolves (lexically) inside the workspace the error names the
//!   target as a workspace-relative path, so the model can retry in one step; when the target
//!   leaves the workspace, or cannot be read, the error says only that the link leaves the
//!   workspace — the outside path is never printed. The target is read with `readlinkat`
//!   relative to the same directory fd and is **never opened**: a relative target is
//!   normalised lexically against the link's parent, an absolute one with `realpath`
//!   (which resolves names, not contents);
//! - a component that exists but is not a directory is `not_a_directory`; a missing component
//!   is `not_found`.
//!
//! Directory listing goes through the same fd (`fdopendir` on a duplicate) and classifies each
//! entry with `fstatat(..., AT_SYMLINK_NOFOLLOW)`, so a walk never follows a link either.
//!
//! Linux offers `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`; the portable per-component
//! walk below is the one that is tested on macOS and is used on every Unix. On non-Unix
//! targets the module falls back to std's path-based opens (documented, not race-free): the
//! coding tools are only ever run on Unix hosts.
//!
//! [`resolve_hook`] is a test seam (feature `testing`): a closure run between the lexical
//! check and the first `openat`, where a test swaps a parent directory for a symlink and
//! proves the open still cannot leave the workspace.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Why a workspace path could not be opened.
#[derive(Debug)]
pub enum SafeFsError {
    /// The path is absolute.
    Absolute,
    /// The path holds `..` (or a root/prefix component).
    ParentDir,
    /// `component` (workspace-relative) is a symlink. `inside` is the lexically resolved
    /// workspace-relative target when it stays inside the workspace; `None` when it leaves
    /// the workspace or could not be read.
    Symlink {
        component: PathBuf,
        inside: Option<PathBuf>,
    },
    /// `component` exists but is not a directory (it was used as one).
    NotADirectory { component: PathBuf },
    /// `component` is a directory but a file was expected.
    IsADirectory { component: PathBuf },
    /// Any other error opening `component`.
    Io {
        component: PathBuf,
        source: io::Error,
    },
}

impl std::fmt::Display for SafeFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafeFsError::Absolute => write!(f, "absolute path"),
            SafeFsError::ParentDir => write!(f, "`..` component"),
            SafeFsError::Symlink { component, inside } => match inside {
                Some(target) => write!(
                    f,
                    "`{}` is a symlink to `{}`; use that path",
                    component.display(),
                    target.display()
                ),
                None => write!(
                    f,
                    "`{}` is a symlink leaving the workspace",
                    component.display()
                ),
            },
            SafeFsError::NotADirectory { component } => {
                write!(f, "`{}` is not a directory", component.display())
            }
            SafeFsError::IsADirectory { component } => {
                write!(f, "`{}` is a directory", component.display())
            }
            SafeFsError::Io { component, source } => {
                write!(f, "`{}`: {source}", component.display())
            }
        }
    }
}

impl std::error::Error for SafeFsError {}

/// How the last component is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// `O_RDONLY | O_NOFOLLOW`.
    Read,
    /// `O_RDWR | O_NOFOLLOW`; the file must exist (the caller truncates after its checks).
    ReadWrite,
    /// `O_WRONLY | O_CREAT | O_NOFOLLOW`; the caller truncates after its checks.
    WriteCreate,
    /// `O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW`: the file must not exist.
    WriteNew,
}

/// The kind of a directory entry, from `fstatat(..., AT_SYMLINK_NOFOLLOW)`: a link is a link,
/// never its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One directory entry as [`read_dir`] reports it.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: OsString,
    pub kind: EntryKind,
    pub size: u64,
}

/// The parts of a `stat` the tools look at.
#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub kind: EntryKind,
    pub size: u64,
    pub nlink: u64,
}

/// Splits a user path into its plain components, refusing absolute paths and `..`. `.` and
/// empty components are dropped, so `./a//b` is `["a", "b"]`.
pub fn components(user: &Path) -> Result<Vec<OsString>, SafeFsError> {
    if user.is_absolute() {
        return Err(SafeFsError::Absolute);
    }
    let mut out = Vec::new();
    for component in user.components() {
        match component {
            Component::Normal(name) => out.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SafeFsError::ParentDir);
            }
        }
    }
    Ok(out)
}

/// The lexical normalisation of `base.join(target)`: `.` dropped, `..` consumed against the
/// components before it; `None` when `..` climbs above the root. No filesystem access.
fn normalise_relative(base: &[OsString], target: &Path) -> Option<PathBuf> {
    let mut stack: Vec<OsString> = base.to_vec();
    for component in target.components() {
        match component {
            Component::Normal(name) => stack.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(stack.iter().collect())
}

#[cfg(feature = "testing")]
static RESOLVE_HOOK: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>> =
    std::sync::Mutex::new(None);

#[cfg(feature = "testing")]
static AFTER_OPEN_HOOK: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>> =
    std::sync::Mutex::new(None);

/// Test seam: a closure run by the workspace tools right **after** the fd is obtained and
/// before it is used. The correct implementation keeps using that fd, so whatever the hook
/// does to the path afterwards cannot change what is read or written; an implementation that
/// re-opened by path here would follow it. Only built with the `testing` feature.
#[cfg(feature = "testing")]
pub fn after_open_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *AFTER_OPEN_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = hook;
}

pub(crate) fn run_after_open_hook() {
    #[cfg(feature = "testing")]
    if let Some(hook) = AFTER_OPEN_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        hook();
    }
}

/// Test seam: installs (or clears) a closure that runs after the lexical check of every open
/// and before the first `openat`. Only built with the `testing` feature.
#[cfg(feature = "testing")]
pub fn resolve_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *RESOLVE_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = hook;
}

fn run_resolve_hook() {
    #[cfg(feature = "testing")]
    if let Some(hook) = RESOLVE_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        hook();
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::{CStr, CString, OsStr};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::io::{AsRawFd, FromRawFd};

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path component"))
    }

    fn relative(parts: &[OsString]) -> PathBuf {
        parts.iter().collect()
    }

    /// Opens the workspace root itself as a directory fd. The root is the canonical path the
    /// workspace was constructed with, so following a link **here** is intended (ADR 0039
    /// keeps "a symlinked workspace root works").
    pub fn open_root(root: &Path) -> io::Result<File> {
        let path = cstr(root.as_os_str())?;
        // SAFETY: `path` is a valid NUL-terminated string; the flags are plain integers.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a freshly opened descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// `openat(dirfd, name, flags)` as a `File`.
    fn openat(dir: &File, name: &OsStr, flags: libc::c_int, mode: libc::c_int) -> io::Result<File> {
        let name = cstr(name)?;
        // SAFETY: `dir` is an open directory fd, `name` a valid C string.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a freshly opened descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// `readlinkat(dirfd, name)`: the link's target bytes, or `None` on any error.
    fn readlinkat(dir: &File, name: &OsStr) -> Option<PathBuf> {
        let name = cstr(name).ok()?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: `buf` is a writable buffer of the given length.
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n < 0 || n as usize >= buf.len() {
            return None;
        }
        buf.truncate(n as usize);
        Some(PathBuf::from(OsString::from_vec(buf)))
    }

    /// The error for a component that turned out to be a symlink: its target is read relative
    /// to the same dirfd (never opened) and resolved lexically against the components before
    /// it; an inside target is named, an outside or unreadable one is not.
    fn symlink_error(dir: &File, parent: &[OsString], name: &OsString, root: &Path) -> SafeFsError {
        let mut component = parent.to_vec();
        component.push(name.clone());
        let inside = readlinkat(dir, name).and_then(|target| {
            if target.is_absolute() {
                classify_absolute(&target, root)
            } else {
                normalise_relative(parent, &target)
            }
        });
        SafeFsError::Symlink {
            component: relative(&component),
            inside,
        }
    }

    /// Where an absolute link target lands, as a workspace-relative path when it is inside
    /// the (canonical) root. The target is normalised with `realpath`, which resolves names
    /// and never opens the file; a dangling target is normalised through its parent so a link
    /// to a not-yet-existing workspace file is still named. `None` is "outside or unknown".
    fn classify_absolute(target: &Path, root: &Path) -> Option<PathBuf> {
        let canonical = std::fs::canonicalize(target).ok().or_else(|| {
            let parent = std::fs::canonicalize(target.parent()?).ok()?;
            Some(parent.join(target.file_name()?))
        })?;
        canonical
            .strip_prefix(root)
            .ok()
            .map(Path::to_path_buf)
            .and_then(|rel| normalise_relative(&[], &rel))
    }

    fn map_open_error(
        error: io::Error,
        dir: &File,
        parent: &[OsString],
        name: &OsString,
        root: &Path,
        expect_dir: bool,
    ) -> SafeFsError {
        let mut component = parent.to_vec();
        component.push(name.clone());
        // Linux reports a link hit by `O_NOFOLLOW` as ELOOP; macOS reports one hit by
        // `O_NOFOLLOW | O_DIRECTORY` as ENOTDIR. Ask the entry itself first, with an lstat that
        // never follows it, so the two platforms give the same answer.
        if let Ok(stat) = fstatat(dir, name)
            && stat.kind == EntryKind::Symlink
        {
            return symlink_error(dir, parent, name, root);
        }
        match error.raw_os_error() {
            Some(libc::ELOOP) => symlink_error(dir, parent, name, root),
            Some(libc::ENOTDIR) if expect_dir => SafeFsError::NotADirectory {
                component: relative(&component),
            },
            Some(libc::EISDIR) => SafeFsError::IsADirectory {
                component: relative(&component),
            },
            _ => SafeFsError::Io {
                component: relative(&component),
                source: error,
            },
        }
    }

    /// Walks `parts` from `root_fd`, opening every component as a directory with
    /// `O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC`. Returns the last directory's fd (a fresh
    /// descriptor for the root itself when `parts` is empty).
    pub fn walk_dirs(root_fd: &File, root: &Path, parts: &[OsString]) -> Result<File, SafeFsError> {
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC;
        let mut dir =
            openat(root_fd, OsStr::new("."), flags, 0).map_err(|source| SafeFsError::Io {
                component: PathBuf::from("."),
                source,
            })?;
        for (index, name) in parts.iter().enumerate() {
            dir = match openat(&dir, name, flags, 0) {
                Ok(next) => next,
                Err(error) => {
                    return Err(map_open_error(
                        error,
                        &dir,
                        &parts[..index],
                        name,
                        root,
                        true,
                    ));
                }
            };
        }
        Ok(dir)
    }

    /// Opens the file `parts` names (the last component with `O_NOFOLLOW` and the mode's
    /// flags) after walking its parents as directories.
    pub fn open_file(
        root_fd: &File,
        root: &Path,
        parts: &[OsString],
        mode: OpenMode,
    ) -> Result<File, SafeFsError> {
        let Some((name, parents)) = parts.split_last() else {
            return Err(SafeFsError::IsADirectory {
                component: PathBuf::from("."),
            });
        };
        let dir = walk_dirs(root_fd, root, parents)?;
        let base = libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let flags = match mode {
            OpenMode::Read => libc::O_RDONLY | base,
            OpenMode::ReadWrite => libc::O_RDWR | base,
            OpenMode::WriteCreate => libc::O_WRONLY | libc::O_CREAT | base,
            OpenMode::WriteNew => libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | base,
        };
        openat(&dir, name, flags, 0o644)
            .map_err(|error| map_open_error(error, &dir, parents, name, root, false))
    }

    /// Opens the directory `parts` names (every component, including the last, as a
    /// directory with `O_NOFOLLOW`).
    pub fn open_dir(root_fd: &File, root: &Path, parts: &[OsString]) -> Result<File, SafeFsError> {
        walk_dirs(root_fd, root, parts)
    }

    fn kind_of(mode: libc::mode_t) -> EntryKind {
        match mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Dir,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }

    /// `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)`: the entry itself, never a link's target.
    pub fn fstatat(dir: &File, name: &OsStr) -> io::Result<Stat> {
        let name = cstr(name)?;
        // SAFETY: zeroed stat buffer, valid fd and C string.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Stat {
            kind: kind_of(st.st_mode),
            size: st.st_size as u64,
            nlink: st.st_nlink as u64,
        })
    }

    /// `fstat` of an open file.
    pub fn fstat(file: &File) -> io::Result<Stat> {
        let metadata = file.metadata()?;
        use std::os::unix::fs::MetadataExt;
        let kind = if metadata.file_type().is_symlink() {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Dir
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        Ok(Stat {
            kind,
            size: metadata.len(),
            nlink: metadata.nlink(),
        })
    }

    /// Lists a directory through its fd. Entries are sorted by name; `.` and `..` are
    /// dropped; each entry's kind comes from `fstatat(AT_SYMLINK_NOFOLLOW)` (an entry that
    /// vanishes between `readdir` and `fstatat` is skipped).
    pub fn read_dir(dir: &File) -> io::Result<Vec<DirEntry>> {
        // `closedir` closes the fd it was given, so hand it a duplicate and keep ours.
        // SAFETY: `dir` is an open fd.
        let dup = unsafe { libc::dup(dir.as_raw_fd()) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // The duplicate shares the file offset; rewind so a second listing starts at the top.
        // SAFETY: `dup` is a valid fd we own.
        let stream = unsafe { libc::fdopendir(dup) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `dup` was not consumed by a failed `fdopendir`.
            unsafe { libc::close(dup) };
            return Err(error);
        }
        // SAFETY: `stream` is a valid DIR*.
        unsafe { libc::rewinddir(stream) };
        let mut names: Vec<OsString> = Vec::new();
        loop {
            // SAFETY: `stream` is a valid DIR*; a null return ends the listing.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: `entry` points at a valid dirent whose d_name is NUL-terminated.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        // SAFETY: closes the DIR* and the duplicate fd it owns.
        unsafe { libc::closedir(stream) };
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let Ok(stat) = fstatat(dir, &name) else {
                continue;
            };
            out.push(DirEntry {
                name,
                kind: stat.kind,
                size: stat.size,
            });
        }
        Ok(out)
    }

    /// Truncates an open file to zero and rewinds it, for the in-place overwrite the tools do
    /// after their checks.
    pub fn truncate(file: &File) -> io::Result<()> {
        file.set_len(0)?;
        use std::io::{Seek, SeekFrom};
        let mut handle = file;
        handle.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    /// Reads a whole open file into a `String`, checking UTF-8 like `read_to_string`.
    pub fn read_to_string(file: &File) -> io::Result<String> {
        use std::io::Read;
        let mut text = String::new();
        let mut handle = file;
        handle.read_to_string(&mut text)?;
        Ok(text)
    }
}

#[cfg(not(unix))]
mod imp {
    //! The non-Unix fallback: std path-based opens with no symlink defence. The coding tools
    //! run on Unix hosts; this keeps the crate compiling elsewhere.
    use super::*;

    fn join(root: &Path, parts: &[OsString]) -> PathBuf {
        let mut path = root.to_path_buf();
        for part in parts {
            path.push(part);
        }
        path
    }

    pub fn open_root(root: &Path) -> io::Result<File> {
        File::open(root)
    }

    fn io(component: &[OsString], source: io::Error) -> SafeFsError {
        SafeFsError::Io {
            component: component.iter().collect(),
            source,
        }
    }

    pub fn walk_dirs(
        _root_fd: &File,
        root: &Path,
        parts: &[OsString],
    ) -> Result<File, SafeFsError> {
        File::open(join(root, parts)).map_err(|e| io(parts, e))
    }

    pub fn open_file(
        _root_fd: &File,
        root: &Path,
        parts: &[OsString],
        mode: OpenMode,
    ) -> Result<File, SafeFsError> {
        let path = join(root, parts);
        let mut options = std::fs::OpenOptions::new();
        match mode {
            OpenMode::Read => options.read(true),
            OpenMode::ReadWrite => options.read(true).write(true),
            OpenMode::WriteCreate => options.write(true).create(true),
            OpenMode::WriteNew => options.write(true).create_new(true),
        };
        options.open(&path).map_err(|e| io(parts, e))
    }

    pub fn open_dir(_root_fd: &File, root: &Path, parts: &[OsString]) -> Result<File, SafeFsError> {
        File::open(join(root, parts)).map_err(|e| io(parts, e))
    }

    pub fn fstatat(_dir: &File, _name: &std::ffi::OsStr) -> io::Result<Stat> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fstatat is Unix-only",
        ))
    }

    pub fn fstat(file: &File) -> io::Result<Stat> {
        let metadata = file.metadata()?;
        let kind = if metadata.is_dir() {
            EntryKind::Dir
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        Ok(Stat {
            kind,
            size: metadata.len(),
            nlink: 1,
        })
    }

    pub fn read_dir(_dir: &File) -> io::Result<Vec<DirEntry>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fd listing is Unix-only",
        ))
    }

    pub fn truncate(file: &File) -> io::Result<()> {
        file.set_len(0)
    }

    pub fn read_to_string(file: &File) -> io::Result<String> {
        use std::io::Read;
        let mut text = String::new();
        let mut handle = file;
        handle.read_to_string(&mut text)?;
        Ok(text)
    }
}

pub use imp::{
    fstat, fstatat, open_dir, open_file, open_root, read_dir, read_to_string, truncate, walk_dirs,
};

/// Reads a file by path with `O_NOFOLLOW` on the **leaf** only. This is the weaker guard for
/// host-side reads that are not workspace-relative (context files up to the repository root,
/// skill bodies under `~/.radian`): a symlink at the named path is refused (`ELOOP`, reported
/// as `is a symlink`), a symlinked parent is not detected. Workspace-relative reads use
/// [`RootDir`] instead.
pub fn read_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a symlink; it is not followed", path.display()),
            );
        }
        error
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A workspace root held as a directory fd plus its canonical path (the path is only used to
/// classify a symlink's absolute target and for messages, never to open anything).
#[derive(Debug)]
pub struct RootDir {
    fd: File,
    path: PathBuf,
}

impl RootDir {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            fd: open_root(path)?,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens `user` (workspace-relative) as a file.
    pub fn file(&self, user: &Path, mode: OpenMode) -> Result<File, SafeFsError> {
        let parts = components(user)?;
        run_resolve_hook();
        open_file(&self.fd, &self.path, &parts, mode)
    }

    /// Opens `user` (workspace-relative; `.` or empty is the root) as a directory.
    pub fn dir(&self, user: &Path) -> Result<File, SafeFsError> {
        let parts = components(user)?;
        run_resolve_hook();
        open_dir(&self.fd, &self.path, &parts)
    }

    /// Opens the directory that holds `user`'s last component and returns it with that name,
    /// for callers that `fstatat` or create the leaf themselves.
    pub fn parent_of(&self, user: &Path) -> Result<(File, OsString), SafeFsError> {
        let parts = components(user)?;
        let Some((name, parents)) = parts.split_last() else {
            return Err(SafeFsError::IsADirectory {
                component: PathBuf::from("."),
            });
        };
        run_resolve_hook();
        let dir = open_dir(&self.fd, &self.path, parents)?;
        Ok((dir, name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_refuse_absolute_and_parent_and_drop_dots() {
        assert!(matches!(
            components(Path::new("/etc")),
            Err(SafeFsError::Absolute)
        ));
        assert!(matches!(
            components(Path::new("a/../b")),
            Err(SafeFsError::ParentDir)
        ));
        let parts = components(Path::new("./a//b/./c")).unwrap();
        assert_eq!(
            parts,
            vec![
                OsString::from("a"),
                OsString::from("b"),
                OsString::from("c")
            ]
        );
        assert!(components(Path::new("")).unwrap().is_empty());
        assert!(components(Path::new(".")).unwrap().is_empty());
    }

    #[test]
    fn a_relative_link_target_is_resolved_lexically_against_its_parent() {
        let base = [OsString::from("docs"), OsString::from("current")];
        // `docs/current -> ../v2/x` resolves from `docs` (the link's parent): `docs/v2/x`.
        let parent = &base[..1];
        assert_eq!(
            normalise_relative(parent, Path::new("../v2/x")),
            Some(PathBuf::from("v2/x"))
        );
        assert_eq!(
            normalise_relative(parent, Path::new("v2")),
            Some(PathBuf::from("docs/v2"))
        );
        // Climbing above the root is "outside".
        assert_eq!(normalise_relative(parent, Path::new("../../etc")), None);
        assert_eq!(normalise_relative(parent, Path::new("/abs")), None);
    }
}

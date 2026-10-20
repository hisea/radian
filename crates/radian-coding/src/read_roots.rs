//! The read-only roots outside the workspace that `read`, `grep` and `glob` may also
//! open (ADR 0039): the cargo registry sources, `$CARGO_HOME/registry/src`, and the
//! sources of every installed toolchain, `$RUSTUP_HOME/toolchains/*/lib/rustlib/src`.
//!
//! A model names one of them as an absolute path or with `~`. A path that falls under a root is
//! opened from that root's own directory fd with the same per-component `O_NOFOLLOW` walk the
//! workspace uses ([`RootDir`]): no symlink is followed at any level, and a link that leaves the
//! root is refused exactly like a link that leaves the workspace. `write`, `patch` and `shell`'s
//! working directory never accept these roots (they resolve a path against the workspace only).
//!
//! The roots are resolved **once**, when the tool set is built: each root itself is
//! canonicalized (a symlinked `~/.cargo` is fine — its parent is not writable by the model) and
//! held as a directory fd; a root that is missing, is not a directory, or cannot be opened is
//! skipped, so the file tools simply do not read it.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::safe_fs::{OpenMode, RootDir, SafeFsError};

/// One readable root: the directory fd, its canonical path, and the absolute spellings a model
/// path may use to name it.
#[derive(Debug)]
pub(crate) struct ReadRoot {
    dir: RootDir,
    /// The canonical path (what the fd points at); used in "leaving the root" messages.
    canonical: PathBuf,
    /// The absolute spellings that select this root, lexically normalised: the canonical path
    /// and, when it differs, the configured one. Keeping the configured spelling means a path
    /// written the way the user set `CARGO_HOME`/`RUSTUP_HOME` still matches when
    /// canonicalization rewrites it (e.g. `/tmp` → `/private/tmp` on macOS).
    prefixes: Vec<PathBuf>,
}

/// A path that resolved under one of the [`ReadRoots`].
pub(crate) struct FoundReadPath {
    /// Index into [`ReadRoots::roots`].
    pub(crate) index: usize,
    /// The path relative to the root.
    pub(crate) relative: PathBuf,
    /// The spelling of the root the model used (the prefix that matched); joining it with
    /// `relative` gives a path `read` accepts again.
    pub(crate) prefix: PathBuf,
}

/// The read-only roots, resolved once when the tool set is built.
#[derive(Debug, Default)]
pub struct ReadRoots {
    roots: Vec<ReadRoot>,
}

/// The two readable roots as one phrase, for the boundary refusal message (ADR 0039).
pub(crate) const READ_ROOTS_DESCRIPTION: &str = "the cargo registry sources (`$CARGO_HOME/registry/src`) and every toolchain's \
     `lib/rustlib/src` (`$RUSTUP_HOME/toolchains/*/lib/rustlib/src`), named as an absolute or \
     `~` path (read-only)";

impl ReadRoots {
    /// No extra roots: the file tools are workspace-only (the default for an embedded tool set
    /// and for tests that do not exercise the registry/toolchain roots).
    pub fn none() -> Self {
        Self { roots: Vec::new() }
    }

    /// Resolves `$CARGO_HOME/registry/src` and every `$RUSTUP_HOME/toolchains/*/lib/rustlib/src`
    /// from the process environment (`~/.cargo` and `~/.rustup` when the variables are unset or
    /// relative). A root that does not exist, is not a directory, or cannot be opened is
    /// skipped.
    pub fn from_env() -> Self {
        let mut roots = Vec::new();
        if let Some(cargo) = cargo_registry_src()
            && let Some(root) = ReadRoot::at(&cargo)
        {
            roots.push(root);
        }
        for toolchain in rustup_toolchain_srcs() {
            if let Some(root) = ReadRoot::at(&toolchain) {
                roots.push(root);
            }
        }
        Self { roots }
    }

    pub(crate) fn roots(&self) -> &[ReadRoot] {
        &self.roots
    }

    /// The root `path` (already absolute and lexically normalised) falls under, the longest
    /// matching prefix winning. `None` means it is not under any readable root.
    pub(crate) fn lookup(&self, path: &Path) -> Option<FoundReadPath> {
        let mut best: Option<(usize, FoundReadPath)> = None;
        for (index, root) in self.roots.iter().enumerate() {
            for prefix in &root.prefixes {
                if let Ok(relative) = path.strip_prefix(prefix) {
                    let width = prefix.components().count();
                    if best
                        .as_ref()
                        .is_none_or(|(best_width, _)| width > *best_width)
                    {
                        best = Some((
                            width,
                            FoundReadPath {
                                index,
                                relative: relative.to_path_buf(),
                                prefix: prefix.clone(),
                            },
                        ));
                    }
                }
            }
        }
        best.map(|(_, found)| found)
    }
}

impl ReadRoot {
    /// Opens `configured` as a readable root, or `None` when it is missing or not a directory.
    /// The configured spelling is canonicalized once here; the root's parent is not writable by
    /// the model, so following a link at the root itself is intended.
    fn at(configured: &Path) -> Option<Self> {
        let canonical = std::fs::canonicalize(configured).ok()?;
        if !canonical.is_dir() {
            return None;
        }
        let dir = RootDir::open(&canonical).ok()?;
        let mut prefixes = vec![canonical.clone()];
        if let Some(configured) = normalise_absolute(configured)
            && configured != canonical
        {
            prefixes.push(configured);
        }
        Some(Self {
            dir,
            canonical,
            prefixes,
        })
    }

    /// Opens `relative` (already relative to this root) as a file.
    pub(crate) fn file(
        &self,
        relative: &Path,
        mode: OpenMode,
    ) -> Result<std::fs::File, SafeFsError> {
        self.dir.file(relative, mode)
    }

    /// Opens `relative` (already relative to this root) as a directory.
    pub(crate) fn dir(&self, relative: &Path) -> Result<std::fs::File, SafeFsError> {
        self.dir.dir(relative)
    }

    /// The canonical path of the root (never the outside target of a link).
    pub(crate) fn canonical(&self) -> &Path {
        &self.canonical
    }
}

/// `$CARGO_HOME/registry/src` (default `~/.cargo/registry/src`).
fn cargo_registry_src() -> Option<PathBuf> {
    let cargo_home =
        env_absolute("CARGO_HOME").or_else(|| user_home().map(|home| home.join(".cargo")))?;
    Some(cargo_home.join("registry").join("src"))
}

/// `lib/rustlib/src` under every toolchain directory (`$RUSTUP_HOME/toolchains/*`, default
/// `~/.rustup`). Sorted, so the roots' order is stable.
fn rustup_toolchain_srcs() -> Vec<PathBuf> {
    let Some(rustup_home) =
        env_absolute("RUSTUP_HOME").or_else(|| user_home().map(|home| home.join(".rustup")))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(rustup_home.join("toolchains")) else {
        return Vec::new();
    };
    let mut srcs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("lib").join("rustlib").join("src"))
        .collect();
    srcs.sort();
    srcs
}

/// `HOME`, when it is set to an absolute path. `None` when it is unset, empty or relative, so a
/// `~` is never guessed.
pub(crate) fn user_home() -> Option<PathBuf> {
    env_absolute("HOME")
}

fn env_absolute(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Lexically normalises an absolute path: `.` is dropped and `..` is consumed against the
/// component before it. `None` when the path is not absolute or a `..` climbs above the root.
/// No filesystem access, so a link is never followed by this step.
pub(crate) fn normalise_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut stack: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => stack.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop()?;
            }
            Component::Normal(name) => stack.push(name.to_os_string()),
            Component::Prefix(_) => return None,
        }
    }
    let mut out = PathBuf::from("/");
    for part in stack {
        out.push(part);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_absolute_drops_dots_and_refuses_climbing_above_the_root() {
        assert_eq!(
            normalise_absolute(Path::new("/a/./b/../c")),
            Some(PathBuf::from("/a/c"))
        );
        assert_eq!(normalise_absolute(Path::new("/..")), None);
        assert_eq!(normalise_absolute(Path::new("relative")), None);
    }
}

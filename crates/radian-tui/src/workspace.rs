//! The workspace a TUI session runs in.
//!
//! The TUI used to mount its file tools only when `--workspace` was given; without it a session
//! had no `read`/`write`/`shell` at all. Now the current directory is the default, with two
//! exceptions that keep a mistake from being expensive:
//!
//! - **`$HOME` itself** becomes `$HOME/tmp` (created `0700`), so a session started from the home
//!   directory never makes the whole home directory the coding root and the sandbox's write
//!   boundary;
//! - the **filesystem root** is refused outright.
//!
//! An explicit `--workspace` is used exactly as given (including `~`), and `agent-cli` keeps the
//! old behaviour on purpose: it is the unattended entry point and should not gain write access
//! by default.

use std::path::{Path, PathBuf};

/// The resolved coding root and, when the default was adjusted, the note for the startup line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingRoot {
    pub path: PathBuf,
    /// `Some("cwd is $HOME")` when `$HOME` was replaced by `$HOME/tmp`.
    pub note: Option<String>,
}

/// Resolves the coding root. `explicit` is `--workspace`; `cwd` is the process's current
/// directory (already canonical, or at least absolute); `home` is `$HOME` when it is usable.
pub fn resolve_coding_root(
    explicit: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<CodingRoot, String> {
    if let Some(path) = explicit {
        return Ok(CodingRoot {
            path: path.to_path_buf(),
            note: None,
        });
    }
    // Both sides are canonical: `getcwd` may hand back a symlinked path (macOS's `/var` →
    // `/private/var`), and the recorded workspace must be canonical anyway.
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let cwd = cwd.as_path();
    if cwd == Path::new("/") {
        return Err(
            "the filesystem root is not a workspace: run in a project directory, or pass \
             --workspace"
                .to_string(),
        );
    }
    // `current_dir()` is the **canonical** path on macOS (`/var` is a symlink to
    // `/private/var`), so `$HOME` must be canonical too or the comparison silently misses and
    // the whole home directory becomes the coding root.
    let home = home.and_then(|home| std::fs::canonicalize(home).ok());
    let Some(home) = home.filter(|home| cwd == home.as_path()) else {
        return Ok(CodingRoot {
            path: cwd.to_path_buf(),
            note: None,
        });
    };
    let dir = home.join("tmp");
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "{} exists and is not a directory; pass --workspace",
                dir.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            radian_session::ensure_private_dir(&dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        }
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    }
    Ok(CodingRoot {
        path: dir,
        note: Some("cwd is $HOME".to_string()),
    })
    // (`home` is the canonical one from above, so `dir` is canonical too.)
}

/// `~/tmp` for the substituted directory, the full path otherwise: the startup line stays short
/// and the note says why.
pub fn display_coding_root(root: &CodingRoot, home: Option<&Path>) -> String {
    // Canonical, like the path `resolve_coding_root` returned.
    let home = home.and_then(|home| std::fs::canonicalize(home).ok());
    match (root.note.as_deref(), home) {
        (Some(_), Some(home)) if root.path == home.join("tmp") => "~/tmp".to_string(),
        _ => root.path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "radian-workspace-{}-{label}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_explicit_workspace_is_used_as_given() {
        let root = resolve_coding_root(Some(Path::new("/work/x")), Path::new("/"), None).unwrap();
        assert_eq!(root.path, PathBuf::from("/work/x"));
        assert_eq!(root.note, None, "no substitution, no note");
    }

    #[test]
    fn the_current_directory_is_the_default() {
        let root = resolve_coding_root(None, Path::new("/work/project"), None).unwrap();
        assert_eq!(root.path, PathBuf::from("/work/project"));
        assert_eq!(root.note, None);
    }

    #[test]
    fn the_filesystem_root_is_refused() {
        let error = resolve_coding_root(None, Path::new("/"), None).unwrap_err();
        assert!(error.contains("filesystem root"), "{error}");
        assert!(error.contains("--workspace"), "{error}");
    }

    #[test]
    fn home_becomes_home_tmp_and_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        // Canonical, like the process's `current_dir()`.
        let home = std::fs::canonicalize(scratch("home")).unwrap();
        let root = resolve_coding_root(None, &home, Some(&home)).unwrap();
        assert_eq!(root.path, home.join("tmp"));
        assert_eq!(root.note.as_deref(), Some("cwd is $HOME"));
        assert!(root.path.is_dir(), "it is created");
        let mode = std::fs::metadata(&root.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "owner only");
        assert_eq!(display_coding_root(&root, Some(&home)), "~/tmp");

        // An existing directory is kept, and a file in its place is refused.
        let again = resolve_coding_root(None, &home, Some(&home)).unwrap();
        assert_eq!(again.path, home.join("tmp"));
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("tmp"), "not a directory").unwrap();
        let error = resolve_coding_root(None, &home, Some(&home)).unwrap_err();
        assert!(error.contains("not a directory"), "{error}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `$HOME` is compared as a path, not by name: a different directory of the same name is not
    /// substituted.
    /// A home directory reached through a symlink still matches: the comparison is on canonical
    /// paths (macOS's `/var` → `/private/var` is exactly this case).
    #[test]
    fn a_symlinked_home_still_matches() {
        let real = std::fs::canonicalize(scratch("real-home")).unwrap();
        let link = real.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let root = resolve_coding_root(None, &real, Some(&link)).unwrap();
        assert_eq!(root.path, real.join("tmp"));
        assert_eq!(root.note.as_deref(), Some("cwd is $HOME"));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn another_directory_is_not_substituted_for_home() {
        let root = resolve_coding_root(None, Path::new("/work/home"), Some(Path::new("/Users/x")))
            .unwrap();
        assert_eq!(root.path, PathBuf::from("/work/home"));
        assert_eq!(root.note, None);
    }
}

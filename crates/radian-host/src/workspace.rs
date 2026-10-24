//! Resolving and validating the `--workspace` argument, shared by `radian-exec` and
//! `radian-tui` so the two hosts cannot disagree about what counts as a usable workspace.
//!
//! The point of doing this **before** the sandbox probe and before any session file exists
//! (dogfooding D1/D2): a mistyped path must fail as a workspace problem, not as a sandbox
//! policy choice, and a start that fails must not leave a session behind. The messages here
//! therefore never mention the sandbox or a `full-access` fallback.

use std::path::{Path, PathBuf};

/// Resolves `workspace` (or the current directory when it is `None`) to a canonical
/// directory, or returns an error that names the path and the reason.
///
/// Checked in the order a user would expect: the path must exist, be a directory, be
/// readable, and canonicalize. A missing path, a regular file, or an unreadable directory
/// each get their own message. Whatever this returns is the value the permission layer and
/// the sandbox are built for, so the probe cannot be reached with a workspace that cannot
/// pass this.
pub fn resolve_workspace(workspace: Option<&Path>) -> Result<PathBuf, String> {
    let path = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?,
    };
    let metadata =
        std::fs::metadata(&path).map_err(|e| format!("workspace {}: {e}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("workspace {}: not a directory", path.display()));
    }
    // `metadata` succeeds for a directory with no read permission; opening it is what a host
    // that mounts tools there actually needs, so test it now rather than after the probe.
    std::fs::read_dir(&path).map_err(|e| format!("workspace {}: {e}", path.display()))?;
    std::fs::canonicalize(&path).map_err(|e| format!("workspace {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::with_label("radian-host-workspace", label)
    }

    #[test]
    fn a_missing_workspace_names_the_path_and_not_the_sandbox() {
        let dir = temp_dir("missing");
        let missing = dir.join("does-not-exist");
        let error = resolve_workspace(Some(&missing)).unwrap_err();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(!error.contains("sandbox"), "{error}");
        assert!(!error.contains("--profile"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_is_not_a_workspace() {
        let dir = temp_dir("file");
        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        let error = resolve_workspace(Some(&file)).unwrap_err();
        assert!(error.contains("not a directory"), "{error}");
        assert!(!error.contains("--profile"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_is_canonicalized() {
        let dir = temp_dir("canon");
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let path = resolve_workspace(Some(&dir.join("sub").join("..").join("sub"))).unwrap();
        assert_eq!(path, std::fs::canonicalize(&nested).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_current_directory_is_used_when_no_workspace_is_given() {
        let path = resolve_workspace(None).unwrap();
        assert!(path.is_dir());
    }
}

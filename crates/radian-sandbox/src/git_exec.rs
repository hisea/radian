//! The `.git` entries git can execute (ADR 0038 decision 3), as one shared list.
//!
//! Two layers enforce the same list: the sandbox backends (macOS `seatbelt`'s
//! `git_exec_denies`, which turns it into SBPL) and the permission layer (the file tools
//! never enter the sandbox, so `workspace_write()` denies these paths with rules built from
//! the same constants and [`is_git_exec_entry`]). The list lives here so the two cannot
//! drift apart: a new executable entry is added here, and both sides pick it up.

use std::ffi::OsStr;
use std::path::Path;

/// Files directly under a git directory that git executes.
pub const GIT_EXEC_FILES: &[&str] = &["config", "config.worktree"];

/// Directories directly under a git directory that git executes (the whole subtree).
pub const GIT_EXEC_DIRS: &[&str] = &["hooks"];

/// Files under `modules/<name>/` (one or more segments, nested submodules included) that git
/// executes. This is deliberately narrower than [`GIT_EXEC_FILES`]: a submodule's
/// `config.worktree` is not an executable entry the sandbox denies (ADR 0038 decision 3
/// names only `config` and `hooks/` for submodules).
pub const GIT_MODULE_FILES: &[&str] = &["config"];

/// Directories under `modules/<name>/` (one or more segments) that git executes.
pub const GIT_MODULE_DIRS: &[&str] = &["hooks"];

/// Files inside a linked worktree's `worktrees/<name>/` directory (optionally under
/// `modules/<name>/...`) that git executes: a repointed link, or a `core.hooksPath` in the
/// per-worktree config.
pub const GIT_WORKTREE_FILES: &[&str] = &["gitdir", "commondir", "config.worktree"];

/// Whether `candidate` is an entry git can execute under the git directory `git_dir` (ADR
/// 0038 decision 3). Both paths are expected resolved (canonical); the check is by path
/// components, so `config-other` never matches `config`, and entries that do not exist yet
/// (a hook the model would create) still match.
pub fn is_git_exec_entry(git_dir: &Path, candidate: &Path) -> bool {
    let Ok(relative) = candidate.strip_prefix(git_dir) else {
        return false;
    };
    let parts: Vec<&OsStr> = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    is_protected_relative(&parts)
}

fn contains(names: &[&str], part: &OsStr) -> bool {
    names.iter().any(|name| OsStr::new(name) == part)
}

fn is_protected_relative(parts: &[&OsStr]) -> bool {
    match parts {
        // `config`, `config.worktree` directly under the git directory.
        [file] if contains(GIT_EXEC_FILES, file) => true,
        // `hooks/...`: the directory itself and everything below it.
        [dir, ..] if contains(GIT_EXEC_DIRS, dir) => true,
        // `worktrees/<name>/<gitdir|commondir|config.worktree>`.
        [w, _, file] if *w == OsStr::new("worktrees") && contains(GIT_WORKTREE_FILES, file) => true,
        // `modules/<name>/...`: strip one segment and judge the rest, so nested
        // submodules (`modules/a/modules/b/config`) are covered without re-discovery.
        [m, _, rest @ ..] if *m == OsStr::new("modules") && !rest.is_empty() => {
            is_protected_module_relative(rest)
        }
        _ => false,
    }
}

/// The entries under `modules/<name>/...`: the submodule's own `config` and `hooks/...`,
/// or a linked worktree of this (sub)module.
fn is_protected_module_relative(parts: &[&OsStr]) -> bool {
    match parts {
        [file] if contains(GIT_MODULE_FILES, file) => true,
        [dir, ..] if contains(GIT_MODULE_DIRS, dir) => true,
        [w, _, file] if *w == OsStr::new("worktrees") && contains(GIT_WORKTREE_FILES, file) => true,
        [m, _, rest @ ..] if *m == OsStr::new("modules") && !rest.is_empty() => {
            is_protected_module_relative(rest)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(git: &str, rel: &str) -> bool {
        is_git_exec_entry(Path::new(git), &PathBuf::from(git).join(rel))
    }

    /// The matrix the seatbelt profile and the permission rules both enforce: every
    /// executable entry matches, nothing else does.
    #[test]
    fn the_shared_list_covers_exactly_the_executable_entries() {
        let git = "/tmp/ws/.git";
        // Top level.
        for rel in [
            "config",
            "config.worktree",
            "hooks",
            "hooks/pre-commit",
            "hooks/nested/dir",
        ] {
            assert!(entry(git, rel), "{rel}");
        }
        // Submodules, nested included.
        for rel in [
            "modules/app/config",
            "modules/app/hooks",
            "modules/app/hooks/post-checkout",
            "modules/a/modules/b/config",
            "modules/a/modules/b/hooks/x",
        ] {
            assert!(entry(git, rel), "{rel}");
        }
        // Linked worktrees, including a submodule's.
        for rel in [
            "worktrees/w1/gitdir",
            "worktrees/w1/commondir",
            "worktrees/w1/config.worktree",
            "modules/app/worktrees/w2/gitdir",
        ] {
            assert!(entry(git, rel), "{rel}");
        }
        // Everything else stays writable: the object store, refs, logs, `info/`, and names
        // that merely start like an executable entry.
        for rel in [
            "info/exclude",
            "objects/ab/cd",
            "refs/heads/main",
            "logs/HEAD",
            "index",
            "HEAD",
            "config-other",
            "hooks-other/x",
            "modules",
            "modules/app",
            "modules/app/config-other",
            "modules/app/config.worktree",
            "worktrees",
            "worktrees/w1",
            "worktrees/w1/HEAD",
        ] {
            assert!(!entry(git, rel), "{rel}");
        }
        // Outside the git directory never matches.
        assert!(!is_git_exec_entry(
            Path::new(git),
            Path::new("/tmp/ws/src/main.rs"),
        ));
    }
}

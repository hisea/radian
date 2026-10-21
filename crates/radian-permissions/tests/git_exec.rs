//! ADR 0038 decision 3 for the file tools: in the `workspace` tier, `write`/`patch`
//! deny the `.git` entries git can execute, exactly like the sandbox does.
//!
//! `write`/`patch` are host tools and never enter the sandbox, so the sandbox's
//! `WritableExceptExec` carve-out alone does not stop the model from writing
//! `.git/hooks/pre-commit` and having it run at the next `git commit`. These tests pin the
//! permission-layer half: the same shared list denies the write before it reaches the tool.

mod common;

use common::{TempDir, call};
use radian_permissions::*;
use serde_json::json;

fn decide(profile: &Profile, name: &str, path: &str) -> Evaluated {
    evaluate(
        profile.rules(),
        &profile.workspace,
        &call(name, json!({"path": path})),
    )
}

/// A workspace shaped like a git repository: a top-level git dir with hooks, a submodule
/// entry and the writable `info/` area.
fn repo(dir: &TempDir) -> Profile {
    let git = dir.path().join(".git");
    std::fs::create_dir_all(git.join("hooks")).unwrap();
    std::fs::create_dir_all(git.join("info")).unwrap();
    std::fs::create_dir_all(git.join("modules").join("app").join("hooks")).unwrap();
    std::fs::write(git.join("config"), "[core]\n").unwrap();
    std::fs::write(git.join("config.worktree"), "[core]\n").unwrap();
    std::fs::write(git.join("modules").join("app").join("config"), "[core]\n").unwrap();
    Profile::workspace_write(dir.path()).unwrap()
}

/// ADR 0038: `write .git/hooks/pre-commit` is denied in the `workspace` tier, with a reason
/// that names the ADR and says the entry can run code.
#[test]
fn workspace_write_denies_write_to_git_hooks() {
    let dir = TempDir::new("git-hooks");
    let profile = repo(&dir);
    let out = decide(&profile, "write", ".git/hooks/pre-commit");
    assert_eq!(out.action, Action::Deny);
    let reason = out.reason.expect("a deny names why");
    assert!(reason.contains("0038"), "reason names ADR 0038: {reason}");
}

/// ADR 0038: `patch .git/config` is denied in the `workspace` tier.
#[test]
fn workspace_write_denies_patch_to_git_config() {
    let dir = TempDir::new("git-config");
    let profile = repo(&dir);
    let out = decide(&profile, "patch", ".git/config");
    assert_eq!(out.action, Action::Deny);
    assert!(out.reason.is_some_and(|r| r.contains("0038")));
}

/// ADR 0038: the per-worktree config is an executable entry too.
#[test]
fn workspace_write_denies_git_config_worktree() {
    let dir = TempDir::new("git-config-worktree");
    let profile = repo(&dir);
    let out = decide(&profile, "write", ".git/config.worktree");
    assert_eq!(out.action, Action::Deny);
}

/// ADR 0038: a submodule's `config` and `hooks/` are denied like the top level's.
#[test]
fn workspace_write_denies_submodule_config_and_hooks() {
    let dir = TempDir::new("git-submodule");
    let profile = repo(&dir);
    for path in [
        ".git/modules/app/config",
        ".git/modules/app/hooks/post-checkout",
        ".git/modules/app/hooks",
    ] {
        let out = decide(&profile, "write", path);
        assert_eq!(out.action, Action::Deny, "{path}");
    }
}

/// ADR 0038: the rest of `.git` stays writable, so native git keeps working.
#[test]
fn workspace_write_still_allows_git_info_exclude() {
    let dir = TempDir::new("git-info");
    let profile = repo(&dir);
    let out = decide(&profile, "write", ".git/info/exclude");
    assert_eq!(out.action, Action::Allow, "{out:?}");
}

/// A file deny is exact: `.git/config` denied must not spill onto `.git/config-other`.
#[test]
fn a_config_deny_does_not_spill_onto_config_other() {
    let dir = TempDir::new("git-config-exact");
    let profile = repo(&dir);
    let out = decide(&profile, "write", ".git/config-other");
    assert_eq!(out.action, Action::Allow, "{out:?}");
}

/// `full` is unchanged by design: it allows everything, including the exec entries.
#[test]
fn full_access_still_allows_git_hooks_writes() {
    let dir = TempDir::new("git-full");
    let _ = repo(&dir);
    let profile = Profile::full_access(dir.path()).unwrap();
    let out = decide(&profile, "write", ".git/hooks/pre-commit");
    assert_eq!(out.action, Action::Allow, "{out:?}");
}

/// A linked worktree's `.git` is a pointer file, not a directory. Rewriting the pointer
/// would repoint git at a gitdir the model controls (whose hooks then run outside the
/// sandbox), so writing the pointer itself is denied — and a path through it to a gitdir
/// outside the workspace is never allowed either (ADR 0039: the file tools open fd-relative
/// from the workspace root and cannot write outside it).
#[test]
fn workspace_write_denies_the_worktree_pointer_and_anything_outside() {
    let dir = TempDir::new("git-pointer");
    let outside = TempDir::new("git-pointer-outside");
    let outside_git = outside.path().join("evil.git");
    std::fs::create_dir_all(&outside_git).unwrap();
    std::fs::write(
        dir.path().join(".git"),
        format!("gitdir: {}\n", outside_git.display()),
    )
    .unwrap();
    let profile = Profile::workspace_write(dir.path()).unwrap();
    // Rewriting the pointer is denied.
    let out = decide(&profile, "write", ".git");
    assert_eq!(out.action, Action::Deny, "{out:?}");
    // The outside gitdir the pointer names is not writable through the file tools.
    let out = decide(
        &profile,
        "write",
        outside_git.join("config").to_string_lossy().as_ref(),
    );
    assert_ne!(out.action, Action::Allow, "{out:?}");
}

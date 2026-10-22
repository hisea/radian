mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use common::TestRepo;
use radian_arc::gate::{GateCheckout, GateError, Stage};

#[test]
fn prepare_removes_a_previous_sha_and_keeps_target() {
    let repo = TestRepo::new("checkout-sha");
    let base = repo.rev("main");
    let first = repo.commit_on("work/first", &base, "first.txt", "first\n");
    let second = repo.commit_on("work/second", &base, "second.txt", "second\n");

    let checkout = GateCheckout::new(&repo.repo, Stage::Commands, "origin");
    checkout.prepare(&first).unwrap();
    let path = checkout.path();
    assert!(path.join("first.txt").exists());
    assert!(!path.join("second.txt").exists());

    // A previous run leaves an untracked file, an ignored file and a build artifact.
    fs::write(path.join("leftover.txt"), "leftover\n").unwrap();
    fs::write(path.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(path.join("ignored.txt"), "ignored\n").unwrap();
    fs::create_dir_all(path.join("target/debug")).unwrap();
    fs::write(path.join("target/debug/artifact"), "built\n").unwrap();

    checkout.prepare(&second).unwrap();
    assert!(!path.join("first.txt").exists());
    assert!(path.join("second.txt").exists());
    assert!(
        !path.join("leftover.txt").exists(),
        "untracked file survived"
    );
    assert!(!path.join("ignored.txt").exists(), "ignored file survived");
    assert!(
        path.join("target/debug/artifact").exists(),
        "the stage's target/ must survive a prepare"
    );
}

#[test]
fn a_core_hooks_path_hook_never_runs() {
    let repo = TestRepo::new("checkout-hooks");
    let base = repo.rev("main");
    let head = repo.commit_on("work/hooks", &base, "hooked.txt", "hooked\n");

    let hooks = repo.repo.join("hook-dir");
    fs::create_dir_all(&hooks).unwrap();
    let marker = repo.repo.join("hook-ran");
    fs::write(
        hooks.join("post-checkout"),
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            hooks.join("post-checkout"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    repo.git(&["config", "core.hooksPath", common::path(&hooks)]);

    GateCheckout::new(&repo.repo, Stage::Commands, "origin")
        .prepare(&head)
        .unwrap();
    assert!(!marker.exists(), "core.hooksPath hook ran during prepare");
}

#[test]
fn prepare_fetches_the_named_remote() {
    let repo = TestRepo::new("checkout-remote");
    let base = repo.rev("main");
    let head = repo.commit_on("work/remote", &base, "remote.txt", "remote\n");
    repo.push("work/remote");
    // The repository's remote is `upstream`; a leftover `origin` points nowhere.
    repo.rename_origin_to("upstream");

    let checkout = GateCheckout::new(&repo.repo, Stage::Commands, "upstream");
    assert_eq!(checkout.remote(), "upstream");
    assert_eq!(checkout.prepare(&head).unwrap(), head);
    assert!(checkout.path().join("remote.txt").exists());

    let hardcoded = GateCheckout::new(&repo.repo, Stage::Commands, "origin");
    assert!(
        hardcoded.prepare(&head).is_err(),
        "a fetch from the wrong remote name must fail"
    );
}

#[test]
fn an_empty_leftover_checkout_directory_is_cleared() {
    let repo = TestRepo::new("checkout-empty-leftover");
    let base = repo.rev("main");
    let head = repo.commit_on("work/empty", &base, "empty.txt", "empty\n");

    let checkout = GateCheckout::new(&repo.repo, Stage::Commands, "origin");
    let path = checkout.path();
    // What a failed `worktree add` leaves behind: the directory, no worktree, no content.
    fs::create_dir_all(&path).unwrap();
    assert!(path.read_dir().unwrap().next().is_none());

    assert_eq!(checkout.prepare(&head).unwrap(), head);
    assert!(
        path.join("empty.txt").exists(),
        "the worktree was not added"
    );
}

#[test]
fn a_non_empty_checkout_directory_is_refused() {
    let repo = TestRepo::new("checkout-occupied");
    let base = repo.rev("main");
    let head = repo.commit_on("work/occupied", &base, "occupied.txt", "occupied\n");

    let checkout = GateCheckout::new(&repo.repo, Stage::Commands, "origin");
    let path = checkout.path();
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("keep.txt"), "keep\n").unwrap();

    let error = checkout.prepare(&head).unwrap_err();
    assert!(
        matches!(error, GateError::CheckoutOccupied(ref refused) if *refused == path),
        "a non-empty directory that is not a worktree must be refused: {error}"
    );
    assert_eq!(
        fs::read_to_string(path.join("keep.txt")).unwrap(),
        "keep\n",
        "the refused directory was modified"
    );
}

#[test]
fn an_inherited_git_dir_does_not_redirect_prepare() {
    let repo = TestRepo::new("checkout-gitdir");
    let base = repo.rev("main");
    let head = repo.commit_on("work/gitdir", &base, "gitdir.txt", "gitdir\n");
    // Another repository, with no remote, so a gate command that picked up `GIT_DIR` fails
    // instead of quietly working in the wrong place.
    let other = TestRepo::new("checkout-gitdir-other");
    other.git(&["remote", "remove", "origin"]);

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_prepare_under_an_inherited_git_dir",
            "--nocapture",
        ])
        .env("ARC_GATE_GIT_DIR_REPO", common::path(&repo.repo))
        .env("ARC_GATE_GIT_DIR_SHA", &head)
        .env("GIT_DIR", other.repo.join(".git"))
        .env("GIT_WORK_TREE", &other.repo)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "prepare failed with an inherited GIT_DIR; child said:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("PREPARED {head}")),
        "the child did not prepare the intended commit:\n{stdout}"
    );
    let path = repo.repo.join(".radian/arc/gates/commands");
    assert!(path.join("gitdir.txt").exists());
    assert!(
        !other.repo.join(".radian").exists(),
        "a gate command ran in the repository GIT_DIR named"
    );
}

/// Child for `an_inherited_git_dir_does_not_redirect_prepare`. No-op unless spawned; the
/// parent sets `GIT_DIR`/`GIT_WORK_TREE` in the environment of this process only.
#[test]
fn child_prepare_under_an_inherited_git_dir() {
    let Ok(repository) = std::env::var("ARC_GATE_GIT_DIR_REPO") else {
        return;
    };
    let sha = std::env::var("ARC_GATE_GIT_DIR_SHA").unwrap();
    let checkout = GateCheckout::new(PathBuf::from(repository), Stage::Commands, "origin");
    let commit = checkout.prepare(&sha).unwrap();
    println!("PREPARED {commit}");
}

mod common;

use std::fs;

use common::TestRepo;
use radian_arc::gate::{GateCheckout, Stage};

#[test]
fn prepare_removes_a_previous_sha_and_keeps_target() {
    let repo = TestRepo::new("checkout-sha");
    let base = repo.rev("main");
    let first = repo.commit_on("work/first", &base, "first.txt", "first\n");
    let second = repo.commit_on("work/second", &base, "second.txt", "second\n");

    let checkout = GateCheckout::new(&repo.repo, Stage::Commands);
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

    GateCheckout::new(&repo.repo, Stage::Commands)
        .prepare(&head)
        .unwrap();
    assert!(!marker.exists(), "core.hooksPath hook ran during prepare");
}

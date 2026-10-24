//! Every git call that talks to a remote is bounded by `[limits] git_remote_timeout_secs`
//! (ADR 0048 decision 7.8): a remote that accepts the connection and then hangs fails the
//! step after the limit, with `git <subcommand> timed out after <n> s`, and the command's
//! whole process group is gone.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::{HangRemote, TestRepo};
use radian_arc::gate::{GateCheckout, Stage};
use radian_arc::store::{Store, config};

/// Writes the arc config a call reads, with a one-second remote limit. `init` keeps a
/// `config.toml` that already exists, so this is also the config the checks run with.
fn short_remote_limit(repo: &TestRepo) {
    let dir = repo.repo.join(".radian").join("arc");
    fs::create_dir_all(&dir).expect("the arc directory");
    let text = config::default_text("main").replace(
        "git_remote_timeout_secs = 300",
        "git_remote_timeout_secs = 1",
    );
    fs::write(dir.join("config.toml"), text).expect("the config");
}

/// A gate fetch of a remote that hangs runs out after the limit, fails with the line, and the
/// child the remote spawns is killed with the git process group.
#[test]
fn a_gate_fetch_that_hangs_fails_with_the_line_and_kills_its_child() {
    let repo = TestRepo::new("gate-fetch-hang");
    short_remote_limit(&repo);
    let hang = HangRemote::install(&repo, "gate-fetch", "upload");
    let head = repo.rev("main");
    let checkout = GateCheckout::new(&repo.repo, Stage::Commands, "origin");

    let started = Instant::now();
    let error = checkout.prepare(&head).expect_err("the fetch runs out");
    assert!(
        error.to_string().contains("git fetch timed out after 1 s"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the call was bounded: {:?}",
        started.elapsed()
    );

    let child = hang.child_pid();
    assert!(child > 0);
    hang.assert_gone(child, "the remote's child");
    let server = hang.server_pid();
    hang.assert_gone(server, "the remote itself");
}

/// `init`'s reachability and dry-run push checks are remote calls too: a hanging remote makes
/// each report its own timeout line, as an ordinary failed check, and leaves no child behind.
#[test]
fn init_checks_that_hang_report_the_line_and_kill_their_children() {
    let repo = TestRepo::new("init-hang");
    short_remote_limit(&repo);
    let hang = HangRemote::install(&repo, "init", "both");

    let started = Instant::now();
    let report = Store::init(&repo.repo, false).expect("init succeeds");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the checks were bounded: {:?}",
        started.elapsed()
    );

    let reachable = report
        .checks
        .iter()
        .find(|check| check.name == "`origin` reachable")
        .expect("the reachability check");
    assert_eq!(
        reachable.detail.as_ref().err().map(String::as_str),
        Some("git ls-remote timed out after 1 s"),
        "{:?}",
        reachable.detail
    );
    let push = report
        .checks
        .iter()
        .find(|check| check.name == "push to `origin` `main`")
        .expect("the push check");
    assert_eq!(
        push.detail.as_ref().err().map(String::as_str),
        Some("git push timed out after 1 s"),
        "{:?}",
        push.detail
    );

    hang.assert_gone(hang.child_pid(), "the remote's child");
    hang.assert_gone(hang.server_pid(), "the remote itself");
}

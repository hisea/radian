//! arc's own fetches into one repository take turns (ADR 0048 decision 10): before every
//! `git fetch` it runs, arc holds an exclusive `flock` on
//! `<git common dir>/radian-arc-fetch.lock`, so two arc processes never race git's
//! compare-and-swap update of `refs/remotes/<remote>/<branch>`. A linked worktree resolves the
//! same common dir and so shares the lock. A fetch that gives up waiting for the lock fails
//! with a line that names the lock, inside the same `[limits] git_remote_timeout_secs` limit
//! the fetch has.
#![cfg(unix)]

mod common;

use std::os::unix::io::AsRawFd as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{TestRepo, path};
use radian_arc::{fetch_for_test, fetch_lock_for_test};

/// A local fetch finishes well inside this, so a failure means a real problem, not the limit.
const FETCH_LIMIT: Duration = Duration::from_secs(60);

/// Commits one file on `origin`'s main from another clone, so a clone that has not fetched is
/// stale and every fetch in it has to move `refs/remotes/origin/main`.
fn move_origin(repo: &TestRepo) {
    let other = repo.second_clone("mover");
    std::fs::write(other.join("moved.txt"), "moved\n").expect("the moved file");
    repo.git_in(&other, &["add", "moved.txt"]);
    repo.git_in(&other, &["commit", "-m", "move origin"]);
    repo.git_in(&other, &["push", "origin", "main"]);
}

/// Starts a `git fetch origin` in `dir`, with the test's git identity, without asserting.
fn start_fetch(dir: &Path) -> Child {
    Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(["fetch", "origin"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git fetch runs")
}

/// How many fetches race in one round, and how many rounds the control may try.
const RACERS: usize = 8;
const CONTROL_ROUNDS: usize = 20;

/// The lock is what keeps arc from losing this race: concurrent fetches through the helper
/// into one repository, with a moved remote, all succeed.
///
/// The same scenario without the lock loses the race at least once over [`CONTROL_ROUNDS`]
/// rounds: `git fetch` updates `refs/remotes/origin/main` with a compare-and-swap, and two
/// processes that read the same old value make one fail with `cannot lock ref` or `incorrect
/// old value provided`. That control is timing-dependent by nature, so it is bounded and the
/// locked test does not depend on it: if no round ever raced, the helper test below would
/// still be the assertion that matters.
#[test]
fn concurrent_fetches_through_the_helper_into_one_repository_all_succeed() {
    let repo = TestRepo::new("fetch-lock-concurrent");
    move_origin(&repo);
    let main = repo
        .ls_remote("refs/heads/main")
        .expect("origin has a main");

    let mut threads = Vec::new();
    for _ in 0..RACERS {
        let dir = repo.repo.clone();
        threads.push(std::thread::spawn(move || {
            fetch_for_test(&dir, &["fetch", "origin"], FETCH_LIMIT)
        }));
    }
    for (n, thread) in threads.into_iter().enumerate() {
        let result = thread.join().expect("the fetch thread did not panic");
        assert_eq!(result, Ok(()), "fetch {n} through the lock");
    }
    assert_eq!(repo.rev("refs/remotes/origin/main"), main);
}

/// The control for the test above: without the lock the same race is lost. It is a check of
/// the test's own premise, not of the helper, so it only asserts that the race showed up at
/// least once over the bounded number of rounds.
#[test]
fn without_the_lock_concurrent_fetches_into_a_moved_clone_race() {
    let repo = TestRepo::new("fetch-lock-race");
    let mut ever_raced = false;
    for round in 0..=CONTROL_ROUNDS {
        // A clone of this round's origin main, then origin moves under it: every fetch has to
        // replace the same stale tracking ref.
        let clone = repo.repo.with_file_name(format!("stale-{round}"));
        repo.git(&["clone", path(&repo.origin), path(&clone)]);
        let moved = repo.advance_main_on("origin", "moved.txt", &format!("{round}\n"));
        assert!(!moved.is_empty());

        let children: Vec<Child> = (0..RACERS).map(|_| start_fetch(&clone)).collect();
        let mut raced: Option<String> = None;
        for child in children {
            let output = child.wait_with_output().expect("the fetch ends");
            if !output.status.success() {
                raced.get_or_insert_with(|| {
                    String::from_utf8_lossy(&output.stderr).trim().to_owned()
                });
            }
        }
        if let Some(stderr) = raced {
            // The failure is the lost compare-and-swap, not some unrelated error: git could
            // not update the one tracking ref the racers share.
            assert!(
                stderr.contains("refs/remotes/origin/main")
                    && (stderr.contains("cannot lock ref")
                        || stderr.contains("incorrect old value")
                        || stderr.contains("failed to update ref")),
                "the failure is the lost ref update: {stderr}"
            );
            ever_raced = true;
            break;
        }
    }
    assert!(
        ever_raced,
        "unlocked concurrent fetches never raced in {CONTROL_ROUNDS} rounds; the control cannot \
         prove the premise, so the locked test stands on its own"
    );
}

/// One repository, one lock: a linked worktree resolves the same git common dir, so a fetch
/// that runs from the worktree waits on the lock held from the main checkout.
#[test]
fn a_linked_worktree_shares_the_repositorys_fetch_lock() {
    let repo = TestRepo::new("fetch-lock-worktree");
    let worktree = repo.repo.with_file_name("linked");
    repo.git(&["worktree", "add", "-b", "linked", path(&worktree), "main"]);

    let main_lock = fetch_lock_for_test(&repo.repo).expect("the main checkout's lock");
    let linked_lock = fetch_lock_for_test(&worktree).expect("the worktree's lock");
    assert_eq!(main_lock, linked_lock, "one lock per repository");
    assert_eq!(
        main_lock.file_name().and_then(|name| name.to_str()),
        Some("radian-arc-fetch.lock")
    );

    // Behavior, not only the path: holding the main checkout's lock blocks a fetch from the
    // worktree, and its failure names that one file.
    let held = hold_lock(&main_lock);
    let started = Instant::now();
    let error = fetch_for_test(&worktree, &["fetch", "origin"], Duration::from_secs(1))
        .expect_err("the lock is held");
    assert!(
        error.contains("radian-arc-fetch.lock"),
        "the failure names the lock: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the wait was bounded: {:?}",
        started.elapsed()
    );
    drop(held);
    fetch_for_test(&worktree, &["fetch", "origin"], FETCH_LIMIT).expect("the lock is free again");
}

/// A holder that keeps the lock past the fetch's limit makes the waiter fail with the
/// lock-named reason inside that limit, and the fetch that follows succeeds.
#[test]
fn a_held_fetch_lock_fails_the_waiter_with_the_lock_named_reason() {
    let repo = TestRepo::new("fetch-lock-held");
    let lock = fetch_lock_for_test(&repo.repo).expect("the lock path");
    let held = hold_lock(&lock);

    let started = Instant::now();
    let error = fetch_for_test(&repo.repo, &["fetch", "origin"], Duration::from_secs(1))
        .expect_err("the lock is held");
    let waited = started.elapsed();
    assert!(
        error.contains("radian-arc-fetch.lock"),
        "the reason names the lock, not a git error: {error}"
    );
    assert!(error.contains("1 s"), "the reason names the limit: {error}");
    assert!(
        error.contains(&lock.display().to_string()),
        "the reason names the lock file: {error}"
    );
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_secs(10),
        "the waiter fails inside the limit: {waited:?}"
    );

    drop(held);
    fetch_for_test(&repo.repo, &["fetch", "origin"], FETCH_LIMIT)
        .expect("the fetch runs once the lock is released");
}

/// Takes the exclusive `flock` on `lock` for as long as the returned file stays open.
fn hold_lock(lock: &Path) -> std::fs::File {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock)
        .expect("the lock file opens");
    // SAFETY: `flock` is a syscall on the open descriptor; no memory is involved.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        result,
        0,
        "the test takes the lock: {}",
        std::io::Error::last_os_error()
    );
    file
}

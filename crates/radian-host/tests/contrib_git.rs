//! The git contributor against real repositories (ADR 0027 C3): the branch from
//! `rev-parse --abbrev-ref HEAD`, and a worktree detected by comparing the absolute git dir
//! with the common dir. The environment has `git`; a skip keeps the suite honest if it does
//! not.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use radian_host::{ContributionStore, ContributorRunner, GitContributor};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A fresh, uniquely named scratch directory under the system temp dir.
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "radian-contrib-git-{}-{tag}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch dir is creatable");
    dir
}

/// Runs git in `dir`, failing the test on a non-zero status.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} in {dir:?}");
}

/// A repo with one empty commit on `main`.
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(
        dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
}

/// One tick, then the stored text for an item.
async fn tick_text(store: &Arc<ContributionStore>, dir: &Path, item: &str) -> Option<String> {
    let contributor = Arc::new(GitContributor::new(dir));
    let mut runner = ContributorRunner::new(contributor, Arc::clone(store));
    runner.tick().await;
    store.get(item).map(|contribution| {
        contribution
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    })
}

/// The contributor publishes the checked-out branch.
#[tokio::test]
async fn the_git_contributor_publishes_the_branch() {
    if !git_available() {
        return;
    }
    let dir = scratch("branch");
    init_repo(&dir);
    let store = ContributionStore::new();
    let branch = tick_text(&store, &dir, "git_branch").await;
    assert_eq!(branch.as_deref(), Some("main"));
    // No value outside a repository: the item hides rather than showing an error.
    let plain = scratch("plain");
    let store = ContributionStore::new();
    assert_eq!(tick_text(&store, &plain, "git_branch").await, None);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&plain);
}

/// The `worktree` value appears only inside a linked worktree, and names the worktree.
#[tokio::test]
async fn the_git_contributor_detects_a_worktree() {
    if !git_available() {
        return;
    }
    let main = scratch("main");
    init_repo(&main);
    let linked = scratch("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "side",
        ],
    );

    // The main checkout is not a worktree: the item hides. `{repo}` is the root's name.
    let store = ContributionStore::new();
    assert_eq!(tick_text(&store, &main, "worktree").await, None);
    let repo_name = main.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        tick_text(&store, &main, "git_root").await.as_deref(),
        Some(repo_name)
    );
    // The linked checkout is: the value is the worktree's own name (the footer item adds the
    // `worktree ` label), and the branch is its own.
    let store = ContributionStore::new();
    let worktree = tick_text(&store, &linked, "worktree").await;
    let name = linked.file_name().unwrap().to_str().unwrap();
    assert_eq!(worktree.as_deref(), Some(name));
    assert_eq!(
        tick_text(&store, &linked, "git_branch").await.as_deref(),
        Some("side")
    );
    // `{repo}` is the shared root's name in a worktree too, not the worktree's own name.
    assert_eq!(
        tick_text(&store, &linked, "git_root").await.as_deref(),
        Some(repo_name)
    );

    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
        .current_dir(&main)
        .status();
    let _ = std::fs::remove_dir_all(&main);
    let _ = std::fs::remove_dir_all(&linked);
}

/// The change counts are the data slot the numbers row fills in slice 2b (ADR 0027):
/// modified tracked files and untracked files, from `git status --porcelain`.
#[tokio::test]
async fn the_git_contributor_publishes_the_change_counts() {
    if !git_available() {
        return;
    }
    let dir = scratch("changes");
    init_repo(&dir);
    std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
    git(&dir, &["add", "tracked.txt"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "add",
        ],
    );
    // One modified tracked file, one untracked file.
    std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let store = ContributionStore::new();
    assert_eq!(
        tick_text(&store, &dir, "git_modified").await.as_deref(),
        Some("1")
    );
    assert_eq!(
        tick_text(&store, &dir, "git_untracked").await.as_deref(),
        Some("1")
    );
    // A clean tree still publishes the slot, as zero.
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "clean",
        ],
    );
    let store = ContributionStore::new();
    assert_eq!(
        tick_text(&store, &dir, "git_modified").await.as_deref(),
        Some("0")
    );
    assert_eq!(
        tick_text(&store, &dir, "git_untracked").await.as_deref(),
        Some("0")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

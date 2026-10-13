//! Read-only git tools, and the hard assertion that they leave `.git` byte-for-byte and
//! mtime-for-mtime unchanged.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::{TempDir, assert_outside, call, text, tools};
use radian_coding::{CodingTools, Limits};
use serde_json::json;

/// Sets up a small repository with one commit, one modification, and one untracked file.
fn repo(dir: &TempDir) -> std::path::PathBuf {
    let root = dir.file("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    // `git commit` triggers git's background auto-maintenance, which can create
    // `.git/objects/maintenance.lock` after this setup returns and race the `.git` snapshot
    // in `git_status_and_diff_...`. That is git's own job, not the read-only tools', so pin
    // the repo: no auto maintenance, no auto gc. (Observed on git 2.55 in the Linux VM; the
    // macOS git 2.53 did not run it, which is why the test only flaked there.)
    git(&root, &["config", "maintenance.auto", "false"]);
    git(&root, &["config", "gc.auto", "0"]);
    std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "new\n").unwrap();
    root
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every file under `.git`, keyed by relative path, with length, mtime, and content, so a
/// rewrite of identical bytes is still detected.
fn snapshot_git(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let git_dir = root.join(".git");
    let mut stack = vec![git_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                let key = path
                    .strip_prefix(&git_dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let mtime = meta.modified().unwrap();
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.insert(key, (meta.len(), mtime, bytes));
            }
        }
    }
    out
}

#[tokio::test]
async fn git_status_and_diff_report_the_worktree_and_leave_dot_git_untouched() {
    let dir = TempDir::new("git-status");
    let root = repo(&dir);
    let tools = tools(&root);

    let before = snapshot_git(&root);

    let status = call(&tools, "git_status", json!({})).await.unwrap();
    let status_text = text(&status);
    assert!(status_text.contains("tracked.txt"), "{status_text}");
    assert!(status_text.contains("untracked.txt"), "{status_text}");

    let diff = call(&tools, "git_diff", json!({})).await.unwrap();
    let diff_text = text(&diff);
    assert!(diff_text.contains("+two"), "{diff_text}");

    let after = snapshot_git(&root);
    assert_eq!(
        before, after,
        ".git must be byte-for-byte and mtime-for-mtime unchanged by read-only git tools"
    );
}

#[tokio::test]
async fn staged_diff_only_shows_staged_changes() {
    let dir = TempDir::new("git-staged");
    let root = repo(&dir);
    git(&root, &["add", "tracked.txt"]);
    let tools = tools(&root);

    let unstaged = call(&tools, "git_diff", json!({})).await.unwrap();
    assert!(!text(&unstaged).contains("+two"), "{}", text(&unstaged));

    let staged = call(&tools, "git_diff", json!({"staged": true}))
        .await
        .unwrap();
    assert!(text(&staged).contains("+two"), "{}", text(&staged));
}

#[tokio::test]
async fn a_path_limited_diff_stays_inside_the_workspace() {
    let dir = TempDir::new("git-path");
    let root = repo(&dir);
    let outside = common::sentinel_outside(&dir);
    let tools = tools(&root);

    let diff = call(&tools, "git_diff", json!({"path": "tracked.txt"}))
        .await
        .unwrap();
    assert!(text(&diff).contains("tracked.txt"));

    let failure = call(
        &tools,
        "git_diff",
        json!({"path": outside.join("sentinel.txt").to_string_lossy()}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "absolute");

    let failure = call(
        &tools,
        "git_diff",
        json!({"path": "../outside/sentinel.txt"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "../outside");
}

#[tokio::test]
async fn git_outside_a_repository_is_a_clear_failure() {
    let dir = TempDir::new("git-not-repo");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "git_status", json!({})).await.unwrap_err();
    assert_eq!(failure.code, "git_failed", "{failure:?}");
    assert!(
        failure.message.contains("not a git repository"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn git_output_is_capped_with_a_marker() {
    let dir = TempDir::new("git-cap");
    let root = repo(&dir);
    let tools = CodingTools::new(&root).unwrap().with_limits(Limits {
        max_git_output_bytes: 16,
        ..Limits::default()
    });

    let output = call(&tools, "git_status", json!({})).await.unwrap();
    assert!(text(&output).ends_with("[git output truncated]\n"));
    assert!(text(&output).len() < 200);
}

#[tokio::test]
async fn git_tools_are_not_confused_with_the_other_tools() {
    // `git_status` ignores extra arguments at the schema level: the spec has no properties.
    let dir = TempDir::new("git-extra");
    let root = repo(&dir);
    let tools = tools(&root);
    // (Schema validation happens in the core, not here; executing with extra keys is still
    // fine because the handler reads none.)
    let output = call(&tools, "git_status", json!({"anything": 1}))
        .await
        .unwrap();
    assert!(text(&output).contains("##"), "{}", text(&output));
}

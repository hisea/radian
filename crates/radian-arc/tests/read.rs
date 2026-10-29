//! The read-only diff the arc TUI draws (ADR 0048 decisions 10 and 13): the task's own change,
//! from the merge base of the remote main and the head — the range the review stage reads — and
//! not `base_sha..head_sha`, which carries main's merged changes after a rework's `merge-main`.
//!
//! The fixture is a real repository with a bare `origin`, so `refs/remotes/origin/main` is
//! exactly what the wrapper's last fetch left.

mod common;

use std::fs;

use common::{Repo, write};
use radian_arc::read::{self, DiffBase, DiffSource, TaskDiff};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};

/// A store in the clone with the default `origin`/`main` configuration.
fn store(repo: &Repo) -> Store {
    let root = repo.arc();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
    Store::at(repo.clone.clone())
}

/// A store in the clone whose `[integration] target_branch` is `target`.
fn store_with_target(repo: &Repo, target: &str) -> Store {
    let text = config::default_text("main").replace(
        "# target_branch = \"main\"      # not set: the value of [repository] main_branch",
        &format!("target_branch = \"{target}\""),
    );
    let root = repo.arc();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), text).unwrap();
    Store::at(repo.clone.clone())
}

/// Commits one file on the current branch and returns the commit id.
fn commit(repo: &Repo, path: &str, content: &str, message: &str) -> String {
    write(&repo.clone.join(path), content);
    repo.git(&["add", path]);
    repo.git(&["commit", "-q", "-m", message]);
    repo.git(&["rev-parse", "HEAD"]).trim().to_owned()
}

/// Writes the wrapper's `AttemptFinished` record for one attempt.
fn finish(repo: &Repo, task: &str, attempt: u32, base: &str, head: &str) {
    let dir = repo.run_dir(task, attempt);
    fs::create_dir_all(&dir).unwrap();
    let value = serde_json::json!({
        "type": "AttemptFinished",
        "attempt_id": format!("{task}-{attempt}"),
        "token": "t",
        "exit_code": 0,
        "outcome": "completed",
        "base_sha": base,
        "head_sha": head,
        "pushed": true,
    });
    fs::write(dir.join("attempt_finished.json"), value.to_string()).unwrap();
}

fn paths(diff: &TaskDiff) -> Vec<&str> {
    diff.files.iter().map(|file| file.path.as_str()).collect()
}

#[test]
fn the_diff_is_the_task_own_change_from_the_merge_base_with_main() {
    let repo = Repo::new("read-diff");
    let base = repo.main_sha();
    // The task's branch: y.rs. Main then gains x.rs, and the branch merges it in — what a
    // conflict rework does before the next attempt.
    repo.git(&["checkout", "-q", "-B", "work/T-read-merge", &base]);
    commit(&repo, "y.rs", "the task's change\n", "the task change");
    repo.git(&["checkout", "-q", "main"]);
    commit(&repo, "x.rs", "main's change\n", "main's change");
    repo.git(&["push", "-q", "origin", "main"]);
    repo.git(&["checkout", "-q", "work/T-read-merge"]);
    repo.git(&["merge", "-q", "--no-ff", "origin/main", "-m", "merge main"]);
    let head1 = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();

    let store = store(&repo);
    let plan = store
        .add_plan("read", "the read plan", "a request\n")
        .unwrap();
    let task = store
        .add_task(
            &plan,
            "merge",
            "the merge task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head1.clone());
            Ok(())
        })
        .unwrap();
    finish(&repo, &task, 1, &base, &head1);

    let diff = read::task_diff(&store, &task, Some(1)).unwrap();
    assert_eq!(diff.source, DiffSource::Range);
    assert_eq!(diff.base, DiffBase::MergeBase, "the remote main ref exists");
    assert_eq!(
        paths(&diff),
        ["y.rs"],
        "main's merged x.rs is not the task's change: {:?}",
        paths(&diff)
    );
    assert!(
        diff.files[0].text.contains("the task's change"),
        "the diff body is the task's own: {}",
        diff.files[0].text
    );

    // A newer attempt adds z.rs: each attempt is diffed from its own head.
    let head2 = commit(&repo, "z.rs", "a later change\n", "a later change");
    finish(&repo, &task, 2, &base, &head2);
    store
        .change_task(&task, |task| {
            task.attempt = 2;
            task.head_sha = Some(head2.clone());
            Ok(())
        })
        .unwrap();

    let older = read::task_diff(&store, &task, Some(1)).unwrap();
    assert_eq!(
        paths(&older),
        ["y.rs"],
        "an older attempt uses its own head, not the newest one: {:?}",
        paths(&older)
    );
    let newer = read::task_diff(&store, &task, Some(2)).unwrap();
    assert_eq!(
        paths(&newer),
        ["y.rs", "z.rs"],
        "the newest attempt's head holds both commits: {:?}",
        paths(&newer)
    );
    for diff in [&older, &newer] {
        assert!(
            !paths(diff).contains(&"x.rs"),
            "no attempt lists main's merged change: {:?}",
            paths(diff)
        );
    }

    // No remote main ref: the attempt's recorded base is the fallback, and the diff says so.
    repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    let fallback = read::task_diff(&store, &task, Some(1)).unwrap();
    assert_eq!(
        fallback.base,
        DiffBase::NoRemoteTarget,
        "the fallback is marked for the source line"
    );
    assert_eq!(
        paths(&fallback),
        ["x.rs", "y.rs"],
        "without a remote main ref the recorded base..head is used: {:?}",
        paths(&fallback)
    );
}

/// A task with no commit of its own and no remote target ref: the diff is empty, but the fallback
/// is still reported, so the page draws `base..head · no remote target` under `no change yet`.
#[test]
fn a_no_change_diff_without_a_remote_main_ref_keeps_the_fallback() {
    let repo = Repo::new("read-diff-empty");
    let base = repo.main_sha();
    let store = store(&repo);
    let plan = store
        .add_plan("read", "the read plan", "a request\n")
        .unwrap();
    let task = store
        .add_task(
            &plan,
            "empty",
            "the empty task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.base_sha = Some(base.clone());
            task.head_sha = Some(base.clone());
            Ok(())
        })
        .unwrap();
    finish(&repo, &task, 1, &base, &base);
    repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);

    let diff = read::task_diff(&store, &task, Some(1)).unwrap();
    assert!(
        diff.files.is_empty(),
        "no commit of its own means no change: {:?}",
        paths(&diff)
    );
    assert_eq!(diff.source, DiffSource::Range);
    assert_eq!(
        diff.base,
        DiffBase::NoRemoteTarget,
        "the source line still says there is no remote target ref"
    );
}

/// A remote target ref that exists but shares no ancestor with the attempt's head is not a
/// missing ref: the diff falls back to the recorded base and says `no merge base with target`,
/// so the page never claims a ref is absent while it is there.
#[test]
fn a_remote_main_ref_without_a_common_ancestor_is_not_a_missing_ref() {
    let repo = Repo::new("read-diff-unrelated");
    let base = repo.main_sha();
    repo.git(&["checkout", "-q", "-B", "work/T-read-unrelated", &base]);
    let head = commit(&repo, "y.rs", "the task's change\n", "the task change");

    let store = store(&repo);
    let plan = store
        .add_plan("read", "the read plan", "a request\n")
        .unwrap();
    let task = store
        .add_task(
            &plan,
            "unrelated",
            "the unrelated task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .unwrap();
    finish(&repo, &task, 1, &base, &head);

    // A root commit with no parent, pointed at by the remote main ref: both refs exist, and git
    // has no merge base for them.
    repo.git(&["checkout", "-q", "--orphan", "unrelated"]);
    write(&repo.clone.join("other.rs"), "an unrelated history\n");
    repo.git(&["add", "other.rs"]);
    repo.git(&["commit", "-q", "-m", "an unrelated root"]);
    let other = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["update-ref", "refs/remotes/origin/main", &other]);

    let diff = read::task_diff(&store, &task, Some(1)).unwrap();
    assert_eq!(
        diff.base,
        DiffBase::Unrelated,
        "the ref exists, so the line must not say there is no remote target"
    );
    assert_eq!(
        paths(&diff),
        ["y.rs"],
        "the recorded base is the fallback range: {:?}",
        paths(&diff)
    );
}

/// With `[integration] target_branch = "release"` ahead of `main`, a task built on `release`
/// is measured from the remote target branch: `release`'s own commit is not part of the task's
/// change, in the diff or in the merge page's commit list. Reading `repository.main_branch`
/// would list it.
#[test]
fn a_target_branch_ahead_of_main_is_the_task_diff_base() {
    let repo = Repo::new("read-target-ahead");
    let base = repo.main_sha();
    // `release` moves ahead of `main`; the task branch starts from `release`.
    repo.git(&["checkout", "-q", "-B", "release", &base]);
    let release = commit(&repo, "release.txt", "release only\n", "release only");
    repo.git(&["push", "-q", "origin", "release"]);
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["checkout", "-q", "-B", "work/T-read-target", &release]);
    let head = commit(
        &repo,
        "feature.txt",
        "the task's change\n",
        "the task change",
    );

    let store = store_with_target(&repo, "release");
    let plan = store
        .add_plan("read", "the read plan", "a request\n")
        .unwrap();
    let task = store
        .add_task(
            &plan,
            "target",
            "the target task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.base_sha = Some(release.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .unwrap();
    finish(&repo, &task, 1, &release, &head);

    let diff = read::task_diff(&store, &task, Some(1)).unwrap();
    assert_eq!(diff.base, DiffBase::MergeBase, "origin/release exists");
    assert_eq!(
        paths(&diff),
        ["feature.txt"],
        "release's own commit is not the task's change: {:?}",
        paths(&diff)
    );
    let merge = read::merge_view(&store, &task, false).unwrap();
    let subjects: Vec<&str> = merge
        .commits
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect();
    assert_eq!(
        subjects,
        ["the task change"],
        "the merge page lists only the task's commits: {subjects:?}"
    );
}

//! The read-only diff the arc TUI draws (ADR 0048 decisions 10 and 13): the task's own change,
//! from the merge base of the remote main and the head — the range the review stage reads — and
//! not `base_sha..head_sha`, which carries main's merged changes after a rework's `merge-main`.
//!
//! The fixture is a real repository with a bare `origin`, so `refs/remotes/origin/main` is
//! exactly what the wrapper's last fetch left.

mod common;

use std::fs;

use common::{Repo, write};
use radian_arc::read::{self, DiffSource, TaskDiff};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};

/// A store in the clone with the default `origin`/`main` configuration.
fn store(repo: &Repo) -> Store {
    let root = repo.arc();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
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
    assert!(!diff.fallback, "the remote main ref exists");
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
    assert!(
        fallback.fallback,
        "the fallback is marked for the source line"
    );
    assert_eq!(
        paths(&fallback),
        ["x.rs", "y.rs"],
        "without a remote main ref the recorded base..head is used: {:?}",
        paths(&fallback)
    );
}

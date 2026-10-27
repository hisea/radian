//! `flows.<name>.commit_dirty`: the wrapper commits what a completed worker left uncommitted
//! (ADR 0048 decision 8).
//!
//! Every test configures a small command worker as the `default` flow's worker and drives the
//! real `radian arc start`, so the claim, the wrapper, the control-file check, the commit and
//! the delivery check are exercised end to end.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use radian_arc::store::{Store, plan_ops};
use serde_json::{Value, json};

fn setup(repo: &Repo) -> Store {
    Store::init(&repo.clone, false).expect("arc init");
    Store::open(&repo.clone).expect("the store opens")
}

fn add_plan(store: &Store, slug: &str) -> String {
    store
        .add_plan(slug, "A plan", "the request\n")
        .expect("add a plan")
}

fn add_task(store: &Store, plan: &str, slug: &str) -> String {
    store
        .add_task(
            plan,
            slug,
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task")
}

/// Replaces one piece of `config.toml`.
fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// Makes the `default` flow run `worker` and sets its `commit_dirty` when `Some`. The flow
/// table goes before `[ids]`, after every `[worker]` key.
fn set_default_flow(repo: &Repo, worker: &Path, commit_dirty: Option<bool>) {
    let key = commit_dirty.map_or_else(String::new, |value| format!("commit_dirty = {value}\n"));
    tune(
        repo,
        "[ids]\n",
        &format!(
            "[flows.default]\ncommand = [\"{}\"]\n{key}\n[ids]\n",
            worker.display()
        ),
    );
}

/// Writes an executable worker script and returns its path. The worker runs in the task's
/// worktree and leaves `image.txt` untracked.
fn worker(repo: &Repo, name: &str, result: &str) -> PathBuf {
    let path = repo.root.join(name);
    let body =
        format!("#!/bin/sh\nset -e\necho \"the change\" > image.txt\nprintf '%s\\n' '{result}'\n");
    fs::write(&path, body).expect("the script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).expect("the script").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("make it executable");
    }
    path
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn finished(repo: &Repo, task: &str) -> Value {
    serde_json::from_str(&read(&repo.run_dir(task, 1).join("attempt_finished.json")))
        .expect("attempt_finished.json")
}

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

/// The `work/<task>` tip in the main clone.
fn branch_tip(repo: &Repo, task: &str) -> String {
    repo.git(&["rev-parse", &format!("refs/heads/work/{task}")])
        .trim()
        .to_owned()
}

/// `commit_dirty = true`: the worker's untracked file is committed on the branch before the
/// delivery check, with the `Result.text` first line as the subject and the `merge-main`
/// identity, and the attempt records the commit in its notes.
///
/// Mutation TK0201-M1: skip the commit step when the key is true → this fails.
#[test]
fn a_dirty_worktree_is_committed_with_the_result_text_as_its_subject() {
    let repo = Repo::new("commit-dirty");
    let store = setup(&repo);
    let plan = add_plan(&store, "commit");
    let task = add_task(&store, &plan, "image");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = worker(
        &repo,
        "commit-worker",
        r#"{"type":"Result","outcome":"completed","text":"Add the image\n\nmore"}"#,
    );
    set_default_flow(&repo, &worker, Some(true));

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished = finished(&repo, &task);
    assert_eq!(finished["pushed"], json!(true), "{finished}");
    assert!(finished["failure"].is_null(), "{finished}");
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        run.stdout
    );

    let head = finished["head_sha"].as_str().expect("the attempt's head");
    assert_eq!(
        repo.git(&[
            "rev-list",
            "--count",
            &format!("{}..{head}", repo.main_sha())
        ])
        .trim(),
        "1",
        "the wrapper made exactly one commit"
    );
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s", head]).trim(),
        "Add the image",
        "the subject is the first line of the first paragraph"
    );
    for field in ["%an <%ae>", "%cn <%ce>"] {
        assert_eq!(
            repo.git(&["log", "-1", &format!("--format={field}"), head])
                .trim(),
            "arc <arc@localhost>",
            "the commit uses the `merge-main` identity"
        );
    }

    let short = &head[..7];
    let note = format!("committed dirty worktree: {short}");
    assert_eq!(finished["notes"], json!([note]), "{finished}");
    let shown = repo.radian(&["arc", "show", task.as_str()]);
    assert_eq!(shown.code, 0, "show: {}", shown.stderr);
    assert!(
        shown.stdout.contains(&format!("attempt_note: {note}")),
        "show prints the attempt's note:\n{}",
        shown.stdout
    );
}

/// An empty `Result.text` takes `<task id>: worker changes` as the subject.
#[test]
fn an_empty_result_text_uses_the_task_subject() {
    let repo = Repo::new("commit-dirty-empty");
    let store = setup(&repo);
    let plan = add_plan(&store, "commit");
    let task = add_task(&store, &plan, "empty");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = worker(
        &repo,
        "commit-worker-empty",
        r#"{"type":"Result","outcome":"completed","text":""}"#,
    );
    set_default_flow(&repo, &worker, Some(true));

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished = finished(&repo, &task);
    assert_eq!(finished["pushed"], json!(true), "{finished}");
    let head = finished["head_sha"].as_str().expect("the attempt's head");
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s", head]).trim(),
        format!("{task}: worker changes")
    );
}

/// `commit_dirty = false`, and a file without the key, keep today's rule: the dirty worktree
/// fails the attempt and nothing is pushed.
#[test]
fn a_dirty_worktree_still_fails_without_the_key() {
    for (label, value) in [("false", Some(false)), ("absent", None)] {
        let repo = Repo::new(&format!("commit-dirty-{label}"));
        let store = setup(&repo);
        let plan = add_plan(&store, "commit");
        let task = add_task(&store, &plan, "plain");
        plan_ops::confirm(&store, &plan).expect("confirm the plan");
        let worker = worker(
            &repo,
            "commit-worker-plain",
            r#"{"type":"Result","outcome":"completed","text":"Add the image"}"#,
        );
        set_default_flow(&repo, &worker, value);

        let run = repo.radian(&["arc", "start", task.as_str()]);
        assert_eq!(run.code, 0, "start failed: {}", run.stderr);
        let finished = finished(&repo, &task);
        assert_eq!(finished["pushed"], json!(false), "{finished}");
        let failure = finished["failure"].as_str().unwrap_or_default();
        assert!(
            failure.contains("uncommitted or untracked files: image.txt"),
            "{finished}"
        );
        assert_eq!(
            branch_tip(&repo, &task),
            repo.main_sha(),
            "no commit was made"
        );
        assert_eq!(repo.remote_branch(&task), None, "nothing was pushed");
    }
}

/// A worker that changes a git control file fails on that check even with the key on, and
/// nothing is committed: the commit sits after the control-file check.
///
/// Mutation TK0201-M2: run the commit before the control-file check → this fails.
#[test]
fn a_changed_control_file_fails_before_any_commit() {
    let repo = Repo::new("commit-dirty-control");
    let store = setup(&repo);
    let plan = add_plan(&store, "commit");
    let task = add_task(&store, &plan, "control");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let path = repo.root.join("control-worker");
    fs::write(
        &path,
        "#!/bin/sh\nset -e\necho \"the change\" > image.txt\n\
         printf '\\n[worker-was-here]\\n' >> \"$ARC_GIT_COMMON_DIR/config\"\n\
         printf '%s\\n' '{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"Add the image\"}'\n",
    )
    .expect("the script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).expect("the script").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("make it executable");
    }
    set_default_flow(&repo, &path, Some(true));

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished = finished(&repo, &task);
    let failure = finished["failure"].as_str().unwrap_or_default();
    assert!(failure.contains("changed git control files"), "{finished}");
    assert_eq!(finished["pushed"], json!(false), "{finished}");
    assert_eq!(
        branch_tip(&repo, &task),
        repo.main_sha(),
        "no commit was made"
    );
    assert!(
        !finished["notes"]
            .as_array()
            .is_some_and(|notes| notes.iter().any(|note| note
                .as_str()
                .is_some_and(|note| note.starts_with("committed dirty worktree: ")))),
        "{finished}"
    );
}

/// A worker whose outcome is not `completed` never has its changes committed, even with the
/// key on: the failure keeps the worktree as the worker left it.
#[test]
fn a_failed_worker_is_not_committed() {
    let repo = Repo::new("commit-dirty-failed");
    let store = setup(&repo);
    let plan = add_plan(&store, "commit");
    let task = add_task(&store, &plan, "failed");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = worker(
        &repo,
        "commit-worker-failed",
        r#"{"type":"Result","outcome":"error","text":"Add the image"}"#,
    );
    set_default_flow(&repo, &worker, Some(true));

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished = finished(&repo, &task);
    assert_eq!(finished["outcome"], json!("error"), "{finished}");
    assert_eq!(finished["pushed"], json!(false), "{finished}");
    assert_eq!(
        branch_tip(&repo, &task),
        repo.main_sha(),
        "no commit was made"
    );
    assert_eq!(finished["dirty"], json!(["image.txt"]), "{finished}");
}

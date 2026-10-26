//! A profile's custom `before_attempt` commands (ADR 0048 decisions 7 and 10).
//!
//! Every test drives the real `radian` binary. The worker is the hidden `--worker-command`
//! shell, so no model is needed: a command's file, log, exit code, timeout and process group
//! are observed without a session. The `reverify` case drives the gate and the owner's command
//! against the library.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::*;
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};
use serde_json::{Value, json};

fn init(repo: &Repo) {
    Store::init(&repo.clone, false).expect("arc init");
}

/// Replaces one piece of `config.toml`.
fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// Replaces the default profile's `before_attempt` list with `entries` (TOML).
fn before_attempt(repo: &Repo, entries: &str) {
    tune(
        repo,
        "before_attempt = [\"merge-main\"]",
        &format!("before_attempt = {entries}"),
    );
}

/// One attempt with a test-only worker shell and no model.
fn attempt(repo: &Repo, task: &str, number: u32, token: &str, worker: &str) -> Run {
    let number = number.to_string();
    let args = [
        "--task",
        task,
        "--attempt",
        number.as_str(),
        "--token",
        token,
        "--worker-command",
        worker,
        "--allow-noop",
    ];
    repo.work(&args, "the brief\n")
}

/// Commits `file` on main in a second clone and pushes it, leaving the main clone's
/// `origin/<main>` stale.
fn advance_main(repo: &Repo, file: &str, content: &str) -> String {
    let other = repo.second_clone("other-main");
    repo.git_at(&other, &["config", "user.email", "arc@example.invalid"]);
    repo.git_at(&other, &["config", "user.name", "arc test"]);
    write(&other.join(file), content);
    repo.git_at(&other, &["add", file]);
    repo.git_at(&other, &["commit", "-m", &format!("main: {file}")]);
    repo.git_at(&other, &["push", "origin", &repo.main_branch]);
    repo.git_at(&other, &["rev-parse", "HEAD"])
        .trim()
        .to_owned()
}

/// The merge commits on `work/<task>`, newest first.
fn merge_commits(repo: &Repo, task: &str) -> Vec<String> {
    repo.git(&[
        "log",
        "--merges",
        "--format=%H",
        &format!("refs/heads/work/{task}"),
    ])
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(str::to_owned)
    .collect()
}

fn failure(run: &Run) -> String {
    run.finished()["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("no failure: {run:?}"))
        .to_owned()
}

/// A command runs on a first attempt, in the worktree, with the command worker's `ARC_*`
/// values, and the worker sees the file it left. Mutation TK0169-M1 (skip the commands on a
/// first attempt): the log, `generated.txt` and the worker's copy are all missing.
#[test]
fn a_command_runs_on_a_first_attempt_and_the_worker_sees_its_file() {
    let repo = Repo::new("before-command-first");
    init(&repo);
    let task = "TK0300-first";
    let worktree = repo.worktree(task);
    before_attempt(
        &repo,
        "[{ run = 'echo built > generated.txt && printf \"%s|%s|%s\\n\" \"$ARC_WORKTREE\" \
         \"$ARC_TASK\" \"$ARC_ATTEMPT\" > env.txt && echo logged' }]",
    );
    let seen = repo.root.join("seen.txt");
    let worker = format!(
        "test -f '{worktree}/generated.txt' && cp '{worktree}/generated.txt' '{seen}'",
        worktree = worktree.display(),
        seen = seen.display(),
    );
    let run = attempt(&repo, task, 1, "tok-first", &worker);
    assert_eq!(
        run.code, 1,
        "the test worker writes no Result: {}",
        run.stderr
    );

    assert_eq!(
        fs::read_to_string(worktree.join("generated.txt")).expect("the command's file"),
        "built\n"
    );
    assert_eq!(
        fs::read_to_string(&seen).expect("the worker saw the file"),
        "built\n"
    );
    let env = fs::read_to_string(worktree.join("env.txt")).expect("the command's env file");
    assert_eq!(
        env.trim(),
        format!(
            "{}|{}|{}-1",
            fs::canonicalize(&worktree)
                .expect("the worktree path")
                .display(),
            task,
            task
        )
    );
    let log = repo.run_dir(task, 1).join("before/1.log");
    let logged = fs::read_to_string(&log).expect("the command's log");
    assert!(logged.contains("logged"), "{logged}");
}

/// A failing command ends the attempt before the worker starts, with the entry, the exit code
/// and the log path in `attempt_finished.json`. Mutation TK0169-M2 (start the worker after a
/// failed command): the test worker touches `worker-ran`.
#[test]
fn a_failing_command_ends_the_attempt_before_the_worker() {
    let repo = Repo::new("before-command-failure");
    init(&repo);
    let task = "TK0301-failing";
    before_attempt(&repo, "[{ run = 'echo boom; exit 7' }]");
    let ran = repo.root.join("worker-ran");
    let worker = format!("touch '{}'", ran.display());
    let run = attempt(&repo, task, 1, "tok-fail", &worker);

    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(
        !ran.exists(),
        "the worker started after its before_attempt command failed"
    );
    let text = failure(&run);
    let log = repo.run_dir(task, 1).join("before/1.log");
    assert!(text.contains("before_attempt entry 1"), "{text}");
    assert!(text.contains("exit code 7"), "{text}");
    assert!(text.contains(&log.display().to_string()), "{text}");
    assert!(
        fs::read_to_string(&log).expect("the log").contains("boom"),
        "the command's output is in its log"
    );
    let on_disk: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(task, 1).join("attempt_finished.json"))
            .expect("attempt_finished.json"),
    )
    .expect("attempt_finished.json reads");
    assert_eq!(on_disk["failure"], run.finished()["failure"]);
    assert!(
        !repo.run_dir(task, 1).join("result.jsonl").exists(),
        "no worker ran, so there is no result stream"
    );
}

/// The command's timeout kills its whole process group: a child it started is gone after the
/// attempt ends. Mutation TK0169-M3 (kill only the command's own pid): the `sleep` child
/// survives.
#[test]
fn a_timeout_kills_the_commands_process_group() {
    let repo = Repo::new("before-command-timeout");
    init(&repo);
    let task = "TK0302-timeout";
    before_attempt(
        &repo,
        "[{ run = 'sleep 300 & echo $! > child.pid; wait', timeout_secs = 1 }]",
    );
    let ran = repo.root.join("worker-ran");
    let worker = format!("touch '{}'", ran.display());
    let run = attempt(&repo, task, 1, "tok-timeout", &worker);

    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(!ran.exists(), "the worker started after the timeout");
    let text = failure(&run);
    assert!(text.contains("before_attempt entry 1"), "{text}");
    assert!(text.contains("timed out after 1 s"), "{text}");
    assert!(text.contains("child.pid"), "{text}");

    let child: u32 = fs::read_to_string(repo.worktree(task).join("child.pid"))
        .expect("the child's pid")
        .trim()
        .parse()
        .expect("a pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(child) {
        assert!(
            Instant::now() < deadline,
            "the timeout killed only the command's shell; {child} still sleeps"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `merge-main` and a command run in the order listed on a rework: the merge is already in
/// the worktree when the command runs. The command's log number is its entry position.
#[test]
fn merge_main_then_a_command_run_in_that_order_on_a_rework() {
    let repo = Repo::new("before-command-order");
    init(&repo);
    let task = "TK0303-order";
    let first = attempt(&repo, task, 1, "tok-order-1", "true");
    assert!(
        repo.arc()
            .join("attempts")
            .join(task)
            .join("last_head")
            .is_file(),
        "the first attempt ran: {}",
        first.stderr
    );

    let moved = advance_main(&repo, "main.txt", "main\n");
    before_attempt(
        &repo,
        "[\"merge-main\", { run = 'test -f main.txt && git log --merges -1 --format=%H > \
         merged.txt' }]",
    );
    let second = attempt(&repo, task, 2, "tok-order-2", "true");
    assert_eq!(
        repo.main_sha(),
        moved,
        "merge-main did not fetch: {}",
        second.stderr
    );
    assert_eq!(
        merge_commits(&repo, task).len(),
        1,
        "merge-main did not merge: {}",
        second.stderr
    );
    let merged = fs::read_to_string(repo.worktree(task).join("merged.txt"))
        .expect("the command ran after the merge");
    assert!(!merged.trim().is_empty(), "{merged:?}");
    assert!(
        repo.run_dir(task, 2).join("before/2.log").is_file(),
        "the command's log is its entry's position"
    );
    assert!(
        !repo.run_dir(task, 2).join("before/1.log").exists(),
        "merge-main writes no log"
    );
}

/// A `reverify` attempt runs no worker and no `before_attempt` command: the gate re-runs
/// without the wrapper, so the command never touches the repository.
#[test]
fn a_reverify_runs_no_before_attempt_command() {
    let repo = Repo::new("before-command-reverify");
    init(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task(
            &plan,
            "work",
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let marker = repo.root.join("before-ran");
    tune(&repo, "\ncommands = []", "\ncommands = [\"false\"]");
    before_attempt(
        &repo,
        &format!("[{{ run = 'touch \"{}\"' }}]", marker.display()),
    );

    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("feature.txt"), "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "feature"]);
    repo.git(&["push", "origin", &branch]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.token = Some("token-1".to_owned());
            task.base_sha = Some(base);
            task.head_sha = Some(head);
            Ok(())
        })
        .expect("the task is in review.verifying");

    let failed = review::verify(&store, &task).expect("the failing gate runs");
    assert_eq!(failed.phase, "rework_pending", "{failed:?}");
    assert!(!marker.exists(), "the gate ran a before_attempt command");

    tune(&repo, "commands = [\"false\"]", "commands = [\"true\"]");
    let reverified = repo.radian(&["arc", "reverify", &task]);
    assert_eq!(
        reverified.code, 0,
        "reverify: {}\n{}",
        reverified.stdout, reverified.stderr
    );
    assert!(
        !marker.exists(),
        "a reverify attempt ran a before_attempt command"
    );
    assert!(
        !repo.worktree(&task).exists(),
        "a reverify attempt created the worker's worktree"
    );
    let now = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (now.state.as_str(), now.phase.as_str()),
        ("review", "awaiting_human")
    );
}

/// `before_attempt = ["merge-main"]` alone behaves exactly as before: no command runs, no log
/// directory exists, and a rework merges the moved main.
#[test]
fn merge_main_alone_behaves_as_before() {
    let repo = Repo::new("before-command-merge-only");
    init(&repo);
    let task = "TK0304-merge-only";
    let first = attempt(&repo, task, 1, "tok-only-1", "true");
    assert_eq!(first.code, 1, "{}", first.stderr);
    assert!(
        merge_commits(&repo, task).is_empty(),
        "a first attempt merged"
    );
    assert!(
        !repo.run_dir(task, 1).join("before").exists(),
        "merge-main writes no log"
    );

    advance_main(&repo, "main.txt", "main\n");
    let second = attempt(&repo, task, 2, "tok-only-2", "true");
    assert_eq!(
        merge_commits(&repo, task).len(),
        1,
        "the rework did not merge main: {}",
        second.stderr
    );
    assert!(
        !repo.run_dir(task, 2).join("before").exists(),
        "merge-main writes no log"
    );
}

/// `show` prints the reason an attempt failed before the gate: a `before_attempt` command's
/// failure is an infrastructure failure, so the task is not in `review` and has no
/// `rework_reason`.
#[test]
fn show_prints_a_before_attempt_failure() {
    let repo = Repo::new("before-command-show");
    init(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task(
            &plan,
            "work",
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    store
        .change_task(&task, |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.token = Some("token-1".to_owned());
            Ok(())
        })
        .expect("the task is doing");
    let reason = "before_attempt entry 1 failed with exit code 7: exit 7; log: /tmp/before/1.log";
    let dir = repo.run_dir(&task, 1);
    fs::create_dir_all(&dir).expect("the attempt directory");
    fs::write(
        dir.join("attempt_finished.json"),
        serde_json::to_string(&json!({
            "type": "AttemptFinished",
            "attempt_id": format!("{}-1", task),
            "token": "token-1",
            "exit_code": 1,
            "outcome": "terminated",
            "failure": reason,
            "notes": [],
        }))
        .expect("the record serializes"),
    )
    .expect("attempt_finished.json");

    let text = radian_arc::store::cli::show_text(&store, &task).expect("show");
    assert!(
        text.contains(&format!("attempt_failure: {reason}")),
        "{text}"
    );
    let value = radian_arc::store::cli::show_json(&store, &task).expect("show --json");
    assert_eq!(value["attempt_failure"], json!(reason));
}

/// A command that changes the repository's git control files fails the attempt through the
/// control-file check, before the worker starts, and the wrapper puts the file back.
#[test]
fn a_command_that_edits_git_config_fails_the_control_check() {
    let repo = Repo::new("before-command-git-config");
    init(&repo);
    let task = "TK0305-git-config";
    before_attempt(&repo, "[{ run = 'git config user.name hacked' }]");
    let ran = repo.root.join("worker-ran");
    let worker = format!("touch '{}'", ran.display());
    let run = attempt(&repo, task, 1, "tok-control", &worker);

    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(
        !ran.exists(),
        "the worker started although a control file changed"
    );
    let text = failure(&run);
    assert!(text.contains("changed git control files"), "{text}");
    assert!(text.contains("config"), "{text}");
    let config = fs::read_to_string(repo.clone.join(".git/config")).expect(".git/config");
    assert!(
        !config.contains("hacked"),
        "the wrapper restored the control file: {config}"
    );
}

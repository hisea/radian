//! `[hooks]`: the commands arc runs at fixed points (ADR 0048 decisions 10 and 13).
//!
//! Each test starts from a real repository and the `radian arc` binary. A task is put in
//! `review.awaiting_human` the way verification leaves it, its worktree is created the way the
//! wrapper creates it, and the merge is driven through `radian arc merge` or the library. The
//! hooks are then read from their log files, their environment and their effect on git.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::*;
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};

/// A repository, its store, and a confirmed plan.
fn setup(label: &str) -> (Repo, Store, String) {
    let repo = Repo::new(label);
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    (repo, store, plan)
}

/// Adds a task, publishes its `work/<task>` branch, verifies it, and creates the worktree the
/// wrapper would have created. Returns the task id, its head and its worktree.
fn prepare_task(repo: &Repo, store: &Store, plan: &str, slug: &str) -> (String, String, PathBuf) {
    let task = store
        .add_task(
            plan,
            slug,
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task");
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    let file = format!("{slug}.txt");
    fs::write(repo.clone.join(&file), "feature\n").expect("the task's file");
    repo.git(&["add", &file]);
    repo.git(&["commit", "-m", &file]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", &repo.remote, &branch]);
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .expect("the task is in review.verifying");
    let verified = review::verify(store, &task).expect("verification runs");
    assert_eq!(verified.phase, "awaiting_human");

    repo.git(&["checkout", &repo.main_branch]);
    let worktree = repo.worktree(&task);
    fs::create_dir_all(worktree.parent().expect("the worktrees directory"))
        .expect("the worktrees directory");
    repo.git(&[
        "worktree",
        "add",
        worktree.to_str().expect("a utf-8 path"),
        &branch,
    ]);
    (task, head, worktree)
}

/// Replaces the `[hooks]` section `init` wrote. The section sits between `[integration]` and
/// `[gate]`.
fn set_after_merge(repo: &Repo, lines: &[&str]) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    let start = text.find("[hooks]\n").expect("init writes [hooks]");
    let rest = &text[start..];
    let end = rest.find("\n[gate]").expect("[hooks] precedes [gate]");
    let block = if lines.is_empty() {
        "[hooks]\nafter_merge = []\n".to_owned()
    } else {
        let mut block = String::from("[hooks]\nafter_merge = [\n");
        for line in lines {
            let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
            block.push_str(&format!("  \"{escaped}\",\n"));
        }
        block.push_str("]\n");
        block
    };
    fs::write(
        &path,
        format!("{}{}{}", &text[..start], block, &rest[end..]),
    )
    .expect("write config.toml");
}

/// Sets `[gate] command_timeout_secs` to `seconds`.
fn command_timeout(repo: &Repo, seconds: u64) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(
        text.contains("command_timeout_secs = 3600"),
        "the default timeout is in the file"
    );
    fs::write(
        &path,
        text.replacen(
            "command_timeout_secs = 3600",
            &format!("command_timeout_secs = {seconds}"),
            1,
        ),
    )
    .expect("write config.toml");
}

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

fn pending(store: &Store, task: &str) -> Vec<String> {
    store
        .load_task(task)
        .expect("the task reads")
        .item
        .pending_hooks
}

/// `ARC_*` values from an `env` dump: `name=value`.
fn env_map(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    fs::read_to_string(path)
        .expect("the hook wrote its environment")
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// The default `[hooks] after_merge` lines remove the remote branch, the worktree and the local
/// branch, and every hook variable reaches the command.
#[test]
fn the_default_after_merge_removes_the_branches_and_the_worktree_and_passes_every_variable() {
    let (repo, store, plan) = setup("hooks-default");
    let (task, head, worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let canonical = fs::canonicalize(&worktree).expect("the worktree canonicalizes");
    // The three lines `init` writes, plus a dump of the environment the command received.
    set_after_merge(
        &repo,
        &[
            "git push origin --delete work/$ARC_TASK",
            "git worktree remove --force $ARC_WORKTREE",
            "git branch -D work/$ARC_TASK",
            "env > hook-env.txt",
        ],
    );

    let run = repo.radian(&["arc", "merge", &task]);
    assert_eq!(run.code, 0, "merge: {}\n{}", run.stderr, run.stdout);
    assert!(run.stdout.contains("done.merged"), "{}", run.stdout);
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
    assert_eq!(repo.remote_branch(&task), None, "the remote branch is gone");
    assert!(!worktree.exists(), "the worktree is gone");
    assert_eq!(repo.local_branch(&task), None, "the local branch is gone");
    assert!(
        pending(&store, &task).is_empty(),
        "the hook record is cleared"
    );

    let env = env_map(&repo.clone.join("hook-env.txt"));
    for name in radian_arc::hooks::HOOK_ENV {
        assert!(
            env.contains_key(*name),
            "`{name}` did not reach the hook: {env:?}"
        );
    }
    let arc_names: std::collections::BTreeSet<&str> = env
        .keys()
        .map(String::as_str)
        .filter(|name| name.starts_with("ARC_"))
        .collect();
    assert_eq!(
        arc_names,
        radian_arc::hooks::HOOK_ENV.iter().copied().collect(),
        "the hook sets exactly the listed variables"
    );
    assert_eq!(env["ARC_TASK"], task);
    assert_eq!(env["ARC_PLAN"], plan);
    assert_eq!(env["ARC_ATTEMPT"], format!("{task}-1"));
    assert_eq!(env["ARC_BRANCH"], format!("work/{task}"));
    assert_eq!(
        env["ARC_WORKTREE"],
        canonical.display().to_string(),
        "ARC_WORKTREE is the absolute worktree path"
    );
    assert_eq!(env["ARC_TARGET_BRANCH"], repo.main_branch);
    assert_eq!(env["ARC_MERGED_SHA"], head);
    assert_eq!(env["ARC_STATE"], "done.merged");
    assert_eq!(env["ARC_HOOK"], "after_merge");
}

/// `after_merge = []` runs nothing: the merge leaves the worktree and both branches in place,
/// and no hard-coded removal is left.
#[test]
fn an_empty_after_merge_keeps_the_worktree_and_the_branches() {
    let (repo, store, plan) = setup("hooks-empty");
    let (task, head, worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_after_merge(&repo, &[]);

    let run = repo.radian(&["arc", "merge", &task]);
    assert_eq!(run.code, 0, "merge: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
    assert!(worktree.exists(), "nothing removed the worktree");
    assert_eq!(repo.remote_branch(&task), Some(head.clone()));
    assert_eq!(repo.local_branch(&task), Some(head));
    assert!(
        pending(&store, &task).is_empty(),
        "the empty hook is cleared"
    );
    let logs = store.root().join("runs").join(&task).join("hooks");
    assert!(
        !logs.exists()
            || fs::read_dir(&logs)
                .expect("the hooks directory")
                .next()
                .is_none(),
        "an empty list wrote no command log"
    );
}

/// A failing command logs one `warning:` line, and the next command still runs. The task stays
/// `done.merged`.
#[test]
fn a_failing_hook_command_warns_and_the_next_command_runs() {
    let (repo, store, plan) = setup("hooks-failing");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_after_merge(&repo, &["exit 7", "touch second-ran"]);

    let run = repo.radian(&["arc", "merge", &task]);
    assert_eq!(run.code, 0, "merge: {}\n{}", run.stderr, run.stdout);
    assert!(
        run.stdout.contains("warning:"),
        "the failure is a warning line:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("exited 7"),
        "the warning names the exit code:\n{}",
        run.stdout
    );
    assert!(
        repo.clone.join("second-ran").is_file(),
        "the second command ran"
    );
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
    assert!(
        store
            .root()
            .join("runs")
            .join(&task)
            .join("hooks")
            .join("after_merge-01.log")
            .is_file(),
        "the failing command has its log"
    );
}

/// A process killed while a hook runs leaves the point pending; the next pass runs it again
/// from the start. The first command kills its parent pass once, so the second pass finishes.
#[cfg(unix)]
#[test]
fn a_hook_killed_mid_run_is_run_again_on_the_next_pass() {
    let (repo, store, plan) = setup("hooks-killed");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_after_merge(
        &repo,
        &[
            "if [ ! -f killed-flag ]; then touch killed-flag; kill -9 $PPID; fi",
            "echo ran >> hook-runs.txt",
        ],
    );
    review::merge(&store, &task).expect("the merge runs");

    let killed = repo.radian(&["arc", "run"]);
    assert_ne!(killed.code, 0, "the first pass was killed: {killed:?}");
    assert!(
        !repo.clone.join("hook-runs.txt").exists(),
        "the first pass died before the second command"
    );
    assert_eq!(
        pending(&store, &task),
        vec!["after_merge".to_owned()],
        "the pending point survived the killed pass"
    );

    let second = repo.radian(&["arc", "run"]);
    assert_eq!(
        second.code, 0,
        "second pass: {}\n{}",
        second.stderr, second.stdout
    );
    assert_eq!(
        fs::read_to_string(repo.clone.join("hook-runs.txt"))
            .expect("the second pass ran the hook")
            .lines()
            .count(),
        1,
        "the hook ran exactly once after the killed pass"
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
}

/// While a loop holds `runner.lock`, the CLI leaves the hook pending and exits; the next pass
/// runs it once and clears it.
#[test]
fn the_cli_leaves_the_hook_pending_while_a_loop_holds_runner_lock() {
    let (repo, store, plan) = setup("hooks-cli-locked");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_after_merge(&repo, &["echo ran >> hook-runs.txt"]);

    let lock = store
        .try_runner_lock()
        .expect("try the lock")
        .expect("runner.lock is free");
    let run = repo.radian(&["arc", "merge", &task]);
    assert_eq!(run.code, 0, "merge: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
    assert_eq!(
        pending(&store, &task),
        vec!["after_merge".to_owned()],
        "the CLI left the hook for the loop"
    );
    assert!(
        !repo.clone.join("hook-runs.txt").exists(),
        "the CLI ran a hook while the loop held the lock"
    );

    drop(lock);
    let pass = repo.radian(&["arc", "run"]);
    assert_eq!(pass.code, 0, "pass: {}\n{}", pass.stderr, pass.stdout);
    assert_eq!(
        fs::read_to_string(repo.clone.join("hook-runs.txt"))
            .expect("the pass ran the hook")
            .lines()
            .count(),
        1
    );
    assert!(
        pending(&store, &task).is_empty(),
        "the pass cleared the record"
    );

    let again = repo.radian(&["arc", "run"]);
    assert_eq!(again.code, 0, "second pass: {}", again.stderr);
    assert_eq!(
        fs::read_to_string(repo.clone.join("hook-runs.txt"))
            .expect("the hook's file")
            .lines()
            .count(),
        1,
        "the hook ran exactly once"
    );
}

/// A hook that hangs is killed at `[gate] command_timeout_secs`, logs a warning, and the point
/// is cleared; the next command runs.
#[test]
fn a_hanging_hook_is_killed_at_the_command_timeout() {
    let (repo, store, plan) = setup("hooks-timeout");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    command_timeout(&repo, 1);
    set_after_merge(&repo, &["sleep 30", "touch after-timeout"]);

    let started = Instant::now();
    let run = repo.radian(&["arc", "merge", &task]);
    let elapsed = started.elapsed();
    assert_eq!(run.code, 0, "merge: {}\n{}", run.stderr, run.stdout);
    assert!(
        elapsed < Duration::from_secs(20),
        "the hanging command was not killed: {elapsed:?}"
    );
    assert!(
        run.stdout.contains("warning:") && run.stdout.contains("timed out after 1 s"),
        "the timeout is a warning line:\n{}",
        run.stdout
    );
    assert!(
        repo.clone.join("after-timeout").is_file(),
        "the command after the timeout ran"
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
}

/// A task record written before `pending_hooks` existed has no field, loads, and runs nothing.
#[test]
fn a_task_record_without_the_pending_hook_field_loads_and_runs_nothing() {
    let (repo, store, plan) = setup("hooks-old-record");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_after_merge(&repo, &["touch should-not-run"]);
    store
        .change_task(&task, |task| {
            task.state = TaskState::Done;
            task.phase = "merged".to_owned();
            task.pending_hooks.clear();
            task.merged_sha = None;
            Ok(())
        })
        .expect("the task is done.merged");
    let path = store.root().join("tasks").join(format!("{task}.md"));
    let text = fs::read_to_string(&path).expect("the task file");
    assert!(!text.contains("pending_hooks:"), "{text}");
    assert!(!text.contains("merged_sha:"), "{text}");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert!(
        !repo.clone.join("should-not-run").exists(),
        "a task without a pending point ran a hook"
    );
    assert!(pending(&store, &task).is_empty());
    assert_eq!(
        state(&store, &task),
        ("done".to_owned(), "merged".to_owned())
    );
}

/// Two pending hooks run in one pass, each once, with its own variables.
#[test]
fn two_pending_hooks_run_once_each_with_their_own_variables() {
    let (repo, store, plan) = setup("hooks-two");
    let (first, first_head, _first_tree) = prepare_task(&repo, &store, &plan, "first");
    let (second, second_head, _second_tree) = prepare_task(&repo, &store, &plan, "second");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_after_merge(
        &repo,
        &["printf '%s %s %s\\n' \"$ARC_TASK\" \"$ARC_MERGED_SHA\" \"$ARC_BRANCH\" >> hooks.txt"],
    );
    review::merge(&store, &first).expect("the first merge runs");
    review::merge(&store, &second).expect("the second merge runs");
    // Main moved under the second merge, so its candidate is a merge commit, not its head.
    let second_merged = repo.remote_main_sha();
    assert_ne!(
        second_merged, second_head,
        "the second merge built a candidate"
    );
    assert_eq!(pending(&store, &first), vec!["after_merge".to_owned()]);
    assert_eq!(pending(&store, &second), vec!["after_merge".to_owned()]);

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    let mut lines: Vec<String> = fs::read_to_string(repo.clone.join("hooks.txt"))
        .expect("the hooks wrote their lines")
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    let mut expected = vec![
        format!("{first} {first_head} work/{first}"),
        format!("{second} {second_merged} work/{second}"),
    ];
    expected.sort();
    assert_eq!(lines, expected);
    assert!(pending(&store, &first).is_empty());
    assert!(pending(&store, &second).is_empty());
}

/// The manual names every environment variable and every point the code has.
#[test]
fn the_manual_names_every_hook_variable_and_point() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let page = fs::read_to_string(manifest.join("../../docs/user/arc/index.md"))
        .expect("docs/user/arc/index.md");
    for name in radian_arc::hooks::HOOK_ENV {
        assert!(
            page.contains(name),
            "docs/user/arc/index.md does not name `{name}`"
        );
    }
    for point in radian_arc::hooks::HOOK_POINTS {
        assert!(
            page.contains(point),
            "docs/user/arc/index.md does not name the point `{point}`"
        );
    }
}

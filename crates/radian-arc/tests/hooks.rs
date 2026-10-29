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
use radian_arc::mcp::ArcServer;
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

/// Replaces the `[hooks]` section `init` wrote with `points`, each a point name and its
/// command lines. The section sits between `[integration]` and `[gate]`.
fn set_hooks(repo: &Repo, points: &[(&str, &[&str])]) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    let start = text.find("[hooks]\n").expect("init writes [hooks]");
    let rest = &text[start..];
    let end = rest.find("\n[gate]").expect("[hooks] precedes [gate]");
    let mut block = String::from("[hooks]\n");
    for (point, lines) in points {
        if lines.is_empty() {
            block.push_str(&format!("{point} = []\n"));
            continue;
        }
        block.push_str(&format!("{point} = [\n"));
        for line in *lines {
            let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
            block.push_str(&format!("  \"{escaped}\",\n"));
        }
        block.push_str("]\n");
    }
    fs::write(
        &path,
        format!("{}{}{}", &text[..start], block, &rest[end..]),
    )
    .expect("write config.toml");
}

/// [`set_hooks`] for the one `after_merge` point.
fn set_after_merge(repo: &Repo, lines: &[&str]) {
    set_hooks(repo, &[("after_merge", lines)]);
}

/// Adds a task in `todo.ready` and returns its id.
fn add_ready_task(store: &Store, plan: &str, slug: &str) -> String {
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

/// Replaces one line of `config.toml` with another, asserting the old line is there.
fn set_config_line(repo: &Repo, old: &str, new: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(old), "config.toml has `{old}`:\n{text}");
    fs::write(&path, text.replacen(old, new, 1)).expect("write config.toml");
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

fn pending_states(store: &Store, task: &str) -> Vec<String> {
    store
        .load_task(task)
        .expect("the task reads")
        .item
        .pending_hook_states
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
    // The verification that prepared the task owes `after_awaiting_human`; run it (it has no
    // command here) so this test sees only the merge's point.
    let cleared = repo.radian(&["arc", "run"]);
    assert_eq!(cleared.code, 0, "run: {}", cleared.stderr);
    assert!(pending(&store, &task).is_empty());

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
            task.pending_hook_states.clear();
            task.merged_sha = None;
            Ok(())
        })
        .expect("the task is done.merged");
    let path = store.root().join("tasks").join(format!("{task}.md"));
    let text = fs::read_to_string(&path).expect("the task file");
    assert!(!text.contains("pending_hooks:"), "{text}");
    assert!(!text.contains("pending_hook_states:"), "{text}");
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
    // Each verification owes `after_awaiting_human`; run it (it has no command here) so the
    // pending lists hold only the merge's point.
    let cleared = repo.radian(&["arc", "run"]);
    assert_eq!(cleared.code, 0, "run: {}", cleared.stderr);
    assert!(pending(&store, &first).is_empty());
    assert!(pending(&store, &second).is_empty());
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

/// The line every new point test writes: the point and the state it saw, one per run.
const HOOK_LINE: &str = "printf '%s %s\\n' \"$ARC_HOOK\" \"$ARC_STATE\" >> hooks.txt";

fn hook_lines(repo: &Repo) -> Vec<String> {
    fs::read_to_string(repo.clone.join("hooks.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `after_fail` fires once after a task enters `failed.cancelled`, with `ARC_HOOK` and
/// `ARC_STATE` naming the point and the phase. The owner's `cancel` runs it itself, since no
/// loop holds `runner.lock`.
#[test]
fn after_fail_fires_once_with_its_point_and_state() {
    let (repo, store, plan) = setup("hooks-after-fail");
    let task = add_ready_task(&store, &plan, "fail");
    set_hooks(
        &repo,
        &[("after_fail", &[HOOK_LINE, "env > after-fail-env.txt"])],
    );

    let run = repo.radian(&["arc", "cancel", &task]);
    assert_eq!(run.code, 0, "cancel: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        state(&store, &task),
        ("failed".to_owned(), "cancelled".to_owned())
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
    assert_eq!(hook_lines(&repo), vec!["after_fail failed.cancelled"]);
    let env = env_map(&repo.clone.join("after-fail-env.txt"));
    assert!(
        !env.contains_key("ARC_MERGED_SHA"),
        "only `after_merge` receives ARC_MERGED_SHA"
    );
}

/// A task that entered `failed.exhausted`, then was cancelled before a pass ran its hook, still
/// reports `failed.exhausted`: the record keeps the phase the transition entered with the point,
/// not the later `failed.cancelled`.
#[test]
fn after_fail_reports_the_failed_phase_it_entered_when_the_owner_cancels_it_later() {
    let (repo, store, plan) = setup("hooks-after-fail-cancelled");
    let task = add_ready_task(&store, &plan, "exhausted");
    store
        .change_task(&task, |task| {
            task.enter(TaskState::Failed, "exhausted");
            Ok(())
        })
        .expect("the task is failed.exhausted");
    assert_eq!(
        state(&store, &task),
        ("failed".to_owned(), "exhausted".to_owned())
    );
    assert!(
        pending(&store, &task).contains(&"after_fail".to_owned()),
        "the failed phase owes the point: {:?}",
        pending(&store, &task)
    );
    set_hooks(&repo, &[("after_fail", &[HOOK_LINE])]);

    let run = repo.radian(&["arc", "cancel", &task]);
    assert_eq!(run.code, 0, "cancel: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        state(&store, &task),
        ("failed".to_owned(), "cancelled".to_owned())
    );
    assert_eq!(
        hook_lines(&repo),
        vec!["after_fail failed.exhausted"],
        "ARC_STATE is the phase the transition entered, not the later cancel"
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
}

/// `after_rework` fires once after a failed verification enters `review.rework_pending`, with
/// the point and the phase. The library writes the record; the next pass runs it.
#[test]
fn after_rework_fires_once_with_its_point_and_state() {
    let (repo, store, plan) = setup("hooks-after-rework");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    set_hooks(&repo, &[("after_rework", &[HOOK_LINE])]);

    review::reject(&store, &task, "the diff does not match the brief").expect("reject runs");
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "rework_pending".to_owned())
    );
    assert!(
        pending(&store, &task).contains(&"after_rework".to_owned()),
        "the transition owes the point: {:?}",
        pending(&store, &task)
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_rework review.rework_pending"]
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
}

/// `after_awaiting_human` fires once after verification enters `review.awaiting_human`, with
/// the point and the phase.
#[test]
fn after_awaiting_human_fires_once_with_its_point_and_state() {
    let (repo, store, plan) = setup("hooks-after-awaiting");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    set_hooks(&repo, &[("after_awaiting_human", &[HOOK_LINE])]);
    assert_eq!(
        pending(&store, &task),
        vec!["after_awaiting_human".to_owned()]
    );
    assert_eq!(
        pending_states(&store, &task),
        vec!["after_awaiting_human=review.awaiting_human".to_owned()],
        "the write records the phase the point fires for with it"
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_awaiting_human review.awaiting_human"]
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
    assert!(
        pending_states(&store, &task).is_empty(),
        "the state record is cleared with the point"
    );
}

/// A point that runs after the task moved on still reports the phase it fired for: the
/// `after_awaiting_human` hook runs while the task is already `review.rework_pending`.
#[test]
fn a_hook_that_runs_after_the_task_moved_reports_the_phase_it_fired_for() {
    let (repo, store, plan) = setup("hooks-moved");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    set_hooks(&repo, &[("after_awaiting_human", &[HOOK_LINE])]);
    review::reject(&store, &task, "the diff does not match the brief").expect("reject runs");
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "rework_pending".to_owned())
    );
    assert_eq!(
        pending(&store, &task),
        vec!["after_awaiting_human".to_owned(), "after_rework".to_owned()]
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_awaiting_human review.awaiting_human"],
        "ARC_STATE is the phase the point fired for, not the task's later phase"
    );
}

/// A task added through the MCP `add_task` tool records `after_proposed` but runs nothing; the
/// next serve pass runs it once, with `todo.proposed`.
#[test]
fn after_proposed_runs_on_the_next_pass_not_in_the_mcp_call() {
    let (repo, store, plan) = setup("hooks-after-proposed");
    add_ready_task(&store, &plan, "first");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_hooks(&repo, &[("after_proposed", &[HOOK_LINE])]);
    let server = ArcServer::new(store.clone());

    let result = server.call(
        "add_task",
        serde_json::json!({
            "plan": plan,
            "slug": "second",
            "title": "A proposed task",
            "brief": "the brief\n",
        })
        .as_object()
        .cloned(),
    );
    assert_eq!(
        result.is_error,
        Some(false),
        "add_task succeeds: {result:?}"
    );
    let task = store
        .task_ids()
        .expect("the task ids")
        .into_iter()
        .find(|id| id.ends_with("-second"))
        .expect("the new task");
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "proposed".to_owned())
    );
    assert_eq!(pending(&store, &task), vec!["after_proposed".to_owned()]);
    assert!(
        !repo.clone.join("hooks.txt").exists(),
        "the MCP call ran a hook"
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(hook_lines(&repo), vec!["after_proposed todo.proposed"]);
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
}

/// `radian arc cancel <plan>` ends three open tasks and records one `after_fail` per task, so
/// the commands run three times, each with its own task.
#[test]
fn a_plan_cancel_runs_after_fail_once_for_each_open_task() {
    let (repo, store, plan) = setup("hooks-plan-cancel");
    let tasks = [
        add_ready_task(&store, &plan, "one"),
        add_ready_task(&store, &plan, "two"),
        add_ready_task(&store, &plan, "three"),
    ];
    set_hooks(
        &repo,
        &[(
            "after_fail",
            &["printf '%s %s %s\\n' \"$ARC_HOOK\" \"$ARC_STATE\" \"$ARC_TASK\" >> hooks.txt"],
        )],
    );

    let run = repo.radian(&["arc", "cancel", &plan]);
    assert_eq!(run.code, 0, "cancel: {}\n{}", run.stderr, run.stdout);
    for task in &tasks {
        assert_eq!(
            state(&store, task),
            ("failed".to_owned(), "cancelled".to_owned())
        );
        assert!(pending(&store, task).is_empty(), "the record is cleared");
    }
    let lines = hook_lines(&repo);
    assert_eq!(lines.len(), 3, "one run per task: {lines:?}");
    for task in &tasks {
        assert!(
            lines
                .iter()
                .any(|line| line == &format!("after_fail failed.cancelled {task}")),
            "no hook ran for `{task}`: {lines:?}"
        );
    }
}

/// A conflict rework arc writes itself returns the task to `todo.ready` and owes no
/// `after_rework`; one that stops in `review.rework_pending` owes exactly one.
#[test]
fn a_conflict_to_ready_owes_no_rework_hook_and_one_to_rework_pending_owes_one() {
    let (repo, store, plan) = setup("hooks-conflict");
    let (ready, ready_phase) = prepare_conflict(&repo, &store, &plan, "auto");
    assert_eq!(
        ready_phase, "ready",
        "the conflict rework returns to todo.ready"
    );
    assert!(
        !pending(&store, &ready).contains(&"after_rework".to_owned()),
        "todo.ready owes no after_rework: {:?}",
        pending(&store, &ready)
    );

    // The same conflict with the limit at 0 stops in `review.rework_pending` for the owner.
    set_config_line(&repo, "max_conflict_rework = 3", "max_conflict_rework = 0");
    let (stopped, stopped_phase) = prepare_conflict(&repo, &store, &plan, "stopped");
    assert_eq!(stopped_phase, "rework_pending");
    assert_eq!(pending(&store, &stopped), vec!["after_rework".to_owned()]);

    set_hooks(&repo, &[("after_rework", &[HOOK_LINE])]);
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_rework review.rework_pending"]
    );
    assert!(pending(&store, &ready).is_empty());
    assert!(pending(&store, &stopped).is_empty());
}

/// A second write of the phase the task is already in is not a new entry: it records no
/// point, so the hook runs once for the entry that moved the task there. A second `reject` is
/// refused by the state rule, so the direct write is the case the helper guards.
#[test]
fn a_second_write_of_the_same_phase_does_not_owe_a_second_hook() {
    let (repo, store, plan) = setup("hooks-same-phase");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    set_hooks(&repo, &[("after_rework", &[HOOK_LINE])]);
    review::reject(&store, &task, "fix it").expect("the first reject runs");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_rework review.rework_pending"]
    );

    let again = review::reject(&store, &task, "fix it again");
    assert!(again.is_err(), "a second reject is refused: {again:?}");
    store
        .change_task(&task, |task| {
            task.enter(TaskState::Review, "rework_pending");
            Ok(())
        })
        .expect("the same phase is written again");
    assert!(
        pending(&store, &task).is_empty(),
        "a repeated write of the same phase owes nothing: {:?}",
        pending(&store, &task)
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_rework review.rework_pending"],
        "the hook did not run a second time"
    );
}

/// With no loop running, `radian arc reject` runs the `after_rework` hook in its own process,
/// before it prints.
#[test]
fn the_cli_reject_runs_its_hook_when_no_loop_holds_runner_lock() {
    let (repo, store, plan) = setup("hooks-cli-reject");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    set_hooks(&repo, &[("after_rework", &[HOOK_LINE])]);

    let run = repo.radian(&["arc", "reject", &task, "--note", "fix the diff"]);
    assert_eq!(run.code, 0, "reject: {}\n{}", run.stderr, run.stdout);
    assert_eq!(
        hook_lines(&repo),
        vec!["after_rework review.rework_pending"]
    );
    assert!(pending(&store, &task).is_empty(), "the record is cleared");
    assert!(
        !run.stdout.contains("warning:"),
        "the hook failed: {}",
        run.stdout
    );
}

/// The manual's phone notification sends only the repository name, the task id and the phase.
/// A fake HTTP server receives the payload; nothing else from the task reaches it.
#[test]
fn the_phone_notification_example_sends_only_the_repo_task_and_phase() {
    let (repo, store, plan) = setup("hooks-notify");
    let (task, _head, _worktree) = prepare_task(&repo, &store, &plan, "feature");
    let (url, received) = fake_notify_server();
    let line = format!(
        "curl -s -m 5 -X POST {url} -H 'Content-Type: application/json' -d \
         \"{{\\\"repo\\\":\\\"\"$(basename \"$PWD\")\"\\\",\\\"task\\\":\\\"$ARC_TASK\\\",\
         \\\"phase\\\":\\\"$ARC_STATE\\\"}}\""
    );
    set_hooks(&repo, &[("after_awaiting_human", &[line.as_str()])]);

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
    let body = received
        .recv_timeout(Duration::from_secs(15))
        .expect("the fake server received the notification");
    let value: serde_json::Value = serde_json::from_str(&body).expect("the payload is JSON");
    let object = value.as_object().expect("the payload is an object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["phase", "repo", "task"],
        "the payload carries only the three fields: {body}"
    );
    assert_eq!(value["repo"], "clone");
    assert_eq!(value["task"], task.as_str());
    assert_eq!(value["phase"], "review.awaiting_human");
}

/// A fake HTTP server for one POST: it returns its address and a channel that carries the
/// request body. The thread ends after one request; a test that never receives fails on its
/// own timeout instead of hanging the process.
fn fake_notify_server() -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the fake server");
    let addr = listener.local_addr().expect("the fake server's address");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut body = String::new();
        loop {
            let read = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            buffer.extend_from_slice(&chunk[..read]);
            let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let start = end + 4;
            if buffer.len() >= start + length {
                body = String::from_utf8_lossy(&buffer[start..start + length]).into_owned();
                break;
            }
        }
        let _ =
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = sender.send(body);
    });
    (format!("http://{addr}/notify"), receiver)
}

/// Publishes a task branch that conflicts with a moved main, puts it in `review.verifying`,
/// and verifies it. Returns the task id and the phase verification left it in.
fn prepare_conflict(repo: &Repo, store: &Store, plan: &str, slug: &str) -> (String, String) {
    let task = add_ready_task(store, plan, slug);
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    let file = format!("{slug}.txt");
    fs::write(repo.clone.join(&file), "work side\n").expect("the task's file");
    repo.git(&["add", &file]);
    repo.git(&["commit", "-m", &file]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", &repo.remote, &branch]);
    // Main moves on the same file, so merging it into the task's branch conflicts.
    repo.git(&["checkout", &repo.main_branch]);
    fs::write(repo.clone.join(&file), "main side\n").expect("main's file");
    repo.git(&["add", &file]);
    repo.git(&["commit", "-m", "main side"]);
    repo.git(&["push", &repo.remote, &repo.main_branch]);
    store
        .change_task(&task, |task| {
            task.enter(TaskState::Review, "verifying");
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .expect("the task is in review.verifying");
    let verified = review::verify(store, &task).expect("verification runs");
    (task, verified.phase)
}

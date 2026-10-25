//! `[worker] command`: another program as the worker (ADR 0048 decision 7).
//!
//! Every test configures a small executable script as `[worker] command` and drives the real
//! `radian arc start` / `radian arc run`, so the claim, the wrapper, the environment, the
//! stdout contract and the cost rule are exercised end to end.

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

/// Sets `[worker] command` to `words`.
fn set_command(repo: &Repo, words: &[&str]) {
    let list = words
        .iter()
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>()
        .join(", ");
    tune(
        repo,
        "[worker]\n",
        &format!("[worker]\ncommand = [{list}]\n"),
    );
}

/// Writes an executable script and returns its path.
fn script(repo: &Repo, name: &str, body: &str) -> PathBuf {
    let path = repo.root.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).expect("the script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).expect("the script").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("make it executable");
    }
    path
}

/// The script's shared opening: count the attempt in `ARC_STATE_DIR`, record the brief it got
/// on stdin, and commit one file in the worktree. The body that follows prints the `Result`.
const PROLOGUE: &str = "\
set -e
n=$(cat \"$ARC_STATE_DIR/n\" 2>/dev/null || printf '0')
n=$((n+1))
printf '%s\\n' \"$n\" > \"$ARC_STATE_DIR/n\"
cat > \"$ARC_STATE_DIR/brief-$n\"
echo \"change $n\" > \"file-$n.txt\"
git add \"file-$n.txt\"
git commit -m \"change $n\" >/dev/null
";

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A command worker that commits and prints a completed `Result` passes, and its `text` is the
/// next attempt's hand-off. Mutation: ignore the `text` and the hand-off carries the "wrote no
/// final report" line.
#[test]
fn a_command_worker_that_commits_and_completes_passes_and_its_text_reaches_the_hand_off() {
    let repo = Repo::new("command-pass");
    let store = setup(&repo);
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "pass");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker",
        &format!(
            "{PROLOGUE}printf '{{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"report \
             %s\"}}\\n' \"$n\"\n"
        ),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let first = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(first.code, 0, "start failed: {}", first.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        first.stdout
    );
    let finished: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("attempt_finished.json")))
            .expect("attempt_finished.json");
    assert_eq!(finished["pushed"], json!(true), "{finished}");
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(
        repo.remote_branch(&task).as_deref(),
        finished["head_sha"].as_str(),
        "the command's commit reached the remote"
    );
    let execution: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("execution.json")))
            .expect("execution.json");
    assert_eq!(
        execution["worker"],
        json!({"kind": "command", "program": worker.to_str().unwrap()}),
        "the attempt records which worker ran"
    );

    // A rework runs a second fresh attempt: its stdin carries the first attempt's report.
    plan_ops::add_rework(&store, &task, "one more round").expect("the rework");
    let second = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(second.code, 0, "second start failed: {}", second.stderr);
    let brief = read(
        &repo
            .arc()
            .join("attempts")
            .join(&task)
            .join("state/brief-2"),
    );
    assert!(
        brief.contains("report 1"),
        "the hand-off carries the command's text:\n{brief}"
    );
    let headoff = brief
        .find("## Where the previous attempt left off")
        .expect("the hand-off section");
    assert_eq!(headoff, 0, "the hand-off opens the brief:\n{brief}");
}

/// An outcome other than `completed` fails the attempt: nothing is pushed and the task waits
/// for its retry.
#[test]
fn an_outcome_other_than_completed_fails_the_attempt() {
    let repo = Repo::new("command-error");
    let store = setup(&repo);
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "error");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker-error",
        &format!(
            "{PROLOGUE}printf '{{\"type\":\"Result\",\"outcome\":\"error\",\"text\":\"no\"}}\\n'\n"
        ),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("attempt_finished.json")))
            .expect("attempt_finished.json");
    assert_eq!(finished["outcome"], json!("error"), "{finished}");
    assert_eq!(finished["pushed"], json!(false), "{finished}");
    assert_eq!(repo.remote_branch(&task), None, "nothing was pushed");
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned())
    );
}

/// A command that writes no `Result` line fails as `terminated`, as the `radian exec` worker
/// does.
#[test]
fn a_command_without_a_result_line_is_terminated() {
    let repo = Repo::new("command-no-result");
    let store = setup(&repo);
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "no-result");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker-silent",
        &format!("{PROLOGUE}exit 0\n"),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("attempt_finished.json")))
            .expect("attempt_finished.json");
    assert_eq!(finished["outcome"], json!("terminated"), "{finished}");
    assert_eq!(finished["pushed"], json!(false), "{finished}");
    assert_eq!(repo.remote_branch(&task), None);
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned())
    );
}

/// A command that prints nothing and sleeps is ended by the stall timer, and the attempt
/// records `stall`.
#[test]
fn a_silent_command_that_sleeps_is_stopped_by_the_stall_limit() {
    let repo = Repo::new("command-stall");
    let store = setup(&repo);
    tune(&repo, "stall_timeout_secs = 900", "stall_timeout_secs = 1");
    tune(&repo, "cancel_grace_secs = 10", "cancel_grace_secs = 1");
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "stall");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(&repo, "command-worker-stall", "sleep 30\n");
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let signal: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("signal.json"))).expect("signal");
    assert_eq!(signal["reason"], json!("stall"), "{signal}");
    let result: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("attempt_result.json")))
            .expect("attempt_result.json");
    assert_eq!(result["class"], json!("stall"), "{result}");
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned())
    );
}

/// Every environment variable arrives with its value, `ARC_SESSION_START` is `new` on a first
/// attempt and `resume` on a resume rework, and `ARC_STATE_DIR` is outside the worktree and
/// survives to the next attempt (ADR 0048 decision 7). Mutation: leave
/// `ARC_SESSION_START` unset on the resume attempt → the env file has no line for it.
#[test]
fn the_command_receives_every_arc_variable_and_its_state_survives() {
    let repo = Repo::new("command-env");
    let store = setup(&repo);
    tune(
        &repo,
        "rework_session = \"fresh\"",
        "rework_session = \"resume\"",
    );
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "env");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker-env",
        &format!(
            "{PROLOGUE}\
             survived=no\n\
             [ -f \"$ARC_STATE_DIR/attempt-1\" ] && survived=yes\n\
             touch \"$ARC_STATE_DIR/attempt-$n\"\n\
             {{\n\\
             printf 'ARC_WORKTREE=%s\\n' \"$ARC_WORKTREE\"\n\
             printf 'ARC_TASK=%s\\n' \"$ARC_TASK\"\n\
             printf 'ARC_ATTEMPT=%s\\n' \"$ARC_ATTEMPT\"\n\
             printf 'ARC_SESSION=%s\\n' \"$ARC_SESSION\"\n\
             printf 'ARC_SESSION_START=%s\\n' \"$ARC_SESSION_START\"\n\
             printf 'ARC_GIT_DIR=%s\\n' \"$ARC_GIT_DIR\"\n\
             printf 'ARC_GIT_COMMON_DIR=%s\\n' \"$ARC_GIT_COMMON_DIR\"\n\
             printf 'ARC_DENY_READ=%s\\n' \"$ARC_DENY_READ\"\n\
             printf 'ARC_STATE_DIR=%s\\n' \"$ARC_STATE_DIR\"\n\
             printf 'STATE_SURVIVED=%s\\n' \"$survived\"\n\
             }} > \"$ARC_STATE_DIR/env-$n\"\n\
             touch \"$RADIAN_SESSION_DIR/$ARC_SESSION.jsonl\"\n\
             printf '{{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"report \
             %s\"}}\\n' \"$n\"\n"
        ),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let first = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(first.code, 0, "start failed: {}", first.stderr);
    plan_ops::add_rework(&store, &task, "again").expect("the rework");
    let second = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(second.code, 0, "second start failed: {}", second.stderr);

    let state_dir = repo.arc().join("attempts").join(&task).join("state");
    assert!(state_dir.is_dir(), "{} exists", state_dir.display());
    assert!(
        !state_dir.starts_with(repo.worktree(&task)),
        "ARC_STATE_DIR is outside the worktree"
    );
    let canonical = fs::canonicalize(&state_dir).expect("the state directory");
    let common = fs::canonicalize(repo.clone.join(".git")).expect("the common dir");

    let check = |attempt: u32, start: &str, survived: &str| {
        let text = read(&state_dir.join(format!("env-{attempt}")));
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{key} is missing:\n{text}"))
                .to_owned()
        };
        assert_eq!(
            value("ARC_WORKTREE"),
            canonical_string(&repo.worktree(&task))
        );
        assert_eq!(value("ARC_TASK"), task);
        assert_eq!(value("ARC_ATTEMPT"), format!("{task}-{attempt}"));
        assert_eq!(value("ARC_SESSION"), task, "the resume-mode session id");
        assert_eq!(value("ARC_SESSION_START"), start);
        assert_eq!(
            value("ARC_GIT_DIR"),
            canonical_string(&repo.clone.join(".git/worktrees").join(&task))
        );
        assert_eq!(value("ARC_GIT_COMMON_DIR"), canonical_string(&common));
        let denied = value("ARC_DENY_READ");
        assert!(
            denied.contains(&format!("{}/config.toml", repo.arc().display())),
            "{denied}"
        );
        assert!(
            denied.contains(':'),
            "the paths are joined with `:`: {denied}"
        );
        assert_eq!(value("ARC_STATE_DIR"), canonical_string(&canonical));
        assert_eq!(value("STATE_SURVIVED"), survived);
    };
    check(1, "new", "no");
    check(2, "resume", "yes");
}

fn canonical_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// A missing program refuses `start` with exit `3`, naming `worker.command` and the program,
/// and refuses `run` with the same line. Mutation: skip the program check at the claim → the
/// claim runs and the wrapper fails instead.
#[test]
fn a_missing_program_refuses_the_claim_by_name() {
    let repo = Repo::new("command-missing");
    let store = setup(&repo);
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "missing");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    set_command(&repo, &["definitely-not-a-program-xyz"]);

    let start = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(start.code, 3, "start: {start:?}");
    assert!(start.stderr.contains("worker.command"), "{}", start.stderr);
    assert!(
        start.stderr.contains("definitely-not-a-program-xyz"),
        "{}",
        start.stderr
    );
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "ready".to_owned()),
        "the refused claim changed nothing"
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 3, "run: {run:?}");
    assert!(run.stderr.contains("worker.command"), "{}", run.stderr);
    assert!(
        run.stderr.contains("definitely-not-a-program-xyz"),
        "{}",
        run.stderr
    );
}

/// A file that is not executable is not a program: the claim is refused by the same message.
#[test]
fn a_non_executable_program_refuses_the_claim() {
    let repo = Repo::new("command-not-executable");
    let store = setup(&repo);
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "plain");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let plain = repo.root.join("plain-program");
    fs::write(&plain, "not a program\n").expect("the file");
    set_command(&repo, &[plain.to_str().expect("a path")]);

    let start = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(start.code, 3, "start: {start:?}");
    assert!(start.stderr.contains("worker.command"), "{}", start.stderr);
    assert!(start.stderr.contains("plain-program"), "{}", start.stderr);
}

/// `command` beside a non-empty `exec_args` is a config error that names both keys.
#[test]
fn a_command_with_exec_args_is_a_config_error() {
    let repo = Repo::new("command-exec-args");
    let _store = setup(&repo);
    tune(&repo, "[worker]\n", "[worker]\ncommand = [\"sh\"]\n");
    tune(&repo, "exec_args = []", "exec_args = [\"--model\", \"m\"]");
    let run = repo.radian(&["arc", "list"]);
    assert_eq!(run.code, 1, "{run:?}");
    assert!(run.stderr.contains("worker.command"), "{}", run.stderr);
    assert!(run.stderr.contains("worker.exec_args"), "{}", run.stderr);
}

/// A `Result` without `cost` is an unknown cost: it adds nothing to the task or the plan, the
/// budget does not stop on it, and the claim prints the warning line once for the attempt.
#[test]
fn a_command_without_a_cost_adds_nothing_and_warns_once() {
    let repo = Repo::new("command-no-cost");
    let store = setup(&repo);
    tune(&repo, "# max_cost_usd = 5.0", "max_cost_usd = 5.0");
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "no-cost");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker-no-cost",
        &format!(
            "{PROLOGUE}printf '{{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"done\"}}\\n'\n"
        ),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let warning = format!("{task}: worker.command reports no cost; the cost limit does not apply");
    assert_eq!(
        run.stdout.matches(&warning).count(),
        1,
        "the warning is printed once:\n{}",
        run.stdout
    );

    let task_file = store.load_task(&task).expect("the task");
    assert_eq!(task_file.item.cost_usd, None, "nothing was charged");
    assert_eq!(task_file.item.total_cost_usd, None);
    let view = store.plan_view(&plan).expect("the plan");
    assert_eq!(view.cost_usd(), 0.0);
    assert_eq!(view.total_cost_usd(), 0.0, "the plan total leaves it out");

    let result: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("attempt_result.json")))
            .expect("attempt_result.json");
    assert_eq!(result["cost_usd"], json!(null), "{result}");
    assert_eq!(result["cost_known"], json!(false), "{result}");

    let shown = repo.radian(&["arc", "show", task.as_str(), "--json"]);
    let shown: Value = serde_json::from_str(&shown.stdout).expect("show --json");
    assert_eq!(shown["cost_usd"], json!(null), "{shown}");

    let text = repo.radian(&["arc", "show", task.as_str()]);
    assert!(
        text.stdout.contains("cost_usd: unknown"),
        "the text form says the cost is unknown:\n{}",
        text.stdout
    );
    let list = repo.radian(&["arc", "list"]);
    assert!(
        list.stdout.contains("total_cost_usd: unknown"),
        "the plan's total is unknown:\n{}",
        list.stdout
    );
}

/// An unknown cost never reaches a task budget, even one of `0`: the attempt retries instead
/// of pausing. Mutation: count the unknown cost as zero → `0 >= 0` pauses the task.
#[test]
fn an_unknown_cost_does_not_stop_on_a_zero_budget() {
    let repo = Repo::new("command-zero-budget");
    let store = setup(&repo);
    tune(&repo, "# max_cost_usd = 5.0", "max_cost_usd = 0.0");
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "zero-budget");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker-zero-budget",
        &format!(
            "{PROLOGUE}printf '{{\"type\":\"Result\",\"outcome\":\"error\",\"text\":\"no\"}}\n'\n"
        ),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned()),
        "the unknown cost did not reach the budget: {}",
        run.stdout
    );
}

/// A command worker that reports `cost` keeps the ordinary accounting.
#[test]
fn a_command_that_reports_a_cost_is_charged() {
    let repo = Repo::new("command-cost");
    let store = setup(&repo);
    let plan = add_plan(&store, "command");
    let task = add_task(&store, &plan, "cost");
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let worker = script(
        &repo,
        "command-worker-cost",
        &format!(
            "{PROLOGUE}printf '{{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"done\",\
             \"cost\":{{\"micros\":2500}}}}\\n'\n"
        ),
    );
    set_command(&repo, &[worker.to_str().expect("a path")]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let task_file = store.load_task(&task).expect("the task");
    assert_eq!(task_file.item.cost_usd, Some(0.0025));
    let record: Value =
        serde_json::from_str(&read(&repo.run_dir(&task, 1).join("attempt_result.json")))
            .expect("attempt_result.json");
    assert_eq!(record["cost_known"], json!(true), "{record}");
    assert_eq!(record["cost_usd"], json!(0.0025), "{record}");
}

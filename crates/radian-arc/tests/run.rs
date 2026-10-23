//! `radian arc run`: the scheduling loop (ADR 0048 decision 13).
//!
//! The auto-level cases run the real `radian arc run` and a real `radian exec` worker under
//! the mock model; the digest, lock and retry cases drive the real CLI against a temp
//! repository with a bare `origin`.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{Barrier, Reply};
use common::*;
use radian_arc::attempt;
use radian_arc::gate::background;
use radian_arc::gate::{Stage, Verdict};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};
use serde_json::Value;

fn setup(repo: &Repo) -> Store {
    Store::init(&repo.clone, false).expect("arc init");
    Store::open(&repo.clone).expect("the store opens")
}

fn add_plan(store: &Store, slug: &str) -> String {
    store
        .add_plan(slug, "A plan", "the request\n")
        .expect("add a plan")
}

fn add_task(store: &Store, plan: &str, slug: &str, after: &[&str]) -> String {
    store
        .add_task(
            plan,
            slug,
            "A task",
            after.iter().map(|id| (*id).to_owned()).collect(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task")
}

/// The config's `commands` stage: one line.
fn commands(repo: &Repo, line: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains("\ncommands = []"), "the key is in the file");
    fs::write(
        &path,
        text.replacen("\ncommands = []", &format!("\ncommands = [\"{line}\"]"), 1),
    )
    .expect("write config.toml");
}

/// The config's integration stage: one key command.
fn key_commands(repo: &Repo, line: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(
        text.contains("\nkey_commands = []"),
        "the key is in the file"
    );
    fs::write(
        &path,
        text.replacen(
            "\nkey_commands = []",
            &format!("\nkey_commands = [\"{line}\"]"),
            1,
        ),
    )
    .expect("write config.toml");
}

/// The worker's model route: the mock server, plus any extra `radian exec` arguments.
fn worker_args_with(repo: &Repo, base_url: &str, extra: &[&str]) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains("exec_args = []"), "the key is in the file");
    let mut args = vec![
        "--model".to_owned(),
        "m".to_owned(),
        "--base-url".to_owned(),
        base_url.to_owned(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    let list = args
        .iter()
        .map(|arg| format!("\"{arg}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &path,
        text.replace("exec_args = []", &format!("exec_args = [{list}]")),
    )
    .expect("write config.toml");
}

fn worker_args(repo: &Repo, base_url: &str) {
    worker_args_with(repo, base_url, &[]);
}

fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

fn plan_auto(store: &Store, plan: &str) -> String {
    store.load_plan(plan).expect("the plan reads").item.auto
}

fn commit_reply(id: &str, file: &str) -> Reply {
    shell_reply(
        id,
        &format!("echo {file} > {file} && git add {file} && git commit -m {file}"),
    )
}

/// Publishes a branch `work/<task>` with one commit and puts the task in `review.verifying`
/// the way a pushed attempt leaves it.
fn publish_verifying(repo: &Repo, store: &Store, plan: &str, task: &str) -> String {
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("feature.txt"), "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "feature"]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", "origin", &branch]);
    let plan = plan.to_owned();
    store
        .change_task(task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.token = Some("token-1".to_owned());
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            task.plan = plan.clone();
            Ok(())
        })
        .expect("the task is in review.verifying");
    head
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Runs `radian arc run` again and again until `done` holds (a pass only schedules: the
/// worker and the gate run outside it) and returns everything the runs printed. Each pass is
/// a few hundred milliseconds; a worker or a gate is collected by a later one.
fn run_until(repo: &Repo, limit: Duration, done: impl Fn() -> bool) -> String {
    let deadline = Instant::now() + limit;
    let mut output = String::new();
    loop {
        let run = repo.radian(&["arc", "run"]);
        assert_eq!(run.code, 0, "run failed: {}", run.stderr);
        output.push_str(&run.stdout);
        if done() {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "the loop did not settle within {limit:?}:\n{output}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn utc(text: &str) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::parse_from_rfc3339(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

// ---- auto levels ----------------------------------------------------------------------

/// At auto `run`, the passes take A to `awaiting_human` and stop: B waits on A's merge. After
/// `merge A`, later passes take B. A pass only starts the worker or the gate; the loop of
/// passes is what collects them.
#[test]
fn one_run_takes_a_chain_to_awaiting_human_and_stops() {
    let repo = Repo::new("run-chain");
    let store = setup(&repo);
    let plan = add_plan(&store, "chain");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[&a]);
    let c = add_task(&store, &plan, "c", &[&b]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    let mock = Mock::start(vec![
        commit_reply("c1", "a.txt"),
        text_reply("done"),
        commit_reply("c2", "b.txt"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    let first = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &a).1 == "awaiting_human"
    });
    assert!(
        first.contains(&format!("{a}-1 verify -> review.awaiting_human")),
        "{first}"
    );
    assert_eq!(
        state(&store, &a),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
    assert_eq!(state(&store, &b), ("todo".to_owned(), "ready".to_owned()));
    assert_eq!(state(&store, &c), ("todo".to_owned(), "ready".to_owned()));
    assert!(
        first.contains(&format!("{b} todo.ready: waiting for {a} to merge")),
        "{first}"
    );
    // One attempt, one run directory named by its attempt id.
    let attempts = store.root().join("runs").join(&a).join("attempts");
    let names: Vec<String> = fs::read_dir(&attempts)
        .expect("the attempt directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![format!("{a}-1")], "{names:?}");
    let dir = repo.run_dir(&a, 1);
    for name in ["result.jsonl", "attempt_finished.json", "execution.json"] {
        assert!(dir.join(name).is_file(), "runs/<task>/attempts/<id>/{name}");
    }
    assert!(dir.join("verdicts/commands.json").is_file());

    let merged = repo.radian(&["arc", "merge", &a]);
    assert_eq!(merged.code, 0, "merge: {}", merged.stderr);
    assert!(merged.stdout.contains("done.merged"), "{}", merged.stdout);

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &b).1 == "awaiting_human"
    });
    assert_eq!(
        state(&store, &b),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
    assert_eq!(state(&store, &c), ("todo".to_owned(), "ready".to_owned()));
    assert_eq!(state(&store, &a), ("done".to_owned(), "merged".to_owned()));
}

/// At auto `merge`, the passes take all three to `done.merged`, in order.
#[test]
fn auto_merge_takes_all_three_to_done_merged_in_one_run() {
    let repo = Repo::new("run-merge");
    let store = setup(&repo);
    let plan = add_plan(&store, "all");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[&a]);
    let c = add_task(&store, &plan, "c", &[&b]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "merge").expect("auto merge");
    commands(&repo, "true");
    let mock = Mock::start(vec![
        commit_reply("c1", "a.txt"),
        text_reply("done"),
        commit_reply("c2", "b.txt"),
        text_reply("done"),
        commit_reply("c3", "c.txt"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    let run = run_until(&repo, Duration::from_secs(180), || {
        [&a, &b, &c]
            .iter()
            .all(|task| state(&store, task) == ("done".to_owned(), "merged".to_owned()))
    });
    for task in [&a, &b, &c] {
        assert_eq!(
            state(&store, task),
            ("done".to_owned(), "merged".to_owned()),
            "{task}: {run}"
        );
    }
    let merged_lines: Vec<&str> = run
        .lines()
        .filter(|line| line.contains("done.merged"))
        .collect();
    assert_eq!(merged_lines.len(), 3, "{run}");
    assert!(merged_lines[0].starts_with(&a), "{merged_lines:?}");
    assert!(merged_lines[1].starts_with(&b), "{merged_lines:?}");
    assert!(merged_lines[2].starts_with(&c), "{merged_lines:?}");
    let head = store
        .load_task(&c)
        .expect("c reads")
        .item
        .head_sha
        .expect("c has a head");
    assert_eq!(
        repo.remote_main_sha(),
        head,
        "the remote main ends at C's head"
    );
    // The automatic approval records its approver.
    let human = Verdict::read(
        &store
            .root()
            .join("runs")
            .join(&c)
            .join("attempts")
            .join(format!("{c}-1"))
            .join("verdicts/human.json"),
    )
    .expect("the human verdict");
    assert!(human.is_pass());
    assert_eq!(human.approver.as_deref(), Some("auto"));
}

/// A plan at auto `off` starts nothing; its verification still advances.
#[test]
fn auto_off_starts_nothing_but_still_verifies() {
    let repo = Repo::new("run-off");
    let store = setup(&repo);
    let plan = add_plan(&store, "off");
    let verifying = add_task(&store, &plan, "verify", &[]);
    let ready = add_task(&store, &plan, "ready", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    let head = publish_verifying(&repo, &store, &plan, &verifying);

    let run = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &verifying).1 == "awaiting_human"
    });
    let stored = store.load_task(&verifying).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human")
    );
    assert_eq!(stored.verified_sha.as_deref(), Some(head.as_str()));
    assert_eq!(
        state(&store, &ready),
        ("todo".to_owned(), "ready".to_owned())
    );
    assert!(
        run.contains(&format!("{ready} todo.ready: waiting for you to start it")),
        "{run}"
    );
    assert!(
        !store
            .root()
            .join("runs")
            .join(&ready)
            .join("attempts")
            .join(format!("{ready}-1"))
            .exists(),
        "auto off started the ready task"
    );
}

/// A second `run` while one holds `runner.lock` exits 4 at once.
#[test]
fn a_second_run_while_one_holds_the_lock_exits_four() {
    use fs2::FileExt;
    let repo = Repo::new("run-lock");
    let _store = setup(&repo);
    let path = repo.arc().join("runner.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .expect("the lock file opens");
    file.lock_exclusive().expect("the test takes the lock");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 4, "{run:?}");
    assert!(
        run.stderr.contains("locked by another process"),
        "{}",
        run.stderr
    );
    file.unlock().expect("the lock is released");
}

/// `[limits] concurrency = 2` with two independent tasks: both attempts run at once.
#[test]
fn concurrency_two_runs_both_attempts_at_once() {
    let repo = Repo::new("run-concurrency");
    let store = setup(&repo);
    let plan = add_plan(&store, "parallel");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    tune(&repo, "concurrency = 1", "concurrency = 2");
    let barrier = Barrier::new(2, Duration::from_secs(30));
    let mut first_a = shell_reply(
        "c1",
        "sleep 1 && echo a > a.txt && git add a.txt && git commit -m a",
    );
    let mut first_b = shell_reply(
        "c2",
        "sleep 1 && echo b > b.txt && git add b.txt && git commit -m b",
    );
    // Both workers' first requests wait at the barrier, so neither can take the other's
    // scripted reply and the two shells provably run at the same time.
    first_a = Reply::Barrier {
        barrier: barrier.clone(),
        reply: Box::new(first_a),
    };
    first_b = Reply::Barrier {
        barrier: barrier.clone(),
        reply: Box::new(first_b),
    };
    let mock = Mock::start(vec![
        first_a,
        first_b,
        text_reply("done"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    let run = run_until(&repo, Duration::from_secs(120), || {
        [&a, &b]
            .iter()
            .all(|task| state(&store, task).1 == "awaiting_human")
    });
    for task in [&a, &b] {
        assert_eq!(
            state(&store, task),
            ("review".to_owned(), "awaiting_human".to_owned()),
            "{task}: {run}"
        );
    }
    let execution = |task: &str| -> Value {
        serde_json::from_str(
            &fs::read_to_string(repo.run_dir(task, 1).join("execution.json"))
                .expect("execution.json"),
        )
        .expect("execution.json parses")
    };
    let (a_exec, b_exec) = (execution(&a), execution(&b));
    let a_start = utc(a_exec["started_at"].as_str().expect("a started_at"));
    let a_end = utc(a_exec["ended_at"].as_str().expect("a ended_at"));
    let b_start = utc(b_exec["started_at"].as_str().expect("b started_at"));
    let b_end = utc(b_exec["ended_at"].as_str().expect("b ended_at"));
    assert!(
        a_start < b_end && b_start < a_end,
        "the attempts did not overlap: {a_exec} {b_exec}"
    );
}

/// `max_auto_rework + 1` failed gates under auto `run` pause the whole plan: `auto` becomes
/// `off`, the task stays `review.rework_pending`, and both `run` and `show` say why.
#[test]
fn max_auto_rework_pauses_the_plan_and_sets_auto_off() {
    let repo = Repo::new("run-rework");
    let store = setup(&repo);
    let plan = add_plan(&store, "rework");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "false");
    let mock = Mock::start(vec![
        commit_reply("c1", "one.txt"),
        text_reply("done"),
        commit_reply("c2", "two.txt"),
        text_reply("done"),
        commit_reply("c3", "three.txt"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    for attempt in 1..=3 {
        let run = run_until(&repo, Duration::from_secs(90), || {
            state(&store, &task).1 == "rework_pending"
        });
        let stored = store.load_task(&task).expect("the task reads").item;
        assert_eq!(
            (stored.state.as_str(), stored.phase.as_str()),
            ("review", "rework_pending"),
            "attempt {attempt}: {run}"
        );
        assert_eq!(stored.auto_rework, attempt);
        if attempt <= 2 {
            assert_eq!(plan_auto(&store, &plan), "run", "attempt {attempt}");
            plan_ops::add_rework(&store, &task, "try again").expect("a rework brief");
        } else {
            assert_eq!(plan_auto(&store, &plan), "off", "attempt {attempt}");
            assert!(
                run.contains("auto rework limit"),
                "the pause says why: {run}"
            );
        }
    }
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(stored.auto_rework, 3);
    let shown = repo.radian(&["arc", "show", &task]);
    assert_eq!(shown.code, 0, "{shown:?}");
    assert!(
        shown.stdout.contains("auto rework limit"),
        "show says why: {}",
        shown.stdout
    );
    // A paused plan starts nothing more.
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        !run.stdout.contains(&format!("{task}-4")),
        "a paused plan started another attempt: {}",
        run.stdout
    );
}

/// Publishes `work/<task>` with a commit on `feature.txt`, sets the task to
/// `review.verifying`, and moves main with a conflicting `feature.txt` in the main clone. The
/// gate's candidate build then conflicts. Returns the moved main.
fn a_conflicting_main(repo: &Repo, store: &Store, plan: &str, task: &str) -> String {
    publish_verifying(repo, store, plan, task);
    repo.git(&["checkout", "main"]);
    write(&repo.clone.join("feature.txt"), "main side\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "main side"]);
    repo.git(&["push", "origin", "main"]);
    repo.git(&["rev-parse", "HEAD"]).trim().to_owned()
}

/// A `commands` conflict at the gate returns the task to `todo.ready` by itself, with the brief
/// arc wrote and its own line; `auto_rework` is untouched. The blocked dependency keeps the
/// next attempt from being claimed, so the assertions see the state the conflict left.
/// Mutations: drop the brief, count the rework in `auto_rework`, or use `base_sha` as the
/// automatic path's trigger — the paths and the line fail.
#[test]
fn a_commands_conflict_returns_the_task_to_todo_ready_by_itself() {
    let repo = Repo::new("run-conflict");
    let store = setup(&repo);
    let plan = add_plan(&store, "conflict");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let task = add_task(&store, &plan, "work", &[&blocker]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    store
        .change_task(&blocker, |task| {
            task.phase = "blocked".to_owned();
            Ok(())
        })
        .expect("the blocker waits");
    commands(&repo, "true");
    let moved = a_conflicting_main(&repo, &store, &plan, &task);

    let run = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task) == ("todo".to_owned(), "ready".to_owned())
    });
    assert!(
        run.contains(&format!(
            "{task} conflict with main -> todo.ready (automatic rework)"
        )),
        "{run}"
    );
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        stored.auto_rework, 0,
        "a conflict rework raised auto_rework: {run}"
    );
    assert_eq!(plan_auto(&store, &plan), "run", "{run}");
    assert_eq!(stored.attempt, 1, "the next attempt was claimed: {run}");
    assert!(
        !repo.attempt_dir(&task, 2).exists(),
        "the next attempt was claimed"
    );
    let short = &moved[..7];
    assert!(
        stored.body.contains("## Rework 2 (conflict with main)"),
        "{}",
        stored.body
    );
    assert!(
        stored
            .body
            .contains(&format!("conflicted with main at `{short}` in `commands`")),
        "{}",
        stored.body
    );
    assert!(stored.body.contains("- feature.txt"), "{}", stored.body);
}

/// A plan at its automatic rework limit is not paused by a conflict: the conflict rework is
/// not an automatic rework. Mutation: count it and the plan pauses (`auto` becomes `off`).
#[test]
fn a_plan_at_its_auto_rework_limit_is_not_paused_by_a_conflict() {
    let repo = Repo::new("run-conflict-limit");
    let store = setup(&repo);
    let plan = add_plan(&store, "conflict-limit");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let task = add_task(&store, &plan, "work", &[&blocker]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    store
        .change_task(&blocker, |task| {
            task.phase = "blocked".to_owned();
            Ok(())
        })
        .expect("the blocker waits");
    tune(&repo, "max_auto_rework = 2", "max_auto_rework = 0");
    commands(&repo, "true");
    a_conflicting_main(&repo, &store, &plan, &task);

    let run = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task) == ("todo".to_owned(), "ready".to_owned())
    });
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(stored.auto_rework, 0, "{run}");
    assert_eq!(
        plan_auto(&store, &plan),
        "run",
        "the conflict paused the plan: {run}"
    );
    assert!(!run.contains("paused"), "{run}");
}

/// A plan budget pauses the plan; `run` respects the pause and starts nothing more.
#[test]
fn run_respects_a_plan_budget_pause() {
    let repo = Repo::new("run-budget");
    let store = setup(&repo);
    let plan = add_plan(&store, "budget");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    store
        .change_plan(&plan, |plan| {
            plan.max_cost_usd = Some(0.0);
            Ok(())
        })
        .expect("the plan limit");
    // The attempt ends on its model-call fuse, a retryable failure: the plan budget then
    // pauses the task instead of retrying it.
    let mock = Mock::start(vec![tool_reply("c1", "read", "{}")]);
    worker_args_with(&repo, mock.base_url(), &["--max-model-calls", "1"]);

    let run = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "paused_budget"
    });
    assert_eq!(plan_auto(&store, &plan), "off", "{run}");
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "paused_budget".to_owned())
    );
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "paused_budget".to_owned())
    );
}

// ---- digest and retry -----------------------------------------------------------------

/// A digest mismatch at `merge` returns the task to `review.verifying`; the next `run`
/// re-verifies it.
#[test]
fn a_digest_mismatch_returns_to_verifying_and_is_reverified() {
    let repo = Repo::new("run-digest");
    let store = setup(&repo);
    let plan = add_plan(&store, "digest");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    let mock = Mock::start(vec![commit_reply("c1", "a.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    let start = repo.radian(&["arc", "start", &task]);
    assert_eq!(start.code, 0, "start: {}", start.stderr);
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(stored.phase, "awaiting_human");
    let head = stored.head_sha.clone().expect("head_sha");
    let first_digest = stored.verdict_set_digest.clone().expect("a digest");

    // A new verdict for the same commit changes the set without touching the sha: the
    // recorded approval no longer matches.
    let extra = store
        .root()
        .join("runs")
        .join(&task)
        .join("attempts")
        .join(format!("{task}-9"))
        .join("verdicts");
    fs::create_dir_all(&extra).expect("the extra verdict directory");
    let mut verdict = Verdict::human(head.clone(), true, None);
    verdict.stage = Stage::Mutation;
    verdict
        .write_new(&extra.join("mutation.json"))
        .expect("the extra verdict");

    let merged = repo.radian(&["arc", "merge", &task]);
    assert_eq!(merged.code, 3, "{merged:?}");
    assert!(merged.stderr.contains("verdict set"), "{}", merged.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "verifying".to_owned())
    );
    let stored = store.load_task(&task).expect("the task reads").item;
    assert!(stored.verified_sha.is_none(), "the approval is invalid");
    assert!(stored.verdict_set_digest.is_none());

    let run = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "awaiting_human"
    });
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human"),
        "{run}"
    );
    assert_eq!(stored.verified_sha.as_deref(), Some(head.as_str()));
    assert_ne!(
        stored.verdict_set_digest.as_deref(),
        Some(first_digest.as_str()),
        "the re-verification recorded the new set"
    );
}

/// A `retry_wait` task whose time is in the future makes `run` exit with it waiting, not
/// sleeping until then.
#[test]
fn a_future_retry_makes_run_exit_with_it_waiting() {
    let repo = Repo::new("run-retry");
    let store = setup(&repo);
    let plan = add_plan(&store, "retry");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    store
        .change_task(&task, |task| {
            task.phase = "retry_wait".to_owned();
            task.retry_at = Some("2099-01-01T00:00:00Z".to_owned());
            Ok(())
        })
        .expect("the task waits");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout.contains(&format!(
            "{task} todo.retry_wait: waiting for its retry at 2099-01-01T00:00:00Z"
        )),
        "{}",
        run.stdout
    );
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned())
    );
    assert!(
        !repo.run_dir(&task, 1).exists(),
        "run started a task that waits for its retry"
    );
}

// ---- start and the wrapper ------------------------------------------------------------

/// `start` on a task in `review.verifying` resumes verification instead of refusing.
#[test]
fn start_resumes_verification_of_a_verifying_task() {
    let repo = Repo::new("run-start-verify");
    let store = setup(&repo);
    let plan = add_plan(&store, "resume");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    let head = publish_verifying(&repo, &store, &plan, &task);

    let run = repo.radian(&["arc", "start", &task]);
    assert_eq!(run.code, 0, "start: {}", run.stderr);
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human"),
        "{}",
        run.stdout
    );
    assert_eq!(stored.verified_sha.as_deref(), Some(head.as_str()));
    assert_eq!(stored.attempt, 1, "start did not claim a new attempt");
    assert!(
        run.stdout
            .contains(&format!("{task}-1 verify -> review.awaiting_human")),
        "{}",
        run.stdout
    );
}

/// The wrapper forwards a SIGTERM to the worker's process group: a background child of the
/// worker dies with it. Mutation: signal the worker's pid only and the `sleep` survives.
#[test]
fn a_cancelled_attempt_whose_worker_started_sleep_leaves_no_sleep_process() {
    let repo = Repo::new("run-sleep");
    setup(&repo);
    let sleeper = repo.root.join("sleeper.pid");
    let task = "TK0001-sleep";
    let worker = format!("sleep 300 & echo $! > '{}'; wait", sleeper.display());
    let mut child = repo.spawn(&[
        "arc",
        "work",
        "--task",
        task,
        "--attempt",
        "1",
        "--token",
        "tok-sleep",
        "--worker-command",
        &worker,
    ]);
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(b"the brief\n")
            .expect("write the brief");
    }
    wait_for(&repo.attempt_dir(task, 1).join("identity.json"));
    wait_for(&sleeper);
    let pid: u32 = fs::read_to_string(&sleeper)
        .expect("sleeper.pid")
        .trim()
        .parse()
        .expect("a pid");
    let identity: Value = serde_json::from_str(
        &fs::read_to_string(repo.attempt_dir(task, 1).join("identity.json")).expect("identity"),
    )
    .expect("identity parses");
    let wrapper = identity["wrapper_pid"].as_u64().expect("wrapper_pid") as u32;
    let sent = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status()
        .expect("kill");
    assert!(sent.success(), "kill -TERM {wrapper}");
    let output = child.wait_with_output().expect("the wrapper ends");
    assert_eq!(
        output.status.code(),
        Some(143),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .expect("kill -0")
            .success();
        if !alive {
            break;
        }
        if Instant::now() >= deadline {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
            panic!("the worker's `sleep 300` (pid {pid}) outlived the cancel");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- a pass only schedules ------------------------------------------------------------

/// Waits until `predicate` holds, failing with `what` after `limit`.
fn wait_until(limit: Duration, what: &str, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen in {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The gate's lock file: it exists once a pass started that task's gate.
fn gate_lock(repo: &Repo, task: &str) -> std::path::PathBuf {
    repo.arc().join("gates").join(format!("{task}.gate.lock"))
}

/// `[limits] concurrency` counts workers. While A's worker runs long, a later pass with a
/// second slot starts B, and B reaches `done.merged` before A ends.
#[test]
fn a_long_worker_does_not_hold_up_another_tasks_verify_and_merge() {
    let repo = Repo::new("run-long-worker");
    let store = setup(&repo);
    let plan = add_plan(&store, "parallel");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "merge").expect("auto merge");
    commands(&repo, "sleep 1");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "sleep 30 && echo a > a.txt && git add a.txt && git commit -m a",
        ),
        shell_reply("c2", "echo b > b.txt && git add b.txt && git commit -m b"),
        text_reply("done"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    // One slot: only A starts, so A's request is the first the mock serves.
    run_until(&repo, Duration::from_secs(60), || {
        state(&store, &a).0 == "doing"
    });
    wait_until(Duration::from_secs(30), "A's first model request", || {
        !mock.requests().is_empty()
    });

    tune(&repo, "concurrency = 1", "concurrency = 2");
    let run = run_until(&repo, Duration::from_secs(120), || {
        state(&store, &b) == ("done".to_owned(), "merged".to_owned())
    });
    assert_eq!(
        state(&store, &a),
        ("doing".to_owned(), "running".to_owned()),
        "A's worker should still be running when B merged: {run}"
    );

    // Clean up the long worker rather than leave it behind.
    let canceled = repo.radian(&["arc", "cancel", &a]);
    assert_eq!(canceled.code, 0, "cancel: {}", canceled.stderr);
}

/// A running gate holds its own lock but no `[limits] concurrency` slot: with A's worker gone,
/// a pass claims B while A's gate still runs.
#[test]
fn a_running_gate_does_not_take_a_concurrency_slot() {
    let repo = Repo::new("run-gate-slot");
    let store = setup(&repo);
    let plan = add_plan(&store, "slot");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "sleep 5");
    let mock = Mock::start(vec![
        commit_reply("c1", "a.txt"),
        text_reply("done"),
        commit_reply("c2", "b.txt"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    run_until(&repo, Duration::from_secs(60), || {
        state(&store, &a).1 == "verifying"
    });
    wait_until(Duration::from_secs(30), "A's gate", || {
        gate_lock(&repo, &a).is_file()
    });

    // The slot A's worker held is free while its gate runs: B starts now.
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert_eq!(
        state(&store, &b).0,
        "doing",
        "B was not claimed while A's gate runs: {}",
        run.stdout
    );

    // Let A's gate finish so the test leaves nothing behind.
    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &a).1 == "awaiting_human"
    });
    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &b).1 == "awaiting_human"
    });
}

/// The wrapper owns the limits: after the `run` process is killed, its worker still stops at
/// `max_wall_time_secs` and leaves its record. Mutation: enforce the wall time in the `run`
/// process instead, and this test waits forever for `attempt_finished.json`.
#[test]
fn a_worker_still_stops_at_its_limit_after_the_run_process_is_killed() {
    let repo = Repo::new("run-limit-killed");
    let store = setup(&repo);
    let plan = add_plan(&store, "limit");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    tune(&repo, "max_wall_time_secs = 7200", "max_wall_time_secs = 3");
    tune(
        &repo,
        "stall_timeout_secs = 900",
        "stall_timeout_secs = 600",
    );
    let mock = Mock::start(vec![Reply::SseThenHang { pieces: Vec::new() }]);
    worker_args(&repo, mock.base_url());

    let mut run = repo.spawn(&["arc", "run"]);
    wait_for(&repo.attempt_dir(&task, 1).join("identity.json"));
    let _ = run.kill();
    let _ = run.wait();

    let finished = repo.run_dir(&task, 1).join("attempt_finished.json");
    wait_for(&finished);
    let signal: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&task, 1).join("signal.json")).expect("signal.json"),
    )
    .expect("signal.json parses");
    assert_eq!(signal["reason"], "wall_time", "{signal}");
    // The next pass applies what the wrapper left.
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned()),
        "{}",
        run.stdout
    );
}

/// One shot: `radian arc run` starts the wrapper and returns; the next run collects the
/// result (ADR 0048 decision 13).
#[test]
fn a_one_shot_run_exits_after_starting_a_worker_and_the_next_run_collects_it() {
    let repo = Repo::new("run-one-shot");
    let store = setup(&repo);
    let plan = add_plan(&store, "one");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "sleep 5 && echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    let started = Instant::now();
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("{task}-1 started -> doing.running")),
        "{}",
        run.stdout
    );
    assert_eq!(
        state(&store, &task),
        ("doing".to_owned(), "running".to_owned()),
        "the pass waited for the worker: {}",
        run.stdout
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a one-shot run took {:?}",
        started.elapsed()
    );
    assert!(
        !repo
            .run_dir(&task, 1)
            .join("attempt_finished.json")
            .exists(),
        "the worker already finished; the test proves nothing"
    );

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "awaiting_human"
    });
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
}

/// One gate per task: while a gate runs, a pass starts no second gate for the same sha, so
/// the stage's command runs once. Mutation: drop the lock check in `background::start` and
/// the counter has two lines.
#[test]
fn a_gate_is_not_started_twice_while_it_runs() {
    let repo = Repo::new("run-one-gate");
    let store = setup(&repo);
    let plan = add_plan(&store, "one-gate");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    let counter = repo.root.join("gate-starts.txt");
    commands(
        &repo,
        &format!("echo start >> '{}' && sleep 3", counter.display()),
    );
    publish_verifying(&repo, &store, &plan, &task);

    let first = repo.radian(&["arc", "run"]);
    assert_eq!(first.code, 0, "run: {}", first.stderr);
    assert_eq!(
        first.stdout.matches("verify started").count(),
        1,
        "a pass started the same gate more than once: {}",
        first.stdout
    );
    assert!(
        first.stdout.contains("verify started"),
        "the first pass did not start the gate: {}",
        first.stdout
    );
    let second = repo.radian(&["arc", "run"]);
    assert_eq!(second.code, 0, "run: {}", second.stderr);
    assert!(
        !second.stdout.contains("verify started"),
        "a second gate started while the first ran: {}",
        second.stdout
    );

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "awaiting_human"
    });
    let starts = fs::read_to_string(&counter).expect("the counter");
    assert_eq!(
        starts.lines().count(),
        1,
        "the commands stage ran more than once: {starts}"
    );
}

/// SIGTERM to `--watch`: no new worker and no new gate starts; the running worker finishes
/// and the next `run` collects it (ADR 0048 decision 13).
#[test]
fn sigterm_to_watch_starts_no_new_work_and_the_next_run_collects_the_worker() {
    let repo = Repo::new("run-watch-stop");
    let store = setup(&repo);
    let plan = add_plan(&store, "stop");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "sleep 5 && echo a > a.txt && git add a.txt && git commit -m a",
        ),
        text_reply("done"),
        commit_reply("c2", "b.txt"),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    let watch = repo.spawn(&["arc", "run", "--watch", "--interval", "30"]);
    wait_for(&repo.attempt_dir(&a, 1).join("identity.json"));
    // While the loop waits out its interval, a new `review.verifying` task appears.
    publish_verifying(&repo, &store, &plan, &b);

    let sent = std::process::Command::new("kill")
        .args(["-TERM", &watch.id().to_string()])
        .status()
        .expect("kill");
    assert!(sent.success(), "kill -TERM");
    let output = watch.wait_with_output().expect("the watch loop ends");
    assert_eq!(
        output.status.code(),
        Some(0),
        "watch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // No gate started for B after the signal.
    assert!(
        !gate_lock(&repo, &b).exists(),
        "the stopped loop started a gate"
    );
    assert_eq!(
        state(&store, &b),
        ("review".to_owned(), "verifying".to_owned())
    );

    // A's worker was not signalled: it finishes on its own, and the next run collects it.
    let finished = repo.run_dir(&a, 1).join("attempt_finished.json");
    wait_for(&finished);
    assert!(
        !repo.attempt_dir(&b, 1).join("identity.json").exists(),
        "the signal started B's worker"
    );
    assert_eq!(
        state(&store, &b),
        ("review".to_owned(), "verifying".to_owned())
    );
    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &a).1 == "awaiting_human"
    });
    assert_eq!(
        state(&store, &a),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
}

// ---- gate records and a stopped stage ------------------------------------------------

/// The task's `runs/<task>/gate.json`, when one exists.
fn gate_record(repo: &Repo, task: &str) -> Option<Value> {
    let text = fs::read_to_string(repo.arc().join("runs").join(task).join("gate.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes a gate record as the gate process would leave it.
fn write_gate_record(repo: &Repo, task: &str, record: Value) {
    let dir = repo.arc().join("runs").join(task);
    fs::create_dir_all(&dir).expect("the run directory");
    fs::write(dir.join("gate.json"), record.to_string()).expect("the gate record");
}

/// Whether `pid` exists (a `kill -0` probe, no `ps`).
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .expect("kill -0")
        .success()
}

/// Sends `signal` to the process group `pgid`, the way a terminal signals its foreground job.
/// This is `kill(2)` and not the `kill` program: procps-ng reads a negative id that follows
/// the signal flag (`kill -INT -123`) as an option and exits 1, where macOS' `kill` accepts
/// it.
fn signal_group(pgid: u32, signal: i32) -> bool {
    // SAFETY: `kill(2)` takes plain integers and has no memory contract; a negative pid names
    // the process group whose leader is `pgid`.
    unsafe { libc::kill(-(pgid as libc::pid_t), signal) == 0 }
}

/// Makes `path` executable.
fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).expect("the script").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("executable");
}

/// A gate `error` record belongs to the current attempt and is reported as an error: a
/// one-shot `run` exits 1 with its text.
#[test]
fn a_gate_error_record_is_reported_as_an_error() {
    let repo = Repo::new("run-gate-error");
    let store = setup(&repo);
    let plan = add_plan(&store, "gate-error");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    publish_verifying(&repo, &store, &plan, &task);
    write_gate_record(
        &repo,
        &task,
        serde_json::json!({
            "kind": "verify",
            "attempt": format!("{task}-1"),
            "status": "error",
            "text": "boom",
        }),
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(
        run.code, 1,
        "a one-shot run keeps an error's code: {} {}",
        run.stdout, run.stderr
    );
    assert!(run.stderr.contains("boom"), "{}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "verifying".to_owned())
    );
}

/// `--watch` prints a pass error as one line and runs the next pass instead of exiting.
#[test]
fn a_watch_pass_that_errors_prints_a_line_and_goes_on() {
    let repo = Repo::new("run-watch-error");
    let store = setup(&repo);
    let plan = add_plan(&store, "watch-error");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    publish_verifying(&repo, &store, &plan, &task);
    write_gate_record(
        &repo,
        &task,
        serde_json::json!({
            "kind": "verify",
            "attempt": format!("{task}-1"),
            "status": "error",
            "text": "boom",
        }),
    );

    let mut watch = repo.spawn(&["arc", "run", "--watch", "--interval", "30"]);
    // The pass reads the record, reports it and clears it; the loop then waits its interval.
    wait_until(
        Duration::from_secs(30),
        "the error record to be read",
        || gate_record(&repo, &task).is_none(),
    );
    assert!(
        watch.try_wait().expect("try_wait").is_none(),
        "one pass error ended --watch"
    );
    let sent = std::process::Command::new("kill")
        .args(["-TERM", &watch.id().to_string()])
        .status()
        .expect("kill");
    assert!(sent.success(), "kill -TERM");
    let output = watch.wait_with_output().expect("the watch loop ends");
    assert_eq!(
        output.status.code(),
        Some(0),
        "watch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("radian arc: "), "{stdout}");
    assert!(stdout.contains("boom"), "{stdout}");
}

/// A refusal is reported once and keeps its record: later passes start no gate for the same
/// attempt while the inputs are unchanged. Mutation: write `""` as the attempt of a refused
/// record and every pass starts another gate.
#[test]
fn a_refused_gate_is_reported_once_and_not_restarted() {
    let repo = Repo::new("run-gate-refused");
    let store = setup(&repo);
    let plan = add_plan(&store, "gate-refused");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    let head = publish_verifying(&repo, &store, &plan, &task);
    // Move the remote work branch: the gate's plan refuses, because the remote is no longer
    // the task's `head_sha`.
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &head]);
    write(&repo.clone.join("moved.txt"), "moved\n");
    repo.git(&["add", "moved.txt"]);
    repo.git(&["commit", "-m", "moved"]);
    repo.git(&["push", "origin", &branch]);

    // The first pass starts the gate; it refuses and leaves its record.
    let first = repo.radian(&["arc", "run"]);
    assert_eq!(first.code, 0, "run: {}", first.stderr);
    wait_until(Duration::from_secs(30), "the gate's refusal", || {
        gate_record(&repo, &task).is_some_and(|record| record["status"] == "refused")
    });
    // The gate process exits right after writing the record; let its lock go.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "verifying".to_owned())
    );

    for pass in 1..=2 {
        let run = repo.radian(&["arc", "run"]);
        assert_eq!(run.code, 0, "run {pass}: {}", run.stderr);
        assert!(
            !run.stdout.contains("verify started"),
            "pass {pass} retried the refusal: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains("verify cannot run"),
            "pass {pass} did not report the refusal: {}",
            run.stdout
        );
    }
    assert!(
        gate_record(&repo, &task).is_some_and(|record| record["status"] == "refused"),
        "the refusal record was cleared"
    );
}

// ---- a gate or a wrapper that fails at once -------------------------------------------

/// How many gate processes appended to `runs/<task>/gate.log`: the gate logs one line per
/// start, so this counts the starts rather than the lines a pass printed.
fn gate_starts(repo: &Repo, task: &str) -> usize {
    fs::read_to_string(repo.arc().join("runs").join(task).join("gate.log"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// Runs one-shot passes until a gate record with `status` and these inputs is there, and
/// returns what the passes printed. The first pass that records a no-record gate may be a
/// later pass of the run that started it, or the next run: a gate process takes a moment to
/// exit, so the test does not assume which.
fn run_until_gate_record(
    repo: &Repo,
    task: &str,
    env: &[(&str, String)],
    done: impl Fn(&Value) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut output = String::new();
    loop {
        let run = repo.radian_with_env(&["arc", "run"], env);
        assert_eq!(run.code, 0, "run: {}\n{}", run.stderr, run.stdout);
        output.push_str(&run.stdout);
        if let Some(record) = gate_record(repo, task)
            && done(&record)
        {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "the gate record did not reach the expected state within the limit:\n{output}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A gate process that exits before writing any record is reported once and started at most
/// once per input: one-shot passes settle instead of starting a gate on every pass. Mutation:
/// drop the no-record guard and a `run` starts a gate until it reaches its pass limit.
#[test]
fn a_gate_that_exits_without_a_record_is_reported_once_and_not_restarted() {
    let repo = Repo::new("run-gate-no-record");
    let store = setup(&repo);
    let plan = add_plan(&store, "no-record");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    publish_verifying(&repo, &store, &plan, &task);
    let env = [(background::EXIT_WITHOUT_RECORD_ENV, "1".to_owned())];
    let note = format!("{task} verify cannot run: its gate exited without a record");

    let first = run_until_gate_record(&repo, &task, &env, |record| record["status"] == "no_record");
    assert_eq!(
        first.matches("verify started").count(),
        1,
        "the pass started more than one gate: {first}"
    );
    assert!(first.contains(&note), "the pass did not report it: {first}");
    let record = gate_record(&repo, &task).expect("the record");
    assert_eq!(record["kind"], "verify", "{record}");
    assert_eq!(record["attempt"], format!("{task}-1"), "{record}");
    assert!(
        record["inputs"]
            .as_str()
            .is_some_and(|inputs| !inputs.is_empty()),
        "the record kept the inputs the pass started with: {record}"
    );
    let inputs = record["inputs"].clone();

    // Later passes start nothing and print the same line.
    for pass in 1..=2 {
        let run = repo.radian_with_env(&["arc", "run"], &env);
        assert_eq!(run.code, 0, "pass {pass}: {}", run.stderr);
        assert!(
            !run.stdout.contains("verify started"),
            "pass {pass} restarted the gate: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains(&note),
            "pass {pass} did not report it: {}",
            run.stdout
        );
    }
    assert_eq!(gate_starts(&repo, &task), 1, "the gate ran more than once");
    assert_eq!(
        gate_record(&repo, &task).expect("the record")["inputs"],
        inputs,
        "the record was rewritten"
    );
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "verifying".to_owned())
    );

    // `show` prints the same text as the `rework_reason`.
    let show = repo.radian(&["arc", "show", &task]);
    assert_eq!(show.code, 0, "show: {}", show.stderr);
    assert!(show.stdout.contains("rework_reason"), "{}", show.stdout);
    assert!(
        show.stdout.contains("its gate exited without a record"),
        "{}",
        show.stdout
    );
}

/// A changed head allows one new start: the inputs the no-record record kept no longer match.
#[test]
fn a_changed_head_starts_a_gate_that_left_no_record_once_more() {
    let repo = Repo::new("run-gate-no-record-head");
    let store = setup(&repo);
    let plan = add_plan(&store, "no-record-head");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    publish_verifying(&repo, &store, &plan, &task);
    let env = [(background::EXIT_WITHOUT_RECORD_ENV, "1".to_owned())];

    run_until_gate_record(&repo, &task, &env, |record| record["status"] == "no_record");
    assert_eq!(gate_starts(&repo, &task), 1);
    let before = gate_record(&repo, &task).expect("the record")["inputs"].clone();

    // A new commit on the same attempt: a new head, so a new start, and the pass of the run
    // that records it may be a later one.
    let branch = format!("work/{task}");
    repo.git(&["checkout", &branch]);
    write(&repo.clone.join("second.txt"), "second\n");
    repo.git(&["add", "second.txt"]);
    repo.git(&["commit", "-m", "second"]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", "origin", &branch]);
    store
        .change_task(&task, |task| {
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .expect("the new head");

    let output = run_until_gate_record(&repo, &task, &env, |record| {
        record["status"] == "no_record" && record["inputs"] != before
    });
    assert_eq!(
        output.matches("verify started").count(),
        1,
        "the new head did not start exactly one gate: {output}"
    );
    assert_eq!(
        gate_starts(&repo, &task),
        2,
        "the gate was not started again"
    );
}

/// Under `--watch`, a gate that left no record is not started again across intervals.
#[test]
fn a_watch_pass_does_not_restart_a_gate_that_left_no_record() {
    let repo = Repo::new("run-gate-no-record-watch");
    let store = setup(&repo);
    let plan = add_plan(&store, "no-record-watch");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    publish_verifying(&repo, &store, &plan, &task);
    let env = [(background::EXIT_WITHOUT_RECORD_ENV, "1".to_owned())];

    let watch =
        repo.spawn_in_own_group_with_env(&["arc", "run", "--watch", "--interval", "5"], &env);
    wait_until(Duration::from_secs(60), "the no-record gate record", || {
        gate_record(&repo, &task).is_some_and(|record| record["status"] == "no_record")
    });
    let at = gate_record(&repo, &task).expect("the record")["at"].clone();

    // Two intervals later the gate has not been started again.
    std::thread::sleep(Duration::from_secs(11));
    assert_eq!(
        gate_starts(&repo, &task),
        1,
        "a watch interval restarted the gate"
    );
    assert_eq!(
        gate_record(&repo, &task).expect("the record")["at"],
        at,
        "the no-record record was rewritten across intervals"
    );

    // The loop is in its own process group: signal the group the way Ctrl-C would.
    assert!(
        signal_group(watch.id(), libc::SIGTERM),
        "SIGTERM the watch group"
    );
    let output = watch.wait_with_output().expect("the watch loop ends");
    assert_eq!(
        output.status.code(),
        Some(0),
        "watch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches("verify started").count(),
        1,
        "the watch loop started the gate more than once: {stdout}"
    );
    assert_eq!(gate_starts(&repo, &task), 1);
}

/// A wrapper that refuses an argument from `config.toml` fails after the attempt directory
/// exists, so it leaves its record and the next pass applies it: the slot is free at once and
/// the task waits for its retry. Mutation: let such a failure leave no record and the slot
/// stays held until the stall window.
#[test]
fn a_wrapper_that_refuses_its_arguments_frees_the_slot_in_the_next_pass() {
    let repo = Repo::new("run-wrapper-usage");
    let store = setup(&repo);
    let plan = add_plan(&store, "wrapper-usage");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    // A remote name the wrapper refuses: it never reaches a worker, and it never fetches.
    tune(&repo, "remote = \"origin\"", "remote = \"orig in\"");

    let output = run_until(&repo, Duration::from_secs(30), || {
        state(&store, &a) == ("todo".to_owned(), "retry_wait".to_owned())
            && state(&store, &b).0 == "doing"
    });
    assert_eq!(
        state(&store, &a),
        ("todo".to_owned(), "retry_wait".to_owned()),
        "A keeps the slot until its stall window: {output}"
    );
    assert!(
        output.contains(&format!("{b}-1 started -> doing.running")),
        "the freed slot did not start B: {output}"
    );
    let result: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&a, 1).join("attempt_result.json"))
            .expect("A's attempt result"),
    )
    .expect("the attempt result parses");
    assert_eq!(result["class"], "infrastructure", "{result}");
    assert!(
        result["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("--remote")),
        "the reason names the refused argument: {result}"
    );
    assert_eq!(
        store.load_task(&a).expect("A reads").item.auto_rework,
        0,
        "a retry is not a rework"
    );
}

/// A wrapper whose task worktree is locked by another attempt fails the same way: it records
/// the exit `4` and the next pass applies it, freeing the slot at once.
#[test]
fn a_wrapper_whose_worktree_lock_is_held_frees_the_slot_in_the_next_pass() {
    use fs2::FileExt as _;

    let repo = Repo::new("run-wrapper-locked");
    let store = setup(&repo);
    let plan = add_plan(&store, "wrapper-locked");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    let mock = Mock::start(vec![commit_reply("c1", "b.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    // The worktree a first attempt would have left, held by another attempt.
    let base = repo.main_sha();
    let worktree = repo.worktree(&a);
    fs::create_dir_all(worktree.parent().expect("the worktrees directory"))
        .expect("the worktrees directory");
    let branch = format!("work/{a}");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        &branch,
        worktree.to_str().expect("a utf-8 path"),
        &base,
    ]);
    let lock = fs::File::open(&worktree).expect("the worktree opens");
    lock.lock_exclusive().expect("hold the worktree lock");

    let output = run_until(&repo, Duration::from_secs(30), || {
        state(&store, &a) == ("todo".to_owned(), "retry_wait".to_owned())
            && state(&store, &b).0 == "doing"
    });
    assert_eq!(
        state(&store, &a),
        ("todo".to_owned(), "retry_wait".to_owned()),
        "A keeps the slot until its stall window: {output}"
    );
    assert!(
        output.contains(&format!("{b}-1 started -> doing.running")),
        "the freed slot did not start B: {output}"
    );
    let result: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&a, 1).join("attempt_result.json"))
            .expect("A's attempt result"),
    )
    .expect("the attempt result parses");
    assert_eq!(result["class"], "infrastructure", "{result}");
    assert!(
        result["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("another attempt holds the lock")),
        "the reason names the held worktree lock: {result}"
    );

    // Let the attempt that held the lock go, cancel A so its retry cannot run another worker,
    // and let B finish so the test leaves nothing behind.
    drop(lock);
    let canceled = repo.radian(&["arc", "cancel", &a]);
    assert_eq!(canceled.code, 0, "cancel A: {}", canceled.stderr);
    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &b).1 == "awaiting_human"
    });
}

/// The heartbeat covers the whole wrapper: a push that takes longer than the stall window
/// does not make reconcile call the attempt dead. Mutation: stop the heartbeat when the
/// worker exits and the push phase looks dead.
#[test]
fn a_wrapper_pushing_past_the_stall_window_is_not_judged_dead() {
    let repo = Repo::new("run-slow-push");
    let store = setup(&repo);
    let plan = add_plan(&store, "slow-push");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    tune(&repo, "stall_timeout_secs = 900", "stall_timeout_secs = 5");
    // The wrapper's push runs this as the remote's receive-pack and sleeps past the window.
    let script = repo.root.join("slow-receive-pack.sh");
    fs::write(
        &script,
        "#!/bin/sh\nsleep 12\nexec git-receive-pack \"$@\"\n",
    )
    .expect("the slow receive-pack");
    executable(&script);
    repo.git(&[
        "config",
        "remote.origin.receivepack",
        script.to_str().unwrap(),
    ]);
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let applied = attempt::reconcile(&store).expect("reconcile");
        let finished = repo
            .run_dir(&task, 1)
            .join("attempt_finished.json")
            .is_file();
        if finished {
            break;
        }
        assert!(
            applied.is_empty(),
            "a live wrapper was judged dead while it pushed: {applied:?}"
        );
        assert!(Instant::now() < deadline, "the attempt never finished");
        std::thread::sleep(Duration::from_millis(100));
    }
    run_until(&repo, Duration::from_secs(60), || {
        state(&store, &task).1 == "awaiting_human"
    });
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
}

/// A `--watch` stop reaches the stage the gate runs, not only the gate: the long `commands`
/// child is gone when the loop returns. Mutation: signal only the gate pid and the `sleep`
/// survives.
#[test]
fn sigterm_to_watch_kills_the_running_gate_stage() {
    let repo = Repo::new("run-stop-stage");
    let store = setup(&repo);
    let plan = add_plan(&store, "stop-stage");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    let pid_file = repo.root.join("stage.pid");
    commands(
        &repo,
        &format!("echo $$ > '{}' && sleep 30", pid_file.display()),
    );
    publish_verifying(&repo, &store, &plan, &task);

    let watch = repo.spawn(&["arc", "run", "--watch", "--interval", "30"]);
    wait_for(&pid_file);
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("stage.pid")
        .trim()
        .parse()
        .expect("a pid");
    assert!(process_alive(pid), "the stage did not start");

    let sent = std::process::Command::new("kill")
        .args(["-TERM", &watch.id().to_string()])
        .status()
        .expect("kill");
    assert!(sent.success(), "kill -TERM");
    let deadline = Instant::now() + Duration::from_secs(20);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the stage child (pid {pid}) outlived the stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = watch.wait_with_output().expect("the watch loop ends");
    assert_eq!(
        output.status.code(),
        Some(0),
        "watch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The wall clock covers the wrapper's own push: a remote that never answers is killed at
/// `max_wall_time_secs`, the attempt records `wall_time`, and `start` returns instead of
/// waiting forever. Mutation: start the clock after the worktree setup and leave the git
/// commands unbounded, and `start` never returns.
#[test]
fn a_hanging_push_ends_the_attempt_at_the_wall_time() {
    let repo = Repo::new("run-wall-push");
    let store = setup(&repo);
    let plan = add_plan(&store, "wall-push");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    commands(&repo, "true");
    tune(&repo, "max_wall_time_secs = 7200", "max_wall_time_secs = 3");
    tune(
        &repo,
        "stall_timeout_secs = 900",
        "stall_timeout_secs = 600",
    );
    // The push waits forever: only the wrapper's clock can end it.
    let script = repo.root.join("hanging-receive-pack.sh");
    fs::write(
        &script,
        "#!/bin/sh\nsleep 60\nexec git-receive-pack \"$@\"\n",
    )
    .expect("the hanging receive-pack");
    executable(&script);
    repo.git(&[
        "config",
        "remote.origin.receivepack",
        script.to_str().unwrap(),
    ]);
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    let started = Instant::now();
    let output = run_with_deadline(&repo, &["arc", "start", &task], Duration::from_secs(40));
    assert_eq!(
        output.status.code(),
        Some(0),
        "start: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "start waited {:?} for a hanging push",
        started.elapsed()
    );

    let signal: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&task, 1).join("signal.json")).expect("signal.json"),
    )
    .expect("signal.json parses");
    assert_eq!(signal["reason"], "wall_time", "{signal}");
    let result: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&task, 1).join("attempt_result.json"))
            .expect("attempt_result.json"),
    )
    .expect("attempt_result.json parses");
    assert_eq!(result["class"], "wall_time", "{result}");
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned())
    );
}

/// The gate's lock is the gate's alone: a stage command that leaves a background process
/// behind must not keep `gates/<task>.gate.lock` after the gate exits. Mutation: leave the
/// inherited descriptor inheritable and the orphan holds the lock.
#[test]
fn a_stage_child_does_not_keep_the_gate_lock() {
    let repo = Repo::new("run-gate-lock");
    let store = setup(&repo);
    let plan = add_plan(&store, "gate-lock");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    // The shell exits at once; its `sleep` lives on with whatever descriptors it inherited.
    commands(&repo, "sleep 15 & true");
    publish_verifying(&repo, &store, &plan, &task);

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    wait_until(Duration::from_secs(30), "the gate's record", || {
        gate_record(&repo, &task).is_some_and(|record| record["status"] == "done")
    });
    // The gate exits right after writing the record; an orphan `sleep` must not hold the
    // lock (it lives 15 s, well past this deadline).
    wait_until(Duration::from_secs(8), "the gate lock to be free", || {
        !background::running(&store, &task)
    });
}

/// A terminal Ctrl-C sends SIGINT to the foreground process group. The loop, each wrapper
/// and each gate are in their own group, so only the loop gets it: the worker keeps running
/// for the next loop, and the loop stops the gate, which stops its stage child. Mutation:
/// leave the gate in the loop's group and the SIGINT reaches it directly (the group signal in
/// the second test below no longer names a gate group).
#[test]
fn sigint_to_the_watch_group_leaves_the_worker_running() {
    let repo = Repo::new("run-sigint-group");
    let store = setup(&repo);
    let plan = add_plan(&store, "sigint");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    // The gate's `commands` sleeps only while this marker exists, so the later loop that
    // collects the worker uses a fast gate.
    let slow = repo.root.join("slow-stage");
    fs::write(&slow, "slow\n").expect("the slow marker");
    let stage_pid_file = repo.root.join("stage.pid");
    commands(
        &repo,
        &format!(
            "if [ -f '{}' ]; then echo $$ > '{}'; sleep 30; fi",
            slow.display(),
            stage_pid_file.display()
        ),
    );
    publish_verifying(&repo, &store, &plan, &b);
    // The worker runs until this marker is removed, so it is provably still alive when the
    // loop gets the terminal signal.
    let work_marker = repo.root.join("work-marker");
    fs::write(&work_marker, "work\n").expect("the work marker");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            &format!(
                "while [ -f '{}' ]; do sleep 1; done; echo a > a.txt && git add a.txt && git commit -m a",
                work_marker.display()
            ),
        ),
        text_reply("done"),
    ]);
    worker_args(&repo, mock.base_url());

    let watch = repo.spawn_in_own_group(&["arc", "run", "--watch", "--interval", "30"]);
    let identity_path = repo.attempt_dir(&a, 1).join("identity.json");
    wait_for(&identity_path);
    wait_for(&stage_pid_file);
    let identity: Value =
        serde_json::from_str(&fs::read_to_string(&identity_path).expect("the wrapper's identity"))
            .expect("identity.json parses");
    let wrapper = identity["wrapper_pid"].as_u64().expect("wrapper_pid") as u32;
    let stage: u32 = fs::read_to_string(&stage_pid_file)
        .expect("stage.pid")
        .trim()
        .parse()
        .expect("a pid");

    let sent = signal_group(watch.id(), libc::SIGINT);
    assert!(sent, "kill(-{}, SIGINT)", watch.id());

    let output = watch.wait_with_output().expect("the watch loop ends");
    assert_eq!(
        output.status.code(),
        Some(0),
        "watch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        process_alive(wrapper),
        "the terminal-like SIGINT reached the worker's wrapper"
    );
    wait_until(Duration::from_secs(20), "the stage child to stop", || {
        !process_alive(stage)
    });

    fs::remove_file(&slow).expect("the slow marker is removed");
    fs::remove_file(&work_marker).expect("the work marker is removed");
    wait_for(&repo.run_dir(&a, 1).join("attempt_finished.json"));
    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &a).1 == "awaiting_human" && state(&store, &b).1 == "awaiting_human"
    });
}

/// SIGINT aimed at the gate alone (its own process group) stops its stage child exactly as
/// SIGTERM does. Mutation: without the gate's own process group, the group signal finds no
/// such group.
#[test]
fn sigint_to_a_gate_stops_its_stage() {
    let repo = Repo::new("run-sigint-gate");
    let store = setup(&repo);
    let plan = add_plan(&store, "sigint-gate");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    let stage_pid_file = repo.root.join("stage.pid");
    commands(
        &repo,
        &format!("echo $$ > '{}' && sleep 30", stage_pid_file.display()),
    );
    publish_verifying(&repo, &store, &plan, &task);

    let watch = repo.spawn(&["arc", "run", "--watch", "--interval", "30"]);
    wait_for(&stage_pid_file);
    let stage: u32 = fs::read_to_string(&stage_pid_file)
        .expect("stage.pid")
        .trim()
        .parse()
        .expect("a pid");
    let record = gate_record(&repo, &task).expect("the gate record");
    let gate = record["pid"].as_u64().expect("the gate pid") as u32;

    // The gate leads its own process group; the stage child is in another, so this reaches
    // the gate alone.
    let sent = signal_group(gate, libc::SIGINT);
    assert!(sent, "kill(-{gate}, SIGINT)");
    wait_until(Duration::from_secs(20), "the stage child to stop", || {
        !process_alive(stage)
    });
    wait_until(Duration::from_secs(20), "the gate to exit", || {
        !process_alive(gate)
    });

    let sent = std::process::Command::new("kill")
        .args(["-TERM", &watch.id().to_string()])
        .status()
        .expect("kill -TERM");
    assert!(sent.success(), "kill -TERM");
    let output = watch.wait_with_output().expect("the watch loop ends");
    assert_eq!(
        output.status.code(),
        Some(0),
        "watch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs one `radian` command with a deadline, so a hung `start` fails the test instead of
/// hanging until nextest's own timeout.
fn run_with_deadline(repo: &Repo, args: &[&str], limit: Duration) -> std::process::Output {
    let mut child = repo.spawn(args);
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return child.wait_with_output().expect("the command's output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("the killed command's output");
            panic!(
                "`{}` did not finish within {limit:?}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---- automatic reverify ---------------------------------------------------------------

/// Puts `task` in `review.rework_pending` with the records a gate leaves when `stage` fails
/// with `reason`: `review.json` in the attempt's run directory, and the verdict file it
/// names. Returns the task's `head_sha`.
fn record_failure(
    repo: &Repo,
    store: &Store,
    plan: &str,
    task: &str,
    stage: Stage,
    reason: &str,
) -> String {
    let head = publish_verifying(repo, store, plan, task);
    let main = repo.main_sha();
    let verdicts = repo.run_dir(task, 1).join("verdicts");
    fs::create_dir_all(&verdicts).expect("the verdict directory");
    let mut verdict = Verdict::human(head.clone(), false, Some(reason.to_owned()));
    verdict.stage = stage;
    if stage == Stage::Integration {
        verdict.base_sha = Some(main);
    } else {
        verdict.main_sha = Some(main);
    }
    let path = verdicts.join(format!("{}.json", stage.as_str()));
    verdict.write_new(&path).expect("the verdict");
    let reason_file = serde_json::json!({
        "action": if stage == Stage::Integration { "merge" } else { "verify" },
        "stage": stage.as_str(),
        "reason": reason,
        "verdict": path.to_string_lossy(),
        "text": format!("{stage}: {reason}"),
        "at": "2026-01-01T00:00:00Z",
    });
    fs::write(
        repo.run_dir(task, 1).join("review.json"),
        reason_file.to_string(),
    )
    .expect("review.json");
    store
        .change_task(task, |task| {
            task.phase = "rework_pending".to_owned();
            Ok(())
        })
        .expect("rework_pending");
    head
}

/// Commits one file on `main` in the clone, pushes it to the remote, and returns the commit
/// id.
fn advance_main(repo: &Repo, file: &str, content: &str) -> String {
    repo.git(&["checkout", "main"]);
    write(&repo.clone.join(file), content);
    repo.git(&["add", file]);
    repo.git(&["commit", "-m", file]);
    let sha = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", &repo.remote, &repo.main_branch]);
    sha
}

/// The `attempt` field of a task.
fn attempt(store: &Store, task: &str) -> u32 {
    store.load_task(task).expect("the task reads").item.attempt
}

/// A `commands` failure that main fixes is re-verified by the next pass, for the same commit,
/// and the task passes. Mutation: drop the step and the task waits in `review.rework_pending`.
#[test]
fn a_command_failure_is_reverified_automatically_once_main_moves() {
    let repo = Repo::new("run-auto-reverify");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-reverify");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    // The tree is broken while the marker exists: the failure is main's, not the change's.
    let marker = repo.root.join("broken-main");
    fs::write(&marker, "broken\n").expect("the marker");
    commands(&repo, &format!("test ! -f '{}'", marker.display()));
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    let first = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "rework_pending"
    });
    assert!(
        first.contains(&format!("{task}-1 verify -> review.rework_pending")),
        "{first}"
    );
    let head = store
        .load_task(&task)
        .expect("the task reads")
        .item
        .head_sha
        .expect("a head");
    assert_eq!(attempt(&store, &task), 1, "{first}");

    fs::remove_file(&marker).expect("the marker is removed");
    advance_main(&repo, "main-fix.txt", "fixed\n");
    let moved = repo.remote_main_sha();
    let short = &moved[..7];
    let run = run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "awaiting_human"
    });
    assert!(
        run.contains(&format!("{task} main moved ({short}) -> auto reverify")),
        "{run}"
    );
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human"),
        "{run}"
    );
    assert_eq!(
        stored.verified_sha.as_deref(),
        Some(head.as_str()),
        "the same commit was re-verified: {run}"
    );
    assert_eq!(stored.attempt, 2, "one worker-less attempt: {run}");
    assert_eq!(
        stored.auto_rework, 1,
        "the automatic reverify is not an automatic rework: {run}"
    );
    let result: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&task, 2).join("attempt_result.json"))
            .expect("attempt_result.json"),
    )
    .expect("attempt_result.json parses");
    assert_eq!(result["class"], "reverify", "{result}");
    assert!(
        !repo.attempt_dir(&task, 2).join("identity.json").exists(),
        "the automatic reverify started a worker"
    );
}

/// While main stands still, the failure is not re-verified: nothing runs again.
#[test]
fn a_command_failure_is_not_reverified_while_main_stands_still() {
    let repo = Repo::new("run-auto-reverify-still");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-still");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "false");
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "rework_pending"
    });
    for pass in 1..=2 {
        let run = repo.radian(&["arc", "run"]);
        assert_eq!(run.code, 0, "pass {pass}: {}", run.stderr);
        assert!(
            !run.stdout.contains("auto reverify"),
            "pass {pass} re-verified a failure main did not move for: {}",
            run.stdout
        );
    }
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "rework_pending".to_owned())
    );
    assert_eq!(attempt(&store, &task), 1);
    assert!(!repo.run_dir(&task, 2).exists());
}

/// A recorded `(head, main)` pair is re-verified automatically at most once. The state below
/// is the one a pass leaves when it dies between recording its inputs and claiming the
/// attempt: the failing reason still names the older main, so only the record stops the pair
/// from running again. Mutation: drop the pair guard and the next pass claims an attempt.
///
/// The "main did not move" half of the guard is the same test: the reason's main equals the
/// remote main after a crash, so nothing runs either way.
#[test]
fn the_same_head_and_main_are_reverified_automatically_once() {
    let repo = Repo::new("run-auto-reverify-pair");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-pair");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "false");
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "rework_pending"
    });
    let head = store
        .load_task(&task)
        .expect("the task reads")
        .item
        .head_sha
        .expect("a head");
    advance_main(&repo, "main-move.txt", "moved\n");
    let moved = repo.remote_main_sha();

    // The pass records the inputs it starts the automatic reverify with before it claims the
    // attempt (ADR 0048 decision 9). The record is the guard: the same pair never runs twice.
    let record = serde_json::json!({
        "reverified": [{
            "head_sha": head,
            "main_sha": moved,
            "at": "2026-01-01T00:00:00Z",
        }],
    });
    let path = repo
        .arc()
        .join("runs")
        .join(&task)
        .join("auto_reverify.json");
    fs::create_dir_all(path.parent().expect("the runs directory")).expect("the runs directory");
    fs::write(&path, format!("{record}\n")).expect("auto_reverify.json");

    for pass in 1..=2 {
        let run = repo.radian(&["arc", "run"]);
        assert_eq!(run.code, 0, "pass {pass}: {}", run.stderr);
        assert!(
            !run.stdout.contains("auto reverify"),
            "pass {pass} re-ran the same pair: {}",
            run.stdout
        );
    }
    assert_eq!(attempt(&store, &task), 1);
    assert!(!repo.run_dir(&task, 2).exists());
    let stored: Value = serde_json::from_str(&fs::read_to_string(&path).expect("the record"))
        .expect("auto_reverify.json parses");
    assert_eq!(
        stored["reverified"].as_array().map(Vec::len),
        Some(1),
        "the record changed: {stored}"
    );
}

/// `max_auto_reverify` stops the step: the task stays in `review.rework_pending`, `run`
/// prints the reason, and `show` prints the same text as the `rework_reason`. Mutation: drop
/// the count and the third main move claims another attempt.
#[test]
fn max_auto_reverify_stops_the_step_and_says_why() {
    let repo = Repo::new("run-auto-reverify-limit");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-limit");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    tune(&repo, "max_auto_reverify = 2", "max_auto_reverify = 1");
    tune(&repo, "max_auto_rework = 2", "max_auto_rework = 10");
    commands(&repo, "false");
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "rework_pending"
    });
    advance_main(&repo, "main-one.txt", "one\n");
    let first = run_until(&repo, Duration::from_secs(90), || {
        attempt(&store, &task) == 2 && state(&store, &task).1 == "rework_pending"
    });
    assert!(first.contains("auto reverify"), "{first}");

    advance_main(&repo, "main-two.txt", "two\n");
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout.contains("auto reverify limit reached"),
        "run did not report the count: {}",
        run.stdout
    );
    let text = "the task used 1 automatic reverifies and `[limits] max_auto_reverify` is 1";
    assert!(run.stdout.contains(text), "{}", run.stdout);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "rework_pending".to_owned()),
        "{}",
        run.stdout
    );
    assert_eq!(attempt(&store, &task), 2, "{}", run.stdout);
    assert!(!repo.run_dir(&task, 3).exists());
    assert_eq!(plan_auto(&store, &plan), "run", "the plan was paused");

    let shown = repo.radian(&["arc", "show", &task]);
    assert_eq!(shown.code, 0, "show: {}", shown.stderr);
    assert!(shown.stdout.contains("rework_reason"), "{}", shown.stdout);
    assert!(
        shown.stdout.contains(text),
        "show does not print the same text: {}",
        shown.stdout
    );
    // A second pass for the same pair reports nothing more.
    let again = repo.radian(&["arc", "run"]);
    assert_eq!(again.code, 0, "run: {}", again.stderr);
    assert!(
        !again.stdout.contains("auto reverify"),
        "the count was reported twice: {}",
        again.stdout
    );
}

/// The owner's `reverify` ignores both guards: it runs after the count is used up and does
/// not use up the count itself.
#[test]
fn a_hand_reverify_still_runs_after_the_count_is_used_up() {
    let repo = Repo::new("run-auto-reverify-hand");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-hand");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    tune(&repo, "max_auto_reverify = 2", "max_auto_reverify = 1");
    tune(&repo, "max_auto_rework = 2", "max_auto_rework = 10");
    let marker = repo.root.join("broken-main");
    fs::write(&marker, "broken\n").expect("the marker");
    commands(&repo, &format!("test ! -f '{}'", marker.display()));
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "rework_pending"
    });
    advance_main(&repo, "main-one.txt", "one\n");
    run_until(&repo, Duration::from_secs(90), || {
        attempt(&store, &task) == 2 && state(&store, &task).1 == "rework_pending"
    });
    advance_main(&repo, "main-two.txt", "two\n");
    let limited = repo.radian(&["arc", "run"]);
    assert_eq!(limited.code, 0, "run: {}", limited.stderr);
    assert!(
        limited.stdout.contains("auto reverify limit reached"),
        "{}",
        limited.stdout
    );

    // The tree is fixed: the owner's reverify re-runs the gate for the same commit.
    fs::remove_file(&marker).expect("the marker is removed");
    let reverified = repo.radian(&["arc", "reverify", &task]);
    assert_eq!(reverified.code, 0, "reverify: {}", reverified.stderr);
    assert!(
        reverified
            .stdout
            .contains(&format!("{task}-3 reverify -> review.verifying")),
        "{}",
        reverified.stdout
    );
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        reverified.stdout
    );
    let record: Value = serde_json::from_str(
        &fs::read_to_string(
            repo.arc()
                .join("runs")
                .join(&task)
                .join("auto_reverify.json"),
        )
        .expect("auto_reverify.json"),
    )
    .expect("auto_reverify.json parses");
    assert_eq!(
        record["reverified"].as_array().map(Vec::len),
        Some(1),
        "the hand reverify used up the count: {record}"
    );
}

/// A review stage failure, a conflict, a delivery failure, and a task of a plan at `auto`
/// `off` are never re-verified by themselves. Mutation: also reverify a review failure and
/// the first task gains an attempt.
#[test]
fn only_a_command_failure_of_an_auto_plan_is_reverified() {
    let repo = Repo::new("run-auto-reverify-not");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-not");
    let reviewed = add_task(&store, &plan, "reviewed", &[]);
    let conflicted = add_task(&store, &plan, "conflicted", &[]);
    let undelivered = add_task(&store, &plan, "undelivered", &[]);
    let off_plan = add_plan(&store, "auto-off");
    let off = add_task(&store, &off_plan, "off", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::confirm(&store, &off_plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    record_failure(
        &repo,
        &store,
        &plan,
        &reviewed,
        Stage::Review,
        "review_blocking: it is wrong",
    );
    record_failure(
        &repo,
        &store,
        &plan,
        &conflicted,
        Stage::Commands,
        "conflict: feature.txt",
    );
    // A delivery failure records no reason at all: arc writes no brief for it.
    publish_verifying(&repo, &store, &plan, &undelivered);
    let result = serde_json::json!({
        "attempt_id": format!("{undelivered}-1"),
        "outcome": "completed",
        "class": "verification",
        "reason": "the tree is dirty",
        "state": "review",
        "phase": "rework_pending",
    });
    let dir = repo.run_dir(&undelivered, 1);
    fs::create_dir_all(&dir).expect("the run directory");
    fs::write(dir.join("attempt_result.json"), result.to_string()).expect("attempt_result.json");
    store
        .change_task(&undelivered, |task| {
            task.phase = "rework_pending".to_owned();
            Ok(())
        })
        .expect("rework_pending");
    record_failure(
        &repo,
        &store,
        &off_plan,
        &off,
        Stage::Commands,
        "command_failed",
    );

    advance_main(&repo, "main-move.txt", "moved\n");
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        !run.stdout.contains("auto reverify"),
        "a task that is not a command failure was re-verified: {}",
        run.stdout
    );
    for task in [&reviewed, &conflicted, &undelivered, &off] {
        assert_eq!(
            state(&store, task),
            ("review".to_owned(), "rework_pending".to_owned()),
            "{task}: {}",
            run.stdout
        );
        assert_eq!(attempt(&store, task), 1, "{task}: {}", run.stdout);
    }
}

/// A failed integration key command is re-verified automatically too, once main moves.
#[test]
fn a_failed_integration_is_reverified_automatically_once_main_moves() {
    let repo = Repo::new("run-auto-reverify-integration");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-integration");
    let task = add_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    commands(&repo, "true");
    let marker = repo.root.join("broken-main");
    fs::write(&marker, "broken\n").expect("the marker");
    key_commands(&repo, &format!("test ! -f '{}'", marker.display()));
    let mock = Mock::start(vec![commit_reply("c1", "change.txt"), text_reply("done")]);
    worker_args(&repo, mock.base_url());

    run_until(&repo, Duration::from_secs(90), || {
        state(&store, &task).1 == "awaiting_human"
    });
    let merged = repo.radian(&["arc", "merge", &task]);
    assert_eq!(merged.code, 0, "merge: {}", merged.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "rework_pending".to_owned()),
        "{}",
        merged.stdout
    );
    let reason: Value = serde_json::from_str(
        &fs::read_to_string(repo.run_dir(&task, 1).join("review.json")).expect("review.json"),
    )
    .expect("review.json parses");
    assert_eq!(reason["stage"], "integration", "{reason}");
    assert_eq!(reason["reason"], "command_failed", "{reason}");

    fs::remove_file(&marker).expect("the marker is removed");
    advance_main(&repo, "main-fix.txt", "fixed\n");
    let run = run_until(&repo, Duration::from_secs(90), || {
        attempt(&store, &task) == 2 && state(&store, &task).1 == "awaiting_human"
    });
    assert!(run.contains("auto reverify"), "{run}");
    assert_eq!(attempt(&store, &task), 2, "{run}");
}

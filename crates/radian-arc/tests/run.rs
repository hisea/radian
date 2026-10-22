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

fn utc(text: &str) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::parse_from_rfc3339(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

// ---- auto levels ----------------------------------------------------------------------

/// At auto `run`, one `run` takes A to `awaiting_human` and stops: B waits on A's merge. After
/// `merge A`, the next `run` takes B.
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

    let first = repo.radian(&["arc", "run"]);
    assert_eq!(first.code, 0, "run failed: {}", first.stderr);
    assert!(
        first
            .stdout
            .contains(&format!("{a}-1 verify -> review.awaiting_human")),
        "{}",
        first.stdout
    );
    assert_eq!(
        state(&store, &a),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
    assert_eq!(state(&store, &b), ("todo".to_owned(), "ready".to_owned()));
    assert_eq!(state(&store, &c), ("todo".to_owned(), "ready".to_owned()));
    assert!(
        first
            .stdout
            .contains(&format!("{b} todo.ready: waiting for {a} to merge")),
        "{}",
        first.stdout
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

    let second = repo.radian(&["arc", "run"]);
    assert_eq!(second.code, 0, "run failed: {}", second.stderr);
    assert_eq!(
        state(&store, &b),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
    assert_eq!(state(&store, &c), ("todo".to_owned(), "ready".to_owned()));
    assert_eq!(state(&store, &a), ("done".to_owned(), "merged".to_owned()));
}

/// At auto `merge`, one `run` takes all three to `done.merged`, in order.
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

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run failed: {}", run.stderr);
    for task in [&a, &b, &c] {
        assert_eq!(
            state(&store, task),
            ("done".to_owned(), "merged".to_owned()),
            "{task}: {}",
            run.stdout
        );
    }
    let merged_lines: Vec<&str> = run
        .stdout
        .lines()
        .filter(|line| line.contains("done.merged"))
        .collect();
    assert_eq!(merged_lines.len(), 3, "{}", run.stdout);
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

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run failed: {}", run.stderr);
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
        run.stdout
            .contains(&format!("{ready} todo.ready: waiting for you to start it")),
        "{}",
        run.stdout
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

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run failed: {}", run.stderr);
    for task in [&a, &b] {
        assert_eq!(
            state(&store, task),
            ("review".to_owned(), "awaiting_human".to_owned()),
            "{task}: {}",
            run.stdout
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
        let run = repo.radian(&["arc", "run"]);
        assert_eq!(run.code, 0, "run {attempt}: {}", run.stderr);
        let stored = store.load_task(&task).expect("the task reads").item;
        assert_eq!(
            (stored.state.as_str(), stored.phase.as_str()),
            ("review", "rework_pending"),
            "attempt {attempt}: {}",
            run.stdout
        );
        assert_eq!(stored.auto_rework, attempt);
        if attempt <= 2 {
            assert_eq!(plan_auto(&store, &plan), "run", "attempt {attempt}");
            plan_ops::add_rework(&store, &task, "try again").expect("a rework brief");
        } else {
            assert_eq!(plan_auto(&store, &plan), "off", "attempt {attempt}");
            assert!(
                run.stdout.contains("auto rework limit"),
                "the pause says why: {}",
                run.stdout
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

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run failed: {}", run.stderr);
    assert_eq!(plan_auto(&store, &plan), "off", "{}", run.stdout);
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

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human"),
        "{}",
        run.stdout
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

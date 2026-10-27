//! The attempt lifecycle (ADR 0048 decisions 4, 6, 9 and 12): claim, apply, retry, budgets,
//! cancel and reconcile. Store-level cases drive the library against a temp repository; the
//! end-to-end cases run the real `radian arc start`/`cancel` with the mock model.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::Reply;
use common::*;
use radian_arc::attempt;
use radian_arc::store::attempt_ops::{self, Class, Finish, WorkerLine};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Exit, Store, plan_ops};
use serde_json::{Value, json};

fn setup(prefix: &str) -> (Repo, Store) {
    let repo = Repo::new(prefix);
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    (repo, store)
}

fn new_plan(store: &Store, slug: &str) -> String {
    store
        .add_plan(slug, "A plan", "the request\n")
        .expect("add a plan")
}

fn new_task(store: &Store, plan: &str, slug: &str, after: &[&str]) -> String {
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

fn confirmed_plan_with_tasks(store: &Store, prefix: &str, slugs: &[&str]) -> (String, Vec<String>) {
    let plan = new_plan(store, prefix);
    let tasks = slugs
        .iter()
        .map(|slug| new_task(store, &plan, slug, &[]))
        .collect();
    radian_arc::store::plan_ops::confirm(store, &plan).expect("confirm");
    (plan, tasks)
}

fn finish(
    store: &Store,
    task: &str,
    attempt: u32,
    outcome: &str,
    pushed: bool,
    failure: Option<&str>,
) -> Finish {
    let token = store
        .load_task(task)
        .expect("the task reads")
        .item
        .token
        .expect("the claim wrote a token");
    Finish {
        attempt_id: format!("{task}-{attempt}"),
        token,
        exit_code: 0,
        outcome: outcome.to_owned(),
        base_sha: Some("a".repeat(40)),
        head_sha: Some("b".repeat(40)),
        pushed,
        failure: failure.map(str::to_owned),
    }
}

fn write_stream(store: &Store, task: &str, attempt: u32, micros: Option<u64>) {
    let dir = attempt_ops::run_dir(store, task, attempt);
    std::fs::create_dir_all(&dir).unwrap();
    let cost = match micros {
        Some(micros) => json!({"micros": micros, "incomplete": false, "estimated": false}),
        None => Value::Null,
    };
    std::fs::write(
        dir.join("result.jsonl"),
        format!(
            "{}\n",
            json!({"type": "Result", "outcome": "failed", "cost": cost})
        ),
    )
    .unwrap();
}

/// One attempt's jsonl stream with a failed `Result` and, when given, its `model_error`.
fn write_result_stream(store: &Store, task: &str, attempt: u32, model_error: Option<&Value>) {
    let dir = attempt_ops::run_dir(store, task, attempt);
    std::fs::create_dir_all(&dir).unwrap();
    let mut result = json!({
        "type": "Result",
        "outcome": "failed",
        "cost": {"micros": 1000, "incomplete": false, "estimated": false},
    });
    if let Some(error) = model_error {
        result["model_error"] = error.clone();
    }
    std::fs::write(dir.join("result.jsonl"), format!("{result}\n")).unwrap();
}

/// The worker kind an attempt's `execution.json` records (ADR 0048 decision 7).
fn write_worker_kind(store: &Store, task: &str, attempt: u32, kind: &str) {
    let dir = attempt_ops::run_dir(store, task, attempt);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("execution.json"),
        json!({"worker": {"kind": kind}}).to_string(),
    )
    .unwrap();
}

/// The attempt's `execution.json`, which the wrapper writes: the cost rule reads the session
/// id from it to tell an attempt that shares the previous session from one with its own.
fn write_execution(store: &Store, task: &str, attempt: u32, session: &str) {
    let dir = attempt_ops::run_dir(store, task, attempt);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("execution.json"),
        json!({
            "attempt_id": format!("{task}-{attempt}"),
            "session_id": session,
        })
        .to_string(),
    )
    .unwrap();
}

fn attempt_result(store: &Store, task: &str, attempt: u32) -> Value {
    let path = attempt_ops::run_dir(store, task, attempt).join("attempt_result.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("attempt_result.json")).unwrap()
}

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

fn utc_seconds(text: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(text)
        .expect("a stored UTC time")
        .timestamp()
}

// ---- claim ----------------------------------------------------------------------------

/// Decision 6: a task starts only from `todo.ready` or the owner's `start` from
/// `todo.retry_wait`, only under a confirmed plan, only when every `after` task is `done`, and
/// only when a concurrency slot is free. Each refusal is exit 3 and writes nothing.
#[test]
fn claim_refusals_are_exit_three_and_write_nothing() {
    let (_repo, store) = setup("claim");
    let plan = new_plan(&store, "gate");
    let first = new_task(&store, &plan, "first", &[]);
    let second = new_task(&store, &plan, "second", &[&first]);
    let third = new_task(&store, &plan, "third", &[]);

    // A draft plan cannot start anything.
    let refusal = attempt_ops::claim(&store, &first).expect_err("a draft plan is refused");
    assert_eq!(refusal.exit(), Exit::State, "{refusal:?}");
    assert!(refusal.message().contains("confirm"), "{refusal:?}");
    assert_eq!(state(&store, &first), ("todo".into(), "ready".into()));

    radian_arc::store::plan_ops::confirm(&store, &plan).expect("confirm");

    // `after` is not done yet.
    let refusal = attempt_ops::claim(&store, &second).expect_err("an unfinished after task");
    assert_eq!(refusal.exit(), Exit::State, "{refusal:?}");
    assert!(refusal.message().contains("after"), "{refusal:?}");

    // One claim takes the only slot (`concurrency` defaults to 1).
    let claim = attempt_ops::claim(&store, &first).expect("the first task starts");
    assert_eq!(claim.attempt, 1);
    assert_eq!(claim.attempt_id, format!("{first}-1"));
    assert!(!claim.resume);
    assert_eq!(
        claim.session_id,
        format!("{first}-1"),
        "a fresh first attempt"
    );
    assert_eq!(state(&store, &first), ("doing".into(), "starting".into()));
    let stored = store.load_task(&first).unwrap().item;
    assert_eq!(
        stored.attempt_id.as_deref(),
        Some(claim.attempt_id.as_str())
    );
    assert_eq!(stored.token.as_deref(), Some(claim.token.as_str()));

    let refusal = attempt_ops::claim(&store, &third).expect_err("the slot is taken");
    assert_eq!(refusal.exit(), Exit::State, "{refusal:?}");
    assert!(refusal.message().contains("concurrency"), "{refusal:?}");
}

/// ADR 0048 decision 4: with `fresh` (the default) every attempt has a session of its own,
/// `<task>-<attempt>`, so a rework claims a new one and never resumes. Mutation: `resume:
/// number > 1` (pass `--resume` with `fresh`) and the second claim resumes.
#[test]
fn a_fresh_claim_names_each_attempts_session_and_never_resumes() {
    let (_repo, store) = setup("fresh-claim");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    let first = attempt_ops::claim(&store, task).expect("claim 1");
    assert_eq!(first.session_id, format!("{task}-1"));
    assert!(!first.resume);
    let finish = finish(&store, task, 1, "failed", false, None);
    attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");

    let retry_at = store.load_task(task).unwrap().item.retry_at.unwrap();
    attempt_ops::release_retries(&store, &retry_at).expect("release");
    let second = attempt_ops::claim(&store, task).expect("claim 2");
    assert_eq!(
        second.session_id,
        format!("{task}-2"),
        "the attempt's own session"
    );
    assert!(!second.resume, "a fresh attempt never resumes");
}

/// `resume` keeps the old behavior: one session per task, the task id, and every attempt after
/// the first continues it.
#[test]
fn a_resume_claim_continues_the_task_session() {
    let (repo, store) = setup("resume-claim");
    tune(
        &repo,
        &[("rework_session = \"fresh\"", "rework_session = \"resume\"")],
    );
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    let first = attempt_ops::claim(&store, task).expect("claim 1");
    assert_eq!(first.session_id, task.as_str(), "the task's one session");
    assert!(!first.resume, "the first attempt starts the session");
    let finish = finish(&store, task, 1, "failed", false, None);
    attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");

    let retry_at = store.load_task(task).unwrap().item.retry_at.unwrap();
    attempt_ops::release_retries(&store, &retry_at).expect("release");
    let second = attempt_ops::claim(&store, task).expect("claim 2");
    assert_eq!(second.session_id, task.as_str(), "the same session");
    assert!(second.resume, "a resume attempt continues the session");
}

/// ADR 0048 decision 4: the claim writes the session it decided on to `start.json`, so
/// `attempt_session_id` and `show --json` name a running attempt's session before the wrapper
/// has written `identity.json` or `execution.json`. Mutation: the claim omits `session_id`
/// from `start.json`.
#[test]
fn a_claim_makes_the_attempts_session_id_known_before_the_worker_ends() {
    let (repo, store) = setup("claim-session");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    let claim = attempt_ops::claim(&store, task).expect("claim");
    assert_eq!(claim.session_id, format!("{task}-1"));
    // No wrapper record exists yet: only the claim's own record names the session.
    assert!(
        !attempt_ops::run_dir(&store, task, 1)
            .join("execution.json")
            .is_file()
    );
    assert!(
        !attempt_ops::attempt_dir(&store, task, 1)
            .join("identity.json")
            .is_file()
    );
    assert_eq!(
        attempt_ops::attempt_session_id(&store, task, 1).as_deref(),
        Some(claim.session_id.as_str())
    );

    let shown = repo.radian(&["arc", "show", task, "--json"]);
    assert_eq!(shown.code, 0, "show: {}", shown.stderr);
    let value: Value = serde_json::from_str(&shown.stdout).expect("show --json is JSON");
    assert_eq!(value["session_id"], json!(claim.session_id));
    let text = repo.radian(&["arc", "show", task]);
    assert_eq!(text.code, 0, "show: {}", text.stderr);
    assert!(
        text.stdout.contains(&format!("session_id: {task}-1")),
        "the text form prints the running attempt's session:\n{}",
        text.stdout
    );
}

/// `resume` keeps one session per task, and the claim records that task id before the worker
/// ends: `attempt_session_id` and `show --json` answer it at once.
#[test]
fn a_resume_claim_makes_the_task_session_known_before_the_worker_ends() {
    let (repo, store) = setup("claim-session-resume");
    tune(
        &repo,
        &[("rework_session = \"fresh\"", "rework_session = \"resume\"")],
    );
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    attempt_ops::claim(&store, task).expect("claim");
    assert_eq!(
        attempt_ops::attempt_session_id(&store, task, 1).as_deref(),
        Some(task.as_str()),
        "the task's one session"
    );
    let shown = repo.radian(&["arc", "show", task, "--json"]);
    assert_eq!(shown.code, 0, "show: {}", shown.stderr);
    let value: Value = serde_json::from_str(&shown.stdout).expect("show --json is JSON");
    assert_eq!(value["session_id"], json!(task));
}

/// ADR 0048 decision 4: the claim writes the worker it decided on to `start.json`, so a reader
/// can tell a command worker from `radian acp` before the wrapper has written any record, and a
/// later `[worker] command` change cannot re-classify the attempt.
///
/// Mutation: the claim omits `worker` from `start.json`.
#[test]
fn a_claim_records_the_worker_it_decided_on() {
    let (repo, store) = setup("claim-worker");
    tune(&repo, &[("concurrency = 1", "concurrency = 2")]);
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["radian", "command"]);
    let start = |task: &str| -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(
                attempt_ops::attempt_dir(&store, task, 1).join(attempt_ops::START_FILE),
            )
            .expect("start.json"),
        )
        .expect("start.json parses")
    };

    let first = attempt_ops::claim(&store, &tasks[0]).expect("the first claim");
    assert_eq!(
        first.worker.record(),
        start(&tasks[0])["worker"],
        "the claim and `start.json` carry the same worker"
    );
    assert_eq!(
        start(&tasks[0])["worker"],
        json!({"kind": "radian", "flow": "default"}),
        "the default worker is `radian acp`"
    );

    // The key takes effect at the next claim: this attempt is the command worker, and the
    // program is recorded as the wrapper will run it (a path with a `/` made absolute).
    tune(
        &repo,
        &[(
            "# command = [\"codex\", \"exec\"]",
            "command = [\"tools/codex\"]",
        )],
    );
    let second = attempt_ops::claim(&store, &tasks[1]).expect("the second claim");
    assert_eq!(
        second.worker.record(),
        start(&tasks[1])["worker"],
        "the claim and `start.json` carry the same worker"
    );
    let program = repo
        .canonical_clone()
        .join("tools/codex")
        .display()
        .to_string();
    assert_eq!(
        second.worker,
        WorkerLine::Command {
            command: vec![program.clone()],
            flow: "default".to_owned(),
        },
        "the claim carries the resolved command the wrapper starts"
    );
    assert_eq!(
        start(&tasks[1])["worker"],
        json!({
            "kind": "command",
            "flow": "default",
            "program": program,
        }),
        "the claim names the command worker the wrapper will start"
    );
}

/// The wrapper writes `identity.json` when it spawns the worker and `execution.json` only when
/// it ends. A reader with no `execution.json` answers from `identity.json`, and an
/// `execution.json` that exists wins over it (ADR 0048 decision 4).
#[test]
fn a_session_id_is_read_from_the_identity_record_when_execution_is_missing() {
    let (_repo, store) = setup("identity-session");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    let dir = attempt_ops::attempt_dir(&store, task, 1);
    // The claim's record carries no field; the wrapper's identity record names the session.
    std::fs::write(
        dir.join(attempt_ops::START_FILE),
        json!({"attempt_id": format!("{task}-1")}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("identity.json"),
        json!({"session_id": format!("{task}-1")}).to_string(),
    )
    .unwrap();
    assert_eq!(
        attempt_ops::attempt_session_id(&store, task, 1).as_deref(),
        Some(format!("{task}-1").as_str())
    );
    // The end record is the first one a reader takes.
    let run = attempt_ops::run_dir(&store, task, 1);
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(
        run.join("execution.json"),
        json!({"session_id": "from-execution"}).to_string(),
    )
    .unwrap();
    assert_eq!(
        attempt_ops::attempt_session_id(&store, task, 1).as_deref(),
        Some("from-execution")
    );
}

/// An old attempt's `start.json` has no session id and no wrapper record exists: the answer is
/// `None`, and `show --json` prints `null` rather than guessing the task id.
#[test]
fn an_attempt_with_no_record_of_its_session_answers_none() {
    let (repo, store) = setup("no-session");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let claim = attempt_ops::claim(&store, task).expect("claim");
    std::fs::write(
        attempt_ops::attempt_dir(&store, task, 1).join(attempt_ops::START_FILE),
        json!({"attempt_id": claim.attempt_id}).to_string(),
    )
    .unwrap();

    assert_eq!(attempt_ops::attempt_session_id(&store, task, 1), None);
    let shown = repo.radian(&["arc", "show", task, "--json"]);
    assert_eq!(shown.code, 0, "show: {}", shown.stderr);
    let value: Value = serde_json::from_str(&shown.stdout).expect("show --json is JSON");
    assert_eq!(value["session_id"], Value::Null);
}

// ---- apply ----------------------------------------------------------------------------

/// A pushed `completed` attempt goes to `review.verifying` with the attempt's shas.
#[test]
fn a_completed_pushed_result_moves_to_review_verifying_with_the_shas() {
    let (_repo, store) = setup("verifying");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    let finish = finish(&store, task, 1, "completed", true, None);
    let applied = attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(0)).expect("apply");
    assert_eq!(applied.class, None);
    assert_eq!(
        (applied.state.as_str(), applied.phase.as_str()),
        ("review", "verifying")
    );
    let stored = store.load_task(task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "verifying")
    );
    assert_eq!(stored.base_sha, finish.base_sha);
    assert_eq!(stored.head_sha, finish.head_sha);
}

/// A `completed` child whose delivery check failed is a `verification` result: the task goes
/// to `review.rework_pending`, and arc does not write a brief.
#[test]
fn a_completed_but_undelivered_result_goes_to_rework_pending() {
    let (_repo, store) = setup("rework");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let brief = store.load_task(task).unwrap().item.body.clone();
    attempt_ops::claim(&store, task).expect("claim");
    let finish = finish(
        &store,
        task,
        1,
        "completed",
        false,
        Some("uncommitted or untracked files: dirty.txt"),
    );
    let applied = attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");
    assert_eq!(applied.class, Some(Class::Verification));
    assert_eq!(
        state(&store, task),
        ("review".into(), "rework_pending".into())
    );
    assert_eq!(
        store.load_task(task).unwrap().item.body,
        brief,
        "arc never writes a brief"
    );
    let record = attempt_result(&store, task, 1);
    assert_eq!(record["class"], json!("verification"));
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("uncommitted or untracked files"),
        "{record}"
    );
}

/// A retryable failure gets `retry_at` 10 s later, then 20 s, then 40 s; the failure after
/// `max_attempts` (3) retries is `failed.exhausted`. The count is the task's `retry_count`, not
/// the attempt number (ADR 0048 decision 9).
#[test]
fn a_failed_outcome_retries_with_the_backoff_then_is_exhausted() {
    let (_repo, store) = setup("retry");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    for (attempt, retry_count, expected_delay) in [(1u32, 1u32, 10i64), (2, 2, 20), (3, 3, 40)] {
        if attempt > 1 {
            let retry_at = store.load_task(task).unwrap().item.retry_at.unwrap();
            let released = attempt_ops::release_retries(&store, &retry_at).expect("release");
            assert_eq!(released, vec![task.clone()], "the retry is due");
            assert_eq!(state(&store, task), ("todo".into(), "ready".into()));
        }
        let claim = attempt_ops::claim(&store, task).expect("claim");
        assert_eq!(claim.attempt, attempt);
        assert!(!claim.resume, "a fresh attempt never resumes");
        assert_eq!(
            claim.session_id,
            format!("{task}-{attempt}"),
            "every fresh attempt has its own session"
        );
        let record = finish(&store, task, attempt, "failed", false, None);
        let applied =
            attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(1)).expect("apply");
        assert_eq!(applied.class, Some(Class::Model));
        assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
        assert_eq!(
            store.load_task(task).unwrap().item.retry_count,
            retry_count,
            "attempt {attempt} counts one retry"
        );
        let retry_at = applied.retry_at.expect("a retry time");
        let now = chrono::Utc::now().timestamp();
        let delay = utc_seconds(&retry_at) - now;
        assert!(
            (delay - expected_delay).abs() <= 2,
            "attempt {attempt}: retry_at is {delay} s away, expected {expected_delay}: {retry_at}"
        );
        assert_eq!(
            store.load_task(task).unwrap().item.retry_at.as_deref(),
            Some(retry_at.as_str())
        );
    }

    let retry_at = store.load_task(task).unwrap().item.retry_at.unwrap();
    attempt_ops::release_retries(&store, &retry_at).expect("release");
    attempt_ops::claim(&store, task).expect("claim 4");
    let finish = finish(&store, task, 4, "failed", false, None);
    let applied = attempt_ops::apply(&store, task, 4, Some(&finish), None, Some(1)).expect("apply");
    assert_eq!(state(&store, task), ("failed".into(), "exhausted".into()));
    assert_eq!(applied.retry_at, None);
}

// ---- a worker's model wait (ADR 0048 decision 9) --------------------------------------

/// The delay of an applied wait, in whole seconds from now.
fn wait_secs(applied: &attempt_ops::Applied) -> i64 {
    let retry_at = applied.retry_at.as_deref().expect("a wait time");
    utc_seconds(retry_at) - chrono::Utc::now().timestamp()
}

/// A worker attempt whose `Result` says the usage window is used up waits on the model: the
/// task goes to `todo.retry_wait`, no retry is counted, the attempt records the `model_error`,
/// and the loop line names the wait (ADR 0048 decision 9).
#[test]
fn a_usage_limit_worker_waits_without_a_retry() {
    let (_repo, store) = setup("worker-wait-usage");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    let error = json!({"kind": "usage_limit", "status": null, "retry_after_secs": null});
    write_result_stream(&store, task, 1, Some(&error));
    let record = finish(&store, task, 1, "failed", false, None);

    let applied = attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");

    assert!(applied.model_wait, "{applied:?}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
    let stored = store.load_task(task).unwrap().item;
    assert_eq!(stored.retry_count, 0, "a model wait is not a retry");
    let retry_at = stored.retry_at.clone().expect("the task waits");
    assert!(
        (wait_secs(&applied) - 300).abs() <= 2,
        "the first wait is five minutes: {retry_at}"
    );
    assert_eq!(
        applied.line(),
        format!("{task} worker model unavailable -> retry at {retry_at}")
    );
    let result = attempt_result(&store, task, 1);
    assert_eq!(result["model_wait"], json!(true), "{result}");
    assert_eq!(result["model_error"], error, "{result}");
}

/// The same for `kind` `unavailable`, whose `Result` carries no reset time: the wait is the
/// same first step, and no retry is counted.
#[test]
fn an_unavailable_worker_waits_without_a_retry() {
    let (_repo, store) = setup("worker-wait-unavailable");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    let error = json!({"kind": "unavailable", "status": null, "retry_after_secs": null});
    write_result_stream(&store, task, 1, Some(&error));
    let record = finish(&store, task, 1, "failed", false, None);

    let applied = attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");

    assert!(applied.model_wait, "{applied:?}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
    assert_eq!(store.load_task(task).unwrap().item.retry_count, 0);
    assert!(
        (wait_secs(&applied) - 300).abs() <= 2,
        "the first wait is five minutes: {applied:?}"
    );
    let result = attempt_result(&store, task, 1);
    assert_eq!(result["model_error"], error, "{result}");
}

/// A session reset time longer than the step is used, and the 60-minute cap still applies
/// (ADR 0048 decision 9).
#[test]
fn a_reset_time_longer_than_the_step_is_used_and_capped() {
    let (_repo, store) = setup("worker-wait-reset");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    for (attempt, reset, expected) in [(1u32, 1000u64, 1000i64), (2, 7200, 3600)] {
        attempt_ops::claim(&store, task).expect("the next attempt starts");
        let error = json!({
            "kind": "usage_limit",
            "status": null,
            "retry_after_secs": reset,
        });
        write_result_stream(&store, task, attempt, Some(&error));
        let record = finish(&store, task, attempt, "failed", false, None);
        let applied =
            attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(1)).expect("apply");
        assert!(applied.model_wait, "attempt {attempt}: {applied:?}");
        assert!(
            (wait_secs(&applied) - expected).abs() <= 2,
            "attempt {attempt}: the wait is {} s, expected {expected}: {applied:?}",
            wait_secs(&applied)
        );
    }
}

/// A model error never bypasses a used-up budget: the task pauses for the budget, and the
/// attempt records no model wait (ADR 0048 decisions 9 and 12).
#[test]
fn a_model_error_does_not_bypass_the_budget_pause() {
    let (_repo, store) = setup("worker-model-error-budget");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.max_cost_usd = Some(0.0);
            Ok(())
        })
        .expect("the task's budget");
    attempt_ops::claim(&store, task).expect("claim");
    write_result_stream(
        &store,
        task,
        1,
        Some(&json!({"kind": "usage_limit", "status": null, "retry_after_secs": null})),
    );
    let record = finish(&store, task, 1, "failed", false, None);

    let applied = attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");

    assert!(!applied.model_wait, "{applied:?}");
    assert_eq!(state(&store, task), ("todo".into(), "paused_budget".into()));
    assert_eq!(applied.retry_at, None);
    assert!(attempt_result(&store, task, 1)["model_error"].is_null());
}

/// A run of model waits doubles and caps at 60 minutes and never reaches
/// `failed.exhausted`, even with `max_attempts = 2`: none of them is a retry (ADR 0048
/// decision 9). Mutation: raise `retry_count` for a model wait and the third attempt exhausts
/// the task.
#[test]
fn consecutive_model_waits_double_cap_and_never_exhaust() {
    let (_repo, store) = setup("worker-wait-backoff");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.max_attempts = Some(2);
            Ok(())
        })
        .expect("the task's limit");
    let error = json!({"kind": "usage_limit", "status": null, "retry_after_secs": null});

    for (index, expected) in [300i64, 600, 1200, 2400, 3600, 3600, 3600]
        .into_iter()
        .enumerate()
    {
        let attempt = index as u32 + 1;
        attempt_ops::claim(&store, task).expect("the next attempt starts");
        write_result_stream(&store, task, attempt, Some(&error));
        let record = finish(&store, task, attempt, "failed", false, None);
        let applied =
            attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(1)).expect("apply");
        assert!(applied.model_wait, "attempt {attempt}: {applied:?}");
        assert_eq!(
            state(&store, task),
            ("todo".into(), "retry_wait".into()),
            "attempt {attempt} waits"
        );
        assert_eq!(
            store.load_task(task).unwrap().item.retry_count,
            0,
            "attempt {attempt} is not a retry"
        );
        assert!(
            (wait_secs(&applied) - expected).abs() <= 2,
            "attempt {attempt}: the wait is {} s, expected {expected}: {applied:?}",
            wait_secs(&applied)
        );
    }
}

/// A failed attempt with no model error keeps the ordinary retry: the count rises and the
/// backoff is ten seconds (ADR 0048 decision 9).
#[test]
fn a_failed_worker_without_a_model_error_still_counts_a_retry() {
    let (_repo, store) = setup("worker-no-model-error");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    write_result_stream(&store, task, 1, None);
    let record = finish(&store, task, 1, "failed", false, None);

    let applied = attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");

    assert!(!applied.model_wait, "{applied:?}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
    assert_eq!(store.load_task(task).unwrap().item.retry_count, 1);
    assert!((wait_secs(&applied) - 10).abs() <= 2, "{applied:?}");
    let result = attempt_result(&store, task, 1);
    assert_eq!(result["model_wait"], json!(false), "{result}");
    assert!(result["model_error"].is_null(), "{result}");
}

/// A model error of another kind is the request's fault: `protocol` keeps today's class,
/// backoff and count, and the attempt records no model error (ADR 0048 decision 9).
#[test]
fn a_protocol_model_error_still_counts_a_retry() {
    let (_repo, store) = setup("worker-protocol");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    write_result_stream(
        &store,
        task,
        1,
        Some(&json!({"kind": "protocol", "status": null, "retry_after_secs": null})),
    );
    let record = finish(&store, task, 1, "failed", false, None);

    let applied = attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");

    assert!(!applied.model_wait, "{applied:?}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
    assert_eq!(store.load_task(task).unwrap().item.retry_count, 1);
    assert!((wait_secs(&applied) - 10).abs() <= 2, "{applied:?}");
    let result = attempt_result(&store, task, 1);
    assert_eq!(result["model_wait"], json!(false), "{result}");
    assert!(result["model_error"].is_null(), "{result}");
}

/// A command worker's `Result` keeps the ordinary retry rules: the model wait belongs to the
/// radian worker only (ADR 0048 decision 9).
#[test]
fn a_command_workers_model_error_keeps_the_retry() {
    let (_repo, store) = setup("worker-command-model-error");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    write_worker_kind(&store, task, 1, "command");
    write_result_stream(
        &store,
        task,
        1,
        Some(&json!({"kind": "usage_limit", "status": null, "retry_after_secs": null})),
    );
    let record = finish(&store, task, 1, "failed", false, None);

    let applied = attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");

    assert!(!applied.model_wait, "{applied:?}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
    assert_eq!(store.load_task(task).unwrap().item.retry_count, 1);
    assert!((wait_secs(&applied) - 10).abs() <= 2, "{applied:?}");
}

/// A task reworked ten times keeps starting: every rework sets the retry count back to 0, so
/// the rework's failing attempt retries however high the attempt number has grown. Mutation:
/// count the attempt number again, or keep the count across a rework, and the second round is
/// `failed.exhausted` instead of `todo.retry_wait`.
#[test]
fn a_reworked_task_keeps_starting_after_max_attempts_worth_of_attempts() {
    let (_repo, store) = setup("rework-retries");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let mut attempt = 1;

    // The first pushed attempt the gate would send to rework.
    attempt_ops::claim(&store, task).expect("claim");
    let record = finish(&store, task, attempt, "completed", true, None);
    attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(0)).expect("apply");

    for round in 1..=10u32 {
        // The lead writes the next brief.
        plan_ops::add_rework(&store, task, "one more round").expect("the rework");
        assert_eq!(
            store.load_task(task).unwrap().item.retry_count,
            0,
            "round {round}: a rework resets the retry count"
        );
        // The rework's first attempt fails: it retries, whatever the attempt number is.
        attempt += 1;
        attempt_ops::claim(&store, task).expect("the rework starts");
        let record = finish(&store, task, attempt, "failed", false, None);
        let applied =
            attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(1)).expect("apply");
        assert_eq!(
            state(&store, task),
            ("todo".into(), "retry_wait".into()),
            "round {round}: the rework's failure was not retried: {applied:?}"
        );
        assert_eq!(store.load_task(task).unwrap().item.retry_count, 1);
        let retry_at = applied.retry_at.expect("a retry time");
        attempt_ops::release_retries(&store, &retry_at).expect("release");
        // The retry delivers, and the next round's rework resets the count again.
        attempt += 1;
        attempt_ops::claim(&store, task).expect("the retry starts");
        let record = finish(&store, task, attempt, "completed", true, None);
        attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(0)).expect("apply");
    }
    assert_eq!(
        attempt, 21,
        "ten reworks each ran their attempt and its retry"
    );
}

/// An attempt that ends without reaching the gate (a worker failure that retries, a delivery
/// failure) did not conflict with main, so it sets the task's run of conflict reworks back to
/// 0. The run is `runs/<task>/conflict_rework.json`. Mutation: keep the count across such an
/// attempt and a task that keeps retrying would still stop at its old run.
#[test]
fn an_attempt_that_does_not_reach_the_gate_resets_the_conflict_count() {
    let (_repo, store) = setup("conflict-reset");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let path = store
        .root()
        .join("runs")
        .join(task)
        .join(radian_arc::review::CONFLICT_REWORK_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).expect("the run directory");
    std::fs::write(&path, "{\n  \"reworks\": 2\n}\n").expect("the conflict record");

    attempt_ops::claim(&store, task).expect("claim");
    let record = finish(&store, task, 1, "failed", false, None);
    attempt_ops::apply(&store, task, 1, Some(&record), None, Some(1)).expect("apply");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));

    let text = std::fs::read_to_string(&path).expect("the conflict record reads");
    let value: Value = serde_json::from_str(&text).expect("the conflict record parses");
    assert_eq!(value["reworks"], 0, "{text}");
}

/// `max_attempts` counts the retries since the last rework, not the attempt number: a task
/// already five attempts in retries from 0 after a rework, gets `max_attempts` (3) retries, and
/// the failure after them is `failed.exhausted`. Mutation: compare the attempt number and the
/// first failure is exhausted.
#[test]
fn retries_after_a_rework_start_from_zero_and_stop_at_max_attempts() {
    let (_repo, store) = setup("rework-retry-zero");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.attempt = 5;
            task.state = TaskState::Review;
            task.phase = "rework_pending".to_owned();
            Ok(())
        })
        .expect("the task is in review");
    plan_ops::add_rework(&store, task, "start over").expect("the rework");
    assert_eq!(store.load_task(task).unwrap().item.retry_count, 0);

    for (attempt, retry_count) in [(6u32, 1u32), (7, 2), (8, 3)] {
        let claim = attempt_ops::claim(&store, task).expect("claim");
        assert_eq!(claim.attempt, attempt);
        let record = finish(&store, task, attempt, "failed", false, None);
        let applied =
            attempt_ops::apply(&store, task, attempt, Some(&record), None, Some(1)).expect("apply");
        assert_eq!(
            state(&store, task),
            ("todo".into(), "retry_wait".into()),
            "attempt {attempt}: {applied:?}"
        );
        assert_eq!(store.load_task(task).unwrap().item.retry_count, retry_count);
        let retry_at = applied.retry_at.expect("a retry time");
        attempt_ops::release_retries(&store, &retry_at).expect("release");
    }

    attempt_ops::claim(&store, task).expect("claim 9");
    let finish = finish(&store, task, 9, "failed", false, None);
    let applied = attempt_ops::apply(&store, task, 9, Some(&finish), None, Some(1)).expect("apply");
    assert_eq!(
        state(&store, task),
        ("failed".into(), "exhausted".into()),
        "{applied:?}"
    );
    assert_eq!(store.load_task(task).unwrap().item.retry_count, 3);
}

/// A poisoned session never retries (decision 4): `failed.irrecoverable`.
#[test]
fn a_poisoned_outcome_is_irrecoverable() {
    let (_repo, store) = setup("poisoned");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    let finish = finish(&store, task, 1, "poisoned", false, None);
    let applied = attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");
    assert_eq!(applied.class, Some(Class::Model));
    assert_eq!(
        state(&store, task),
        ("failed".into(), "irrecoverable".into())
    );
    assert_eq!(applied.retry_at, None);
}

/// An old token's result changes nothing: no write, no attempt record.
#[test]
fn a_result_with_an_old_token_changes_nothing() {
    let (_repo, store) = setup("old-token");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    let mut finish = finish(&store, task, 1, "completed", true, None);
    finish.token = "an-old-token".to_owned();
    let applied = attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(0)).expect("apply");
    assert!(applied.ignored.is_some(), "{applied:?}");
    assert_eq!(state(&store, task), ("doing".into(), "starting".into()));
    assert!(
        !attempt_ops::run_dir(&store, task, 1)
            .join(attempt_ops::ATTEMPT_RESULT_FILE)
            .exists(),
        "an ignored result writes no attempt record"
    );
}

/// ADR 0044: `Result.cost` is the session total, so the attempt's cost is this attempt's
/// total minus the previous attempt's when both ran in one session (`resume`). Mutation: count
/// the session total as the attempt cost and the second attempt reads 1.5 instead of 0.5.
#[test]
fn cost_is_the_attempt_delta_of_the_session_total() {
    let (_repo, store) = setup("cost");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    attempt_ops::claim(&store, task).expect("claim 1");
    write_stream(&store, task, 1, Some(1_000_000));
    write_execution(&store, task, 1, task);
    let first_finish = finish(&store, task, 1, "failed", false, None);
    let first =
        attempt_ops::apply(&store, task, 1, Some(&first_finish), None, Some(1)).expect("apply");
    assert!((first.cost_usd - 1.0).abs() < 1e-9, "{first:?}");
    assert_eq!(store.load_task(task).unwrap().item.cost_usd, Some(1.0));

    let retry_at = store.load_task(task).unwrap().item.retry_at.unwrap();
    attempt_ops::release_retries(&store, &retry_at).expect("release");
    attempt_ops::claim(&store, task).expect("claim 2");
    write_stream(&store, task, 2, Some(1_500_000));
    write_execution(&store, task, 2, task);
    let second_finish = finish(&store, task, 2, "failed", false, None);
    let second =
        attempt_ops::apply(&store, task, 2, Some(&second_finish), None, Some(1)).expect("apply");
    assert!((second.cost_usd - 0.5).abs() < 1e-9, "{second:?}");
    let stored = store.load_task(task).unwrap().item;
    assert_eq!(stored.cost_usd, Some(1.5), "the task's running total");
    let record = attempt_result(&store, task, 2);
    assert!(
        (record["cost_usd"].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "{record}"
    );
    assert!(
        (record["session_cost_usd"].as_f64().unwrap() - 1.5).abs() < 1e-9,
        "{record}"
    );
}

/// With `fresh` every attempt has a session of its own, so its session total is the cost it
/// added: the delta would read zero for a cheaper second attempt. Mutation: always take the
/// delta and the second attempt costs nothing.
#[test]
fn a_fresh_attempt_counts_its_whole_session_cost() {
    let (_repo, store) = setup("fresh-cost");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];

    attempt_ops::claim(&store, task).expect("claim 1");
    write_stream(&store, task, 1, Some(1_000_000));
    write_execution(&store, task, 1, &format!("{task}-1"));
    let first_finish = finish(&store, task, 1, "failed", false, None);
    let first =
        attempt_ops::apply(&store, task, 1, Some(&first_finish), None, Some(1)).expect("apply");
    assert!((first.cost_usd - 1.0).abs() < 1e-9, "{first:?}");

    let retry_at = store.load_task(task).unwrap().item.retry_at.unwrap();
    attempt_ops::release_retries(&store, &retry_at).expect("release");
    attempt_ops::claim(&store, task).expect("claim 2");
    write_stream(&store, task, 2, Some(500_000));
    write_execution(&store, task, 2, &format!("{task}-2"));
    let second_finish = finish(&store, task, 2, "failed", false, None);
    let second =
        attempt_ops::apply(&store, task, 2, Some(&second_finish), None, Some(1)).expect("apply");
    assert!(
        (second.cost_usd - 0.5).abs() < 1e-9,
        "the whole second session is the attempt's cost: {second:?}"
    );
    assert_eq!(store.load_task(task).unwrap().item.cost_usd, Some(1.5));
}

/// A task file written before `total_cost_usd` existed starts its total from its `cost_usd`
/// on the first write, and the next attempt's cost is added to both counters (ADR 0048
/// decision 12). Mutation: start the total from 0 and it misses the old counted cost.
#[test]
fn an_old_task_without_a_total_starts_from_its_cost() {
    let (_repo, store) = setup("old-total");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.cost_usd = Some(2.0);
            task.total_cost_usd = None;
            Ok(())
        })
        .expect("an older task file");

    attempt_ops::claim(&store, task).expect("claim");
    write_stream(&store, task, 1, Some(500_000));
    let finish = finish(&store, task, 1, "failed", false, None);
    attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");
    let stored = store.load_task(task).unwrap().item;
    assert_eq!(stored.cost_usd, Some(2.5));
    assert_eq!(
        stored.total_cost_usd,
        Some(2.5),
        "the total starts from 2.0"
    );
}

/// `reset-budget` on an older task file with no `total_cost_usd` first turns its `cost_usd`
/// into the total, so zeroing the budget counter does not discard the spend it recorded. The
/// task form and the plan form behave alike. Mutation: zero `cost_usd` without materializing
/// the total and `running_total` falls back to `0`.
#[test]
fn reset_budget_keeps_an_old_tasks_cost_as_its_total() {
    let (_repo, store) = setup("reset-old-total");
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["one", "two"]);
    let one = &tasks[0];
    let two = &tasks[1];

    store
        .change_task(one, |task| {
            task.state = TaskState::Todo;
            task.phase = "paused_budget".to_owned();
            task.cost_usd = Some(1.25);
            task.total_cost_usd = None;
            Ok(())
        })
        .expect("an older task file");
    attempt_ops::reset_budget(&store, one).expect("the task form");
    let stored = store.load_task(one).unwrap().item;
    assert_eq!(stored.cost_usd, Some(0.0), "the counter resets");
    assert_eq!(
        stored.total_cost_usd,
        Some(1.25),
        "the task's old cost becomes its total: {stored:?}"
    );

    store
        .change_task(two, |task| {
            task.cost_usd = Some(2.0);
            task.total_cost_usd = None;
            Ok(())
        })
        .expect("another older task file");
    attempt_ops::reset_budget(&store, &plan).expect("the plan form");
    let stored = store.load_task(two).unwrap().item;
    assert_eq!(stored.cost_usd, Some(0.0), "the counter resets");
    assert_eq!(
        stored.total_cost_usd,
        Some(2.0),
        "the plan form keeps the old cost too: {stored:?}"
    );
}

/// A task budget pauses the task instead of retrying; `reset-budget` makes it ready and
/// resets its counted cost.
#[test]
fn a_task_budget_pauses_and_reset_budget_makes_it_ready() {
    let (_repo, store) = setup("task-budget");
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.max_cost_usd = Some(1.0);
            Ok(())
        })
        .expect("set the limit");
    let _ = plan;

    attempt_ops::claim(&store, task).expect("claim");
    write_stream(&store, task, 1, Some(1_000_000));
    let finish = finish(&store, task, 1, "failed", false, None);
    let applied = attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");
    assert_eq!(state(&store, task), ("todo".into(), "paused_budget".into()));
    assert_eq!(applied.retry_at, None);

    let reset = attempt_ops::reset_budget(&store, task).expect("reset");
    assert_eq!(reset.kind, "task");
    assert_eq!(reset.tasks, vec![task.clone()]);
    next_claim_possible(&store, task);
    assert_eq!(store.load_task(task).unwrap().item.cost_usd, Some(0.0));
}

fn next_claim_possible(store: &Store, task: &str) {
    let claim = attempt_ops::claim(store, task).expect("the reset task is claimable");
    assert_eq!(claim.attempt, 2);
}

/// A plan budget pauses the task and sets the plan's `auto` to `off`; the plan form of
/// `reset-budget` moves every paused task back and zeroes the plan's counted cost.
#[test]
fn a_plan_budget_pauses_the_plan_and_sets_auto_off() {
    let (_repo, store) = setup("plan-budget");
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["one", "two"]);
    store
        .change_plan(&plan, |plan| {
            plan.max_cost_usd = Some(1.0);
            plan.auto = "run".to_owned();
            Ok(())
        })
        .expect("set the plan limit");
    let task = &tasks[0];

    attempt_ops::claim(&store, task).expect("claim");
    write_stream(&store, task, 1, Some(1_000_000));
    let finish = finish(&store, task, 1, "failed", false, None);
    attempt_ops::apply(&store, task, 1, Some(&finish), None, Some(1)).expect("apply");
    assert_eq!(state(&store, task), ("todo".into(), "paused_budget".into()));
    assert_eq!(store.load_plan(&plan).unwrap().item.auto, "off");

    let reset = attempt_ops::reset_budget(&store, &plan).expect("reset the plan");
    assert_eq!(reset.kind, "plan");
    assert_eq!(reset.tasks, vec![task.clone()]);
    assert_eq!(state(&store, task), ("todo".into(), "ready".into()));
    assert_eq!(store.load_task(task).unwrap().item.cost_usd, Some(0.0));
    assert_eq!(store.load_plan(&plan).unwrap().item.auto, "off");
}

// ---- end to end -----------------------------------------------------------------------

fn write_worker_args(repo: &Repo, base_url: &str) {
    let path = repo.arc().join("config.toml");
    let text = std::fs::read_to_string(&path).expect("config.toml");
    assert!(
        text.contains("exec_args = []"),
        "the default config has the key"
    );
    let text = text.replace(
        "exec_args = []",
        &format!("exec_args = [\"--model\", \"m\", \"--base-url\", \"{base_url}\"]"),
    );
    std::fs::write(&path, text).expect("write config.toml");
}

fn tune(repo: &Repo, replacements: &[(&str, &str)]) {
    let path = repo.arc().join("config.toml");
    let mut text = std::fs::read_to_string(&path).expect("config.toml");
    for (from, to) in replacements {
        assert!(text.contains(from), "config.toml has `{from}`");
        text = text.replace(from, to);
    }
    std::fs::write(&path, text).expect("write config.toml");
}

fn wait_for(path: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until `path` exists **and has content**, failing after `limit`.
fn wait_for_file_within(path: &Path, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared with content within {limit:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until `path` holds `needle`, failing after `limit`. A streamed line reaches the file
/// as the writer flushes it, so a test can cancel while the turn is mid-answer.
fn wait_for_text(path: &Path, needle: &str, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains(needle)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never held `{needle}` within {limit:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until `path` exists **and has content**. The writer creates the file and then writes
/// it, so existence alone can race a reader that parses the file.
fn wait_for_file(path: &Path) {
    wait_for_file_within(path, Duration::from_secs(60));
}

/// Runs one `radian arc …` command with a deadline, so a missing timer fails the test
/// instead of hanging until nextest's own timeout.
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

/// A process start time from `ps`, empty when `ps` cannot run (the `workspace` sandbox
/// refuses it). The identity records only need a value the test controls.
fn pid_start_or_empty(pid: u32) -> String {
    std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("LC_TIME", "C")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn kill(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .expect("kill");
    let _ = status;
}

/// Makes `path` executable, for a fake `ps` the test puts on a child's `PATH`.
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).expect("the file").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make it executable");
}

/// A stand-in wrapper for a `doing` task: a shell process that writes the attempt's
/// `AttemptFinished` when it gets SIGTERM. Its pid and start time go into `identity.json`, the
/// record `cancel` reads, so `cancel` signals exactly this process.
fn fake_wrapper(repo: &Repo, store: &Store, task: &str, attempt: u32) -> std::process::Child {
    let token = store
        .load_task(task)
        .expect("the task reads")
        .item
        .token
        .expect("the claim wrote a token");
    let run_dir = attempt_ops::run_dir(store, task, attempt);
    std::fs::create_dir_all(&run_dir).expect("the run directory");
    let staged = repo.root.join(format!("{task}-{attempt}-finished.json"));
    std::fs::write(
        &staged,
        json!({
            "type": "AttemptFinished",
            "attempt_id": format!("{task}-{attempt}"),
            "token": token,
            "exit_code": 0,
            "outcome": "cancelled",
            "base_sha": null,
            "head_sha": null,
            "dirty": [],
            "pushed": false,
            "remote_sha": null,
        })
        .to_string(),
    )
    .expect("the staged finish record");
    let finish = run_dir.join("attempt_finished.json");
    let script = format!(
        "trap 'cp \"{}\" \"{}\"; exit 0' TERM; while :; do sleep 0.05; done",
        staged.display(),
        finish.display()
    );
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .spawn()
        .expect("a fake wrapper");
    let dir = repo.attempt_dir(task, attempt);
    std::fs::create_dir_all(&dir).expect("the attempt directory");
    let identity = json!({
        "attempt_id": format!("{task}-{attempt}"),
        "token": "t",
        "pid": child.id(),
        "process_started_at": pid_start_or_empty(child.id()),
        "wrapper_pid": child.id(),
        "wrapper_process_started_at": pid_start_or_empty(child.id()),
    });
    std::fs::write(dir.join("identity.json"), identity.to_string()).expect("the identity record");
    child
}

/// A stand-in wrapper that outlives its SIGTERM: only the SIGKILL after the grace ends it. With
/// `marker`, the file is written when the SIGTERM arrives, which is how a test knows the task's
/// cancel is running (and waiting the grace). `pid_start_or_empty` records the start time when
/// `ps` works; without it the pid alone is the identity, the way arc falls back.
fn stubborn_wrapper(
    repo: &Repo,
    task: &str,
    attempt: u32,
    marker: Option<&Path>,
) -> std::process::Child {
    let script = match marker {
        Some(marker) => format!(
            "trap 'printf signalled > \"{}\"' TERM; while :; do sleep 0.05; done",
            marker.display()
        ),
        None => "trap '' TERM; while :; do sleep 0.05; done".to_owned(),
    };
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .spawn()
        .expect("a stand-in wrapper that ignores SIGTERM");
    let dir = repo.attempt_dir(task, attempt);
    std::fs::create_dir_all(&dir).expect("the attempt directory");
    let identity = json!({
        "attempt_id": format!("{task}-{attempt}"),
        "token": "t",
        "pid": child.id(),
        "process_started_at": pid_start_or_empty(child.id()),
        "wrapper_pid": child.id(),
        "wrapper_process_started_at": pid_start_or_empty(child.id()),
    });
    std::fs::write(dir.join("identity.json"), identity.to_string()).expect("the identity record");
    child
}

fn plan_is(store: &Store, plan: &str, status: &str) -> bool {
    store.load_plan(plan).expect("the plan reads").item.status == status
}

/// The owner's path: `radian arc start` claims, runs the wrapper, applies a completed,
/// pushed attempt, and continues into the gate. The start process waits until the attempt
/// ends and prints the attempt's line and the verification's line.
#[test]
fn start_runs_an_attempt_through_verification_to_awaiting_human() {
    let repo = Repo::new("start-ok");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    write_worker_args(&repo, mock.base_url());

    let run = repo.radian(&["arc", "start", task]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("completed -> review.verifying")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("verify -> review.awaiting_human")),
        "{lines:?}"
    );
    let stored = store.load_task(task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human")
    );
    assert!(stored.head_sha.is_some());
    assert_ne!(stored.head_sha, stored.base_sha, "the worker committed");
    assert_eq!(
        stored.verified_sha, stored.head_sha,
        "the gate verified head"
    );
    assert!(
        stored
            .verdict_set_digest
            .as_deref()
            .is_some_and(|digest| digest.len() == 64),
        "the task records the verdict set digest: {stored:?}"
    );
    assert_eq!(
        repo.remote_branch(task).as_deref(),
        stored.head_sha.as_deref(),
        "the branch was pushed"
    );
    assert!(
        store
            .root()
            .join("runs")
            .join(task)
            .join("attempts")
            .join(format!("{task}-1"))
            .join("verdicts/commands.json")
            .is_file(),
        "the commands verdict is in the attempt's directory"
    );
    // The claim's fresh session reaches the wrapper's record, and `show` finds it there.
    let shown = repo.radian(&["arc", "show", task, "--json"]);
    assert_eq!(shown.code, 0, "show: {}", shown.stderr);
    let value: Value = serde_json::from_str(&shown.stdout).expect("show --json is JSON");
    assert_eq!(value["session_id"], json!(format!("{task}-1")));
    let text = repo.radian(&["arc", "show", task]);
    assert_eq!(text.code, 0, "show: {}", text.stderr);
    assert!(
        text.stdout.contains(&format!("session_id: {task}-1")),
        "the text form prints the session:\n{}",
        text.stdout
    );
}

/// A delivery failure (a dirty tree) is `review.rework_pending`, and `start` exits 0 because
/// the result was applied.
#[test]
fn start_applies_a_delivery_failure_as_rework_pending() {
    let repo = Repo::new("start-dirty");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let mock = Mock::start(vec![
        write_reply("c1", "dirty.txt", "x"),
        text_reply("done"),
    ]);
    write_worker_args(&repo, mock.base_url());

    let run = repo.radian(&["arc", "start", task]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    assert!(run.stdout.contains("rework_pending"), "{}", run.stdout);
    assert_eq!(
        state(&store, task),
        ("review".into(), "rework_pending".into())
    );
    let record = attempt_result(&store, task, 1);
    assert_eq!(record["class"], json!("verification"));
}

/// The owner's `start` claims a `todo.retry_wait` task at once, without waiting for its
/// `retry_at`; the scheduling loop leaves the same task alone until the time has passed
/// (ADR 0048 decision 9). The printed lines are the ones a `todo.ready` start prints.
#[test]
fn start_claims_a_retry_wait_task_without_waiting_for_retry_at() {
    let repo = Repo::new("start-retry-wait");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    let task = &tasks[0];
    let retry_at = "2099-01-01T00:00:00Z";
    store
        .change_task(task, |task| {
            task.state = TaskState::Todo;
            task.phase = "retry_wait".to_owned();
            task.retry_at = Some(retry_at.to_owned());
            Ok(())
        })
        .expect("the task waits for a retry");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    write_worker_args(&repo, mock.base_url());

    // The loop respects the backoff: a one-shot `run` does not claim the task.
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run failed: {}", run.stderr);
    assert_eq!(
        state(&store, task),
        ("todo".into(), "retry_wait".into()),
        "{}",
        run.stdout
    );
    assert_eq!(
        store.load_task(task).unwrap().item.retry_at.as_deref(),
        Some(retry_at)
    );
    assert!(
        !repo.run_dir(task, 1).exists(),
        "run claimed a task that waits for its retry: {}",
        run.stdout
    );

    // The owner's `start` is a run-now: it claims the same task at once.
    let started = repo.radian(&["arc", "start", task]);
    assert_eq!(started.code, 0, "start failed: {}", started.stderr);
    assert!(
        started.stdout.contains("completed -> review.verifying"),
        "{}",
        started.stdout
    );
    assert!(
        started.stdout.contains("verify -> review.awaiting_human"),
        "{}",
        started.stdout
    );
    let stored = store.load_task(task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human")
    );
    assert_eq!(stored.attempt, 1, "the retry_wait task was claimed");
    assert_eq!(stored.retry_at, None, "the claim clears the retry time");
}

/// The claim keeps its concurrency check: with every slot taken, `start` refuses a
/// `todo.retry_wait` task with exit 3 and leaves it unchanged.
#[test]
fn start_refuses_a_retry_wait_task_when_every_slot_is_taken() {
    let repo = Repo::new("start-retry-wait-full");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["busy", "work"]);
    let busy = &tasks[0];
    let task = &tasks[1];
    // The busy task holds the one concurrency slot. Its attempt stays 0, so reconcile
    // skips it instead of applying a result.
    store
        .change_task(busy, |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("the slot is taken");
    store
        .change_task(task, |task| {
            task.state = TaskState::Todo;
            task.phase = "retry_wait".to_owned();
            task.retry_at = Some("2099-01-01T00:00:00Z".to_owned());
            Ok(())
        })
        .expect("the task waits for a retry");

    let run = repo.radian(&["arc", "start", task]);
    assert_eq!(run.code, 3, "start: {}", run.stderr);
    assert!(
        run.stderr.contains("limits.concurrency"),
        "the refusal names the limit: {}",
        run.stderr
    );
    let stored = store.load_task(task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "retry_wait")
    );
    assert_eq!(stored.attempt, 0, "nothing was claimed");
    assert_eq!(stored.retry_at.as_deref(), Some("2099-01-01T00:00:00Z"));
}

/// A scheduling pass never claims a task that is backing off, even when its snapshot said the
/// task was ready: `claim_by` re-reads the phase under the lock. The owner's `claim` still
/// accepts the same task at once. Mutation: let `ClaimBy::SchedulingLoop` accept `retry_wait`
/// and the scheduling claim succeeds.
#[test]
fn a_scheduling_claim_refuses_a_task_that_began_backing_off() {
    let (_repo, store) = setup("scheduling-retry-wait");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.phase = "retry_wait".to_owned();
            task.retry_at = Some("2099-01-01T00:00:00Z".to_owned());
            Ok(())
        })
        .expect("the task waits for a retry");

    let refusal = attempt_ops::claim_by(&store, task, attempt_ops::ClaimBy::SchedulingLoop)
        .expect_err("the loop leaves a backing-off task alone");
    assert_eq!(refusal.exit(), Exit::State, "{refusal:?}");
    assert!(refusal.message().contains("scheduling loop"), "{refusal:?}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
    assert_eq!(store.load_task(task).unwrap().item.attempt, 0);

    let claim = attempt_ops::claim(&store, task).expect("the owner claims it at once");
    assert_eq!(claim.attempt, 1);
}

/// `start` still refuses `todo.paused_budget` and every `failed.*` phase: the retry-wait
/// claim is the only addition (ADR 0048 decision 9).
#[test]
fn start_still_refuses_paused_budget_and_failed_tasks() {
    let repo = Repo::new("start-refused-states");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    // The second task keeps the plan out of the cancelled rule while the first one is
    // `failed.cancelled`, so the refusal is the task's own state.
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work", "other"]);
    let task = &tasks[0];
    for (task_state, phase) in [
        (TaskState::Todo, "paused_budget"),
        (TaskState::Failed, "exhausted"),
        (TaskState::Failed, "cancelled"),
        (TaskState::Failed, "irrecoverable"),
    ] {
        store
            .change_task(task, |task| {
                task.state = task_state;
                task.phase = phase.to_owned();
                task.retry_at = None;
                Ok(())
            })
            .expect("the task is set");
        let run = repo.radian(&["arc", "start", task]);
        assert_eq!(
            run.code,
            3,
            "{}.{phase}: start did not refuse: {}",
            task_state.as_str(),
            run.stderr
        );
        assert!(
            run.stderr.contains("only a `todo.ready`") && run.stderr.contains("`todo.retry_wait`"),
            "{}.{phase}: {}",
            task_state.as_str(),
            run.stderr
        );
        let stored = store.load_task(task).expect("the task reads").item;
        assert_eq!(
            (stored.state, stored.phase.as_str()),
            (task_state, phase),
            "the refused start changed the state"
        );
        assert_eq!(stored.attempt, 0);
    }
}

/// No new line on the jsonl stream for `stall_timeout_secs` sends SIGTERM; the wrapper still
/// writes `AttemptFinished`, and the attempt's class is `stall`.
#[test]
fn a_stall_signals_the_wrapper_and_records_the_class() {
    let repo = Repo::new("stall");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let mock = Mock::start(vec![Reply::SseThenHang { pieces: Vec::new() }]);
    tune(
        &repo,
        &[("stall_timeout_secs = 900", "stall_timeout_secs = 1")],
    );
    write_worker_args(&repo, mock.base_url());

    let output = run_with_deadline(&repo, &["arc", "start", task], Duration::from_secs(30));
    assert_eq!(
        output.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = attempt_result(&store, task, 1);
    assert_eq!(record["class"], json!("stall"), "{record}");
    assert!(
        attempt_ops::run_dir(&store, task, 1)
            .join("attempt_finished.json")
            .is_file(),
        "the wrapper wrote AttemptFinished after the signal"
    );
    let signal: Value = serde_json::from_str(
        &std::fs::read_to_string(attempt_ops::run_dir(&store, task, 1).join("signal.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(signal["reason"], json!("stall"));
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
}

/// `max_wall_time_secs` is an absolute limit on top of the stall timer; the class is
/// `wall_time`.
#[test]
fn a_wall_time_signals_the_wrapper_and_records_the_class() {
    let repo = Repo::new("wall");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    let mock = Mock::start(vec![Reply::SseThenHang { pieces: Vec::new() }]);
    tune(
        &repo,
        &[
            ("max_wall_time_secs = 7200", "max_wall_time_secs = 1"),
            ("stall_timeout_secs = 900", "stall_timeout_secs = 60"),
        ],
    );
    write_worker_args(&repo, mock.base_url());

    let output = run_with_deadline(&repo, &["arc", "start", task], Duration::from_secs(30));
    assert_eq!(
        output.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = attempt_result(&store, task, 1);
    assert_eq!(record["class"], json!("wall_time"), "{record}");
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));
}

/// `radian arc cancel` stops a running radian worker through the protocol first: the wrapper
/// sends `session/cancel`, the agent ends the turn with `stopReason: cancelled` (the attempt's
/// `Result` says so) and the journal keeps the interrupted prefix; then the wrapper writes its
/// `AttemptFinished` and the task ends `failed.cancelled` (ADR 0048 decision 9).
#[test]
fn cancel_stops_a_running_attempt() {
    let repo = Repo::new("cancel");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    // One streamed piece and then silence: the turn is mid-answer when the cancel arrives, so
    // the journal has an interrupted assistant prefix to keep.
    let mock = Mock::start(vec![Reply::SseThenHang {
        pieces: vec![
            format!(
                "data: {}\n\n",
                agent_provider_openai_compatible::testing::chunk(
                    json!({"role": "assistant", "content": "the partial answer"}),
                    None
                )
            )
            .into_bytes(),
        ],
    }]);
    tune(
        &repo,
        &[
            ("stall_timeout_secs = 900", "stall_timeout_secs = 600"),
            // A worker that ignores SIGTERM is killed after this grace. The test does not
            // need the user's ten seconds: it checks that the signal arrives, not how long a
            // real worker would take to die. Five seconds leaves a loaded machine room to
            // schedule the wrapper while staying far below nextest's timeout.
            ("cancel_grace_secs = 10", "cancel_grace_secs = 5"),
        ],
    );
    write_worker_args(&repo, mock.base_url());

    let mut start = repo.spawn(&["arc", "start", &task]);
    wait_for(&repo.attempt_dir(&task, 1).join("identity.json"));
    // The wrapper writes every `session/update` to `result.jsonl` as it arrives, so its text is
    // the moment the answer is streaming.
    let stream = repo.run_dir(&task, 1).join("result.jsonl");
    wait_for_text(&stream, "the partial answer", Duration::from_secs(60));
    let cancel = repo.radian(&["arc", "cancel", &task]);
    assert_eq!(cancel.code, 0, "cancel failed: {}", cancel.stderr);
    assert!(
        cancel.stdout.contains("failed.cancelled"),
        "{}",
        cancel.stdout
    );
    // The wrapper confirms the stop with `AttemptFinished`. Waiting for that record is waiting
    // for the exact state the cancel produced; the start process then has nothing left to do.
    wait_for_file_within(
        &repo.run_dir(&task, 1).join("attempt_finished.json"),
        Duration::from_secs(15),
    );
    assert_eq!(state(&store, &task), ("failed".into(), "cancelled".into()));

    // The protocol cancel reached the agent: its turn answered `cancelled`, and the wrapper
    // wrote that as the attempt's `Result`. A SIGTERM alone writes no Result.
    let lines: Vec<Value> = std::fs::read_to_string(&stream)
        .expect("the attempt's result.jsonl")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let result = lines
        .iter()
        .rev()
        .find(|line| line["type"] == json!("Result"))
        .expect("the cancelled turn's Result");
    assert_eq!(result["outcome"], json!("cancelled"), "{result}");

    // The journal kept the interrupted assistant prefix (ADR 0003).
    let journal = repo.sessions.join(format!("{task}-1.jsonl"));
    let events: Vec<Value> = std::fs::read_to_string(&journal)
        .unwrap_or_else(|error| panic!("{}: {error}", journal.display()))
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let interrupted = events
        .iter()
        .find(|event| {
            event["event"] == json!("MessageAppended")
                && event["data"]["message"]["assistant"]["interrupted"] == json!(true)
        })
        .expect("the interrupted assistant prefix is journaled");
    assert!(
        interrupted["data"]["message"]["assistant"]["content"][0]["text"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("the partial answer")),
        "{interrupted}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while start.try_wait().expect("try_wait").is_none() {
        assert!(
            Instant::now() < deadline,
            "`radian arc start` did not end after the cancel"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = start.wait().expect("the start process ends");
    assert_eq!(status.code(), Some(0), "start did not exit cleanly");
}

/// A wrapper that never writes a result (SIGKILLed after the grace period) leaves the task
/// in `doing.reconciling`: arc cannot prove what happened. The record here is a live process
/// with the recorded start time, so the cancel signals it and then gives up on a result.
#[test]
fn cancel_without_a_result_leaves_the_task_reconciling() {
    let repo = Repo::new("cancel-kill");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 0")],
    );
    attempt_ops::claim(&store, task).expect("claim");
    store
        .change_task(task, |task| {
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("running");

    let mut wrapper = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a stand-in wrapper");
    let pid = wrapper.id();
    let identity = json!({
        "attempt_id": format!("{task}-1"),
        "token": "t",
        "pid": pid + 1,
        "process_started_at": pid_start_or_empty(pid),
        "wrapper_pid": pid,
        "wrapper_process_started_at": pid_start_or_empty(pid),
    });
    let dir = repo.attempt_dir(task, 1);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("identity.json"), identity.to_string()).unwrap();

    let canceled = attempt::cancel(&store, task).expect("cancel");
    assert_eq!(
        (canceled.state.as_str(), canceled.phase.as_str()),
        ("doing", "reconciling")
    );
    // A plan that is not cancelled leaves the task for the owner: a pass does not end it.
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert_eq!(state(&store, task), ("doing".into(), "reconciling".into()));
    let _ = wrapper.wait();
}

/// The grace belongs to the process the wrapper is stopping; `cancel` then leaves the wrapper a
/// record window of its own before SIGKILL (`CANCEL_RECORD_RESERVE`). A wrapper that needs longer
/// than the grace to write its record still gets it applied, rather than being killed at the
/// grace. Mutation TK0169-M6 (kill the wrapper at the grace, with no record window): the
/// stand-in dies before it writes, and the task is left `doing.reconciling`.
#[test]
fn a_cancel_waits_a_record_window_after_the_grace() {
    let repo = Repo::new("cancel-record-window");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 1")],
    );
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    attempt_ops::claim(&store, &task).expect("claim");
    store
        .change_task(&task, |task| {
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("running");
    let token = store
        .load_task(&task)
        .expect("the task reads")
        .item
        .token
        .expect("the claim wrote a token");

    // A stand-in wrapper that ignores SIGTERM and writes its record half a second after the
    // one-second grace: only the record window lets it finish. It prints `ready` once the trap
    // is installed, so the test's SIGTERM cannot arrive before the shell handles it.
    let finished = repo.run_dir(&task, 1).join("attempt_finished.json");
    let ready = repo.root.join("slow-wrapper.ready");
    let record = json!({
        "type": "AttemptFinished",
        "attempt_id": format!("{task}-1"),
        "token": token,
        "exit_code": 0,
        "outcome": "terminated",
        "base_sha": null,
        "head_sha": null,
        "dirty": [],
        "pushed": false,
        "remote_sha": null,
        "failure": null,
    });
    let script = repo.root.join("slow-wrapper.sh");
    let record_path = repo.root.join("slow-wrapper-record.json");
    std::fs::write(&record_path, record.to_string()).expect("the stand-in's record");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap 'sleep 1.5; cp '{record}' '{finished}'; exit 0' TERM\n\
             printf ready > '{ready}'\nwhile :; do sleep 0.05; done\n",
            record = record_path.display(),
            finished = finished.display(),
            ready = ready.display(),
        ),
    )
    .expect("the stand-in wrapper");
    make_executable(&script);
    let mut wrapper = std::process::Command::new(&script)
        .spawn()
        .expect("the stand-in wrapper starts");
    let pid = wrapper.id();
    let dir = repo.attempt_dir(&task, 1);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("identity.json"),
        json!({
            "attempt_id": format!("{task}-1"),
            "token": token,
            "pid": 0,
            "process_started_at": null,
            "wrapper_pid": pid,
            "wrapper_process_started_at": null,
        })
        .to_string(),
    )
    .unwrap();

    wait_for_file_within(&ready, Duration::from_secs(10));
    let started = Instant::now();
    let canceled = attempt::cancel(&store, &task).expect("cancel");
    assert_eq!(
        (canceled.state.as_str(), canceled.phase.as_str()),
        ("failed", "cancelled"),
        "the cancel killed the wrapper before its record window: {canceled:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1400),
        "the cancel did not wait for the record the wrapper wrote after the grace: {:?}",
        started.elapsed()
    );
    let _ = wrapper.wait();
}

/// A reused pid with another start time is never signalled: the record names a live process
/// whose start time does not match. The task still moves to `failed.cancelled` (the owner
/// asked), but the process stays alive.
#[test]
fn a_reused_pid_with_another_start_time_is_not_signalled() {
    let repo = Repo::new("reused-pid");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    store
        .change_task(task, |task| {
            task.phase = "reconciling".to_owned();
            Ok(())
        })
        .expect("reconciling");

    let mut stand_in = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a stand-in process");
    let pid = stand_in.id();
    let identity = json!({
        "attempt_id": format!("{task}-1"),
        "token": "t",
        "pid": pid,
        "process_started_at": "Thu Jan  1 00:00:00 1970",
        "wrapper_pid": pid,
        "wrapper_process_started_at": "Thu Jan  1 00:00:00 1970",
    });
    let dir = repo.attempt_dir(task, 1);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("identity.json"), identity.to_string()).unwrap();

    // The `ps` the cancel reads is the test's own: it reports another start time, so the
    // comparison does not depend on whether the host's sandbox refuses `ps`.
    let fake = repo.root.join("fake-bin");
    std::fs::create_dir_all(&fake).expect("the fake bin directory");
    let ps = fake.join("ps");
    std::fs::write(&ps, "#!/bin/sh\nprintf 'Thu Jan  2 00:00:00 1970\\n'\n").expect("the fake ps");
    make_executable(&ps);
    let path = format!(
        "{}:{}",
        fake.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = repo.radian_with_env(&["arc", "cancel", task], &[("PATH", path)]);
    assert_eq!(run.code, 0, "cancel: {}", run.stderr);
    assert_eq!(state(&store, task), ("failed".into(), "cancelled".into()));
    assert!(
        stand_in.try_wait().expect("try_wait").is_none(),
        "the process with another start time was not signalled"
    );
    kill(pid, "-9");
    let _ = stand_in.wait();
}

/// ADR 0048 decision 9: when `ps` cannot report a process's start time (the `workspace`
/// sandbox refuses it), the recorded pid alone is signalled. A `ps` that reports nothing makes
/// the comparison impossible; the wrapper the record names still gets its SIGTERM and writes
/// `AttemptFinished`, so the task ends at `failed.cancelled`.
#[test]
fn cancel_signals_the_recorded_wrapper_when_ps_is_unavailable() {
    let repo = Repo::new("cancel-no-ps");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 1")],
    );
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    attempt_ops::claim(&store, &task).expect("claim");
    store
        .change_task(&task, |task| {
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("running");
    let mut wrapper = fake_wrapper(&repo, &store, &task, 1);
    let pid = wrapper.id();
    // A recorded start time, the shape a wrapper that could read `ps` writes.
    let identity = json!({
        "attempt_id": format!("{task}-1"),
        "token": "t",
        "pid": pid,
        "process_started_at": "Thu Jan  1 00:00:00 1970",
        "wrapper_pid": pid,
        "wrapper_process_started_at": "Thu Jan  1 00:00:00 1970",
    });
    let dir = repo.attempt_dir(&task, 1);
    std::fs::create_dir_all(&dir).expect("the attempt directory");
    std::fs::write(dir.join("identity.json"), identity.to_string()).expect("the identity record");

    // The fake `ps` reports nothing (exit 1), the way `ps` does when it cannot report a start
    // time. `PATH` keeps the normal directories so `cancel` can run git; the fake comes first,
    // so it is the `ps` every lookup finds.
    let fake = repo.root.join("fake-bin");
    std::fs::create_dir_all(&fake).expect("the fake bin directory");
    let ps = fake.join("ps");
    std::fs::write(&ps, "#!/bin/sh\nexit 1\n").expect("the fake ps");
    make_executable(&ps);
    let path = format!(
        "{}:{}",
        fake.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = repo.radian_with_env(&["arc", "cancel", &task], &[("PATH", path)]);
    let finished = repo
        .run_dir(&task, 1)
        .join("attempt_finished.json")
        .is_file();
    let _ = wrapper.kill();
    let _ = wrapper.wait();
    assert_eq!(run.code, 0, "cancel: {}", run.stderr);
    assert!(
        finished,
        "the wrapper the record names was not signalled: {}",
        run.stdout
    );
    assert_eq!(state(&store, &task), ("failed".into(), "cancelled".into()));
}

/// `radian arc cancel <plan>` ends every task in `todo`, `doing` or `review` at
/// `failed.cancelled`, leaves `done` and `failed` tasks alone, sets `auto` `off` and the
/// plan's `status` `cancelled`, prints one line per changed task and the plan, and refuses a
/// second cancel.
#[test]
fn cancelling_a_plan_ends_every_open_task_and_the_plan() {
    let repo = Repo::new("cancel-plan");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 1")],
    );
    let plan = new_plan(&store, "plan");
    let done = new_task(&store, &plan, "done", &[]);
    let todo = new_task(&store, &plan, "todo", &[]);
    let doing = new_task(&store, &plan, "doing", &[]);
    let review = new_task(&store, &plan, "review", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    for (task, state, phase) in [
        (&done, TaskState::Done, "merged"),
        (&review, TaskState::Review, "awaiting_human"),
    ] {
        store
            .change_task(task, |task| {
                task.state = state;
                task.phase = phase.to_owned();
                Ok(())
            })
            .expect("the fixture state");
    }

    attempt_ops::claim(&store, &doing).expect("claim the doing task");
    store
        .change_task(&doing, |task| {
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("running");
    let mut wrapper = fake_wrapper(&repo, &store, &doing, 1);

    let canceled = attempt::cancel_any(&store, &plan).expect("the plan cancels");
    let attempt::Cancelled::Plan(canceled) = canceled else {
        panic!("a plan id takes the plan form");
    };
    assert_eq!(canceled.tasks.len(), 3, "{canceled:?}");
    for task in [&todo, &doing, &review] {
        assert_eq!(
            state(&store, task),
            ("failed".to_owned(), "cancelled".to_owned()),
            "{task}"
        );
    }
    assert_eq!(
        state(&store, &done),
        ("done".to_owned(), "merged".to_owned()),
        "a merged task is left alone"
    );
    let stored = store.load_plan(&plan).expect("the plan reads").item;
    assert_eq!(stored.status, "cancelled");
    assert_eq!(stored.auto, "off");
    let lines = canceled.lines();
    assert_eq!(lines.len(), 4, "{lines:?}");
    for task in [&todo, &doing, &review] {
        assert!(
            lines
                .iter()
                .any(|line| line == &format!("{task} failed.cancelled")),
            "{task} has its line: {lines:?}"
        );
    }
    assert_eq!(lines.last(), Some(&format!("{plan} cancelled")));

    let again = repo.radian(&["arc", "cancel", &plan]);
    assert_eq!(again.code, 3, "{again:?}");
    assert!(
        again.stderr.contains("cancelled"),
        "the refusal names the status: {}",
        again.stderr
    );
    let _ = wrapper.kill();
    let _ = wrapper.wait();
}

/// The owner's `cancel` accepts a task that ran out of attempts (`failed.exhausted`) or cannot
/// continue (`failed.irrecoverable`) and moves it to `failed.cancelled` (ADR 0048 decision 9):
/// "seen, given up". The attempt history, the attempt id, the costs and the plan's task list
/// stay as they are.
#[test]
fn cancel_moves_a_terminal_failure_to_failed_cancelled() {
    for phase in ["exhausted", "irrecoverable"] {
        let (_repo, store) = setup(&format!("cancel-{phase}"));
        let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work", "other"]);
        let task = &tasks[0];
        let attempt_id = format!("{task}-3");
        store
            .change_task(task, |stored| {
                stored.state = TaskState::Failed;
                stored.phase = phase.to_owned();
                stored.attempt = 3;
                stored.attempt_id = Some(attempt_id.clone());
                stored.cost_usd = Some(0.25);
                stored.total_cost_usd = Some(1.5);
                stored.retry_at = None;
                Ok(())
            })
            .expect("the terminal task is set");

        let canceled = attempt::cancel(&store, task).expect("the terminal failure cancels");
        assert_eq!(
            (canceled.state.as_str(), canceled.phase.as_str()),
            ("failed", "cancelled"),
            "{phase}"
        );
        let stored = store.load_task(task).expect("the task reads").item;
        assert_eq!(
            (stored.state.as_str(), stored.phase.as_str()),
            ("failed", "cancelled"),
            "{phase}"
        );
        assert_eq!(stored.attempt, 3, "{phase}: the attempt number stays");
        assert_eq!(
            stored.attempt_id.as_deref(),
            Some(attempt_id.as_str()),
            "{phase}: the attempt id stays"
        );
        assert_eq!(
            stored.cost_usd,
            Some(0.25),
            "{phase}: the budget counter stays"
        );
        assert_eq!(stored.total_cost_usd, Some(1.5), "{phase}: the total stays");
        assert_eq!(
            store
                .load_plan(&plan)
                .expect("the plan reads")
                .item
                .tasks
                .len(),
            2,
            "{phase}: cancelling adds no follow-up task"
        );
    }
}

/// `done.*` and `failed.cancelled` are still refused with exit 3 (ADR 0048 decision 9), and
/// the refusal lists every phase `cancel` accepts. Mutation: accept `failed.cancelled` in
/// `TaskFile::is_cancellable` and a second cancel succeeds instead.
#[test]
fn cancel_refuses_done_and_cancelled_and_lists_the_accepted_phases() {
    let (_repo, store) = setup("cancel-refused");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["done", "cancelled"]);
    for (task, state, phase) in [
        (&tasks[0], TaskState::Done, "merged"),
        (&tasks[1], TaskState::Failed, "cancelled"),
    ] {
        store
            .change_task(task, |stored| {
                stored.state = state;
                stored.phase = phase.to_owned();
                Ok(())
            })
            .expect("the refused state is set");
        let error = attempt::cancel(&store, task).expect_err("the state is refused");
        assert_eq!(error.exit(), Exit::State, "{phase}: {error:?}");
        let message = error.message();
        for accepted in [
            "`todo`",
            "`doing`",
            "`review`",
            "`failed.exhausted`",
            "`failed.irrecoverable`",
        ] {
            assert!(
                message.contains(accepted),
                "{phase}: the refusal does not name {accepted}: {message}"
            );
        }
    }
}

/// The plan form cancels exhausted and irrecoverable tasks by the task form's rules and leaves
/// `done` and `failed.cancelled` alone (ADR 0048 decision 9). Mutation: leave exhausted tasks
/// alone in `cancel_plan_tasks` and they stay `failed.exhausted`.
#[test]
fn cancelling_a_plan_ends_its_exhausted_and_irrecoverable_tasks() {
    let (_repo, store) = setup("cancel-plan-terminal");
    let (plan, tasks) = confirmed_plan_with_tasks(
        &store,
        "plan",
        &["merged", "exhausted", "irrecoverable", "cancelled"],
    );
    for (task, state, phase) in [
        (&tasks[0], TaskState::Done, "merged"),
        (&tasks[1], TaskState::Failed, "exhausted"),
        (&tasks[2], TaskState::Failed, "irrecoverable"),
        (&tasks[3], TaskState::Failed, "cancelled"),
    ] {
        store
            .change_task(task, |stored| {
                stored.state = state;
                stored.phase = phase.to_owned();
                Ok(())
            })
            .expect("the fixture state is set");
    }

    let canceled = attempt::cancel_any(&store, &plan).expect("the plan cancels");
    let attempt::Cancelled::Plan(canceled) = canceled else {
        panic!("a plan id takes the plan form");
    };
    let lines = canceled.lines();
    assert_eq!(canceled.tasks.len(), 2, "{lines:?}");
    for task in [&tasks[1], &tasks[2]] {
        assert_eq!(
            state(&store, task),
            ("failed".to_owned(), "cancelled".to_owned()),
            "{task}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == &format!("{task} failed.cancelled")),
            "{task} has its line: {lines:?}"
        );
    }
    assert_eq!(
        state(&store, &tasks[0]),
        ("done".to_owned(), "merged".to_owned()),
        "a merged task is left alone"
    );
    assert_eq!(
        state(&store, &tasks[3]),
        ("failed".to_owned(), "cancelled".to_owned()),
        "an already cancelled task is left alone"
    );
    assert_eq!(lines.last(), Some(&format!("{plan} cancelled")));
}

/// A plan whose only non-merged task was `failed.exhausted` reads finished after the owner
/// cancels it (`PlanView::is_finished`, ADR 0048 decisions 3 and 9): every task is now
/// `done.merged` or `failed.cancelled`, and at least one merged.
#[test]
fn a_plan_reads_finished_after_its_exhausted_task_is_cancelled() {
    let (_repo, store) = setup("cancel-finished");
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["merged", "gone"]);
    store
        .change_task(&tasks[0], |stored| {
            stored.state = TaskState::Done;
            stored.phase = "merged".to_owned();
            Ok(())
        })
        .expect("the merged task is set");
    store
        .change_task(&tasks[1], |stored| {
            stored.state = TaskState::Failed;
            stored.phase = "exhausted".to_owned();
            Ok(())
        })
        .expect("the exhausted task is set");
    assert!(
        !store
            .plan_view(&plan)
            .expect("the plan reads")
            .is_finished(),
        "the exhausted task is still work left"
    );

    attempt::cancel(&store, &tasks[1]).expect("the exhausted task cancels");

    let view = store.plan_view(&plan).expect("the plan reads");
    assert!(
        view.is_finished(),
        "every task is done.merged or failed.cancelled: {:?}",
        view.tasks
            .iter()
            .map(|task| format!("{}.{}", task.item.state.as_str(), task.item.phase))
            .collect::<Vec<_>>()
    );
    assert_eq!(view.progress(), (1, 1), "a cancelled task is not counted");
}

/// The plan's status write is a transaction of its own, so the lead's `add_task` can land
/// between the task walk and that write. The plan is `cancelled` by then and takes no task, so
/// one more walk ends the one that made it in: nothing is left `todo.proposed` under a final
/// plan.
#[test]
fn a_task_added_while_the_plan_is_cancelled_is_cancelled_too() {
    let repo = Repo::new("cancel-plan-add");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 2")],
    );
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    attempt_ops::claim(&store, &task).expect("claim");
    store
        .change_task(&task, |task| {
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("running");
    let marker = repo.root.join("signalled");
    let mut wrapper = stubborn_wrapper(&repo, &task, 1, Some(&marker));

    let clone = repo.clone.clone();
    let plan_id = plan.clone();
    let cancel = std::thread::spawn(move || {
        let store = Store::open(&clone).expect("the store opens");
        attempt::cancel_plan(&store, &plan_id)
    });
    // The walk is inside this task's cancel and waits the grace, which leaves the window the
    // status write and the task walk are separate in.
    wait_for_file(&marker);
    let added = store
        .add_task_proposed(
            &plan,
            "extra",
            "An extra task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("the lead adds a task while the walk runs");

    let canceled = cancel
        .join()
        .expect("the cancel thread did not panic")
        .expect("the plan cancels");
    assert_eq!(
        store.load_plan(&plan).expect("the plan reads").item.status,
        "cancelled"
    );
    assert_eq!(
        state(&store, &task),
        ("doing".to_owned(), "reconciling".to_owned()),
        "the wrapper was killed and left no result"
    );
    assert_eq!(
        state(&store, &added),
        ("failed".to_owned(), "cancelled".to_owned()),
        "the task added during the walk is cancelled too"
    );
    assert!(
        canceled
            .tasks
            .iter()
            .any(|task| task.task == added && task.phase == "cancelled"),
        "{canceled:?}"
    );
    assert_eq!(canceled.lines().last(), Some(&format!("{plan} cancelled")));
    let _ = wrapper.wait();
}

/// The pass that ends a cancelled plan's `doing.reconciling` task does not wait a second cancel
/// grace: the plan form already sent that wrapper its SIGTERM and killed it. The pass checks
/// the pid and its start time, and kills the process at once when it is still there.
#[test]
fn a_pass_ends_a_reconciling_task_without_a_second_grace() {
    let repo = Repo::new("pass-reconciling-grace");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 5")],
    );
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    attempt_ops::claim(&store, &task).expect("claim");
    store
        .change_task(&task, |task| {
            task.phase = "reconciling".to_owned();
            Ok(())
        })
        .expect("reconciling");
    // The wrapper the plan form could not end: it ignores SIGTERM and is still the process the
    // record names.
    let mut wrapper = stubborn_wrapper(&repo, &task, 1, None);
    store
        .change_plan(&plan, |plan| {
            plan.status = "cancelled".to_owned();
            plan.auto = "off".to_owned();
            Ok(())
        })
        .expect("the plan form cancelled the plan");

    let started = Instant::now();
    let ended = attempt::end_reconciling_of_cancelled_plans(&store).expect("the pass runs");
    let elapsed = started.elapsed();

    assert_eq!(ended, vec![task.clone()]);
    assert_eq!(
        state(&store, &task),
        ("failed".to_owned(), "cancelled".to_owned())
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the pass waited a second cancel grace: {elapsed:?}"
    );
    let _ = wrapper.wait();
}

/// A `review` task's running gate is signalled before the task moves: `cancel` stops the
/// process the gate record names, then ends the task at `failed.cancelled`.
#[test]
fn cancelling_a_review_task_signals_its_running_gate_first() {
    let repo = Repo::new("cancel-review-gate");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    tune(
        &repo,
        &[("cancel_grace_secs = 10", "cancel_grace_secs = 1")],
    );
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    store
        .change_task(task, |task| {
            task.state = TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            Ok(())
        })
        .expect("review");

    let mut gate = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a stand-in gate");
    let dir = repo.arc().join("runs").join(task);
    std::fs::create_dir_all(&dir).expect("the run directory");
    std::fs::write(
        dir.join("gate.json"),
        json!({
            "kind": "verify",
            "attempt": format!("{task}-1"),
            "status": "running",
            "pid": gate.id(),
            "process_started_at": pid_start_or_empty(gate.id()),
        })
        .to_string(),
    )
    .expect("the gate record");

    let canceled = attempt::cancel(&store, task).expect("cancel");
    assert_eq!(
        (canceled.state.as_str(), canceled.phase.as_str()),
        ("failed", "cancelled")
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if gate.try_wait().expect("try_wait").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the gate outlived the cancel");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `cancel` resolves its id as `reset-budget` does: an id that names both a plan and a task is
/// refused (exit 3) and changes nothing.
#[test]
fn an_id_that_names_a_plan_and_a_task_is_refused_by_cancel() {
    let (_repo, store) = setup("cancel-both");
    let (plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    std::fs::copy(
        store.root().join("tasks").join(format!("{}.md", tasks[0])),
        store.root().join("tasks").join(format!("{plan}.md")),
    )
    .expect("a task file under the plan's id");

    let error = attempt::cancel_any(&store, &plan).expect_err("the id names both");
    assert_eq!(error.exit(), Exit::State);
    assert!(
        error.message().contains("names both"),
        "{}",
        error.message()
    );
    assert!(!plan_is(&store, &plan, "cancelled"), "nothing changed");
}

/// A cancelled plan is final: `confirm`, `auto`, `priority`, `start`, `reverify`,
/// `reset-budget`, `add task` and the MCP `add_task`, `update_plan` and `add_rework` tools
/// all refuse it, naming the status.
#[test]
fn a_cancelled_plan_refuses_every_command_that_would_change_it() {
    let (_repo, store) = setup("cancel-refusals");
    let plan = new_plan(&store, "plan");
    let task = new_task(&store, &plan, "work", &[]);
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    attempt::cancel_any(&store, &plan).expect("the plan cancels");
    assert!(plan_is(&store, &plan, "cancelled"));

    let refusals = [
        plan_ops::confirm(&store, &plan).expect_err("confirm"),
        plan_ops::set_auto(&store, &plan, "run").expect_err("auto"),
        plan_ops::set_priority(&store, &plan, "P1").expect_err("priority"),
        store
            .add_task(&plan, "new", "New", vec![], false, vec![], "brief")
            .expect_err("add task"),
        attempt_ops::reset_budget(&store, &plan).expect_err("reset-budget"),
        attempt::start(&store, &task).expect_err("start"),
    ];
    for error in refusals {
        assert_eq!(error.exit(), Exit::State, "{}", error.message());
        assert!(error.message().contains("cancelled"), "{}", error.message());
    }

    // A task of the plan, set back to `review`, is refused by `reverify` and by the lead's
    // rework tool; the plan itself by `update_plan`.
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.token = Some("t".to_owned());
            task.head_sha = Some("b".repeat(40));
            Ok(())
        })
        .expect("review.verifying");
    let reverify = radian_arc::review::reverify(&store, &task).expect_err("reverify");
    assert_eq!(reverify.exit(), Exit::State);
    assert!(
        reverify.message().contains("cancelled"),
        "{}",
        reverify.message()
    );
    store
        .change_task(&task, |task| {
            task.phase = "awaiting_human".to_owned();
            Ok(())
        })
        .expect("review.awaiting_human");

    let server = radian_arc::mcp::ArcServer::new(store.clone());
    let call = |name: &str, arguments: serde_json::Value| {
        server
            .dispatch(name, arguments.as_object().cloned())
            .expect_err("the tool refuses")
    };
    let add_task = call(
        "add_task",
        json!({"plan": plan, "slug": "mcp", "title": "T", "brief": "b"}),
    );
    assert!(add_task.contains("cancelled"), "{add_task}");
    let update_plan = call(
        "update_plan",
        json!({"plan": plan, "change_record": "a change"}),
    );
    assert!(update_plan.contains("cancelled"), "{update_plan}");
    let add_rework = call("add_rework", json!({"task": task, "brief": "again"}));
    assert!(add_rework.contains("cancelled"), "{add_rework}");
}

/// A plan whose tasks are all `failed.cancelled` is cancelled by its tasks (ADR 0048 decision
/// 3), and it is final exactly as a plan whose `status` says so: `cancel`, `confirm` and its
/// dry run, `auto`, `priority`, `reset-budget`, `start`, `add task`, and the MCP `add_task`,
/// `update_plan` and `add_rework` tools refuse it. No task added later can make it active
/// again, so its file's `status` is never rewritten either.
#[test]
fn a_plan_its_tasks_cancelled_refuses_every_command_that_would_change_it() {
    let (_repo, store) = setup("cancel-by-tasks");
    let plan = new_plan(&store, "plan");
    let first = new_task(&store, &plan, "one", &[]);
    let second = new_task(&store, &plan, "two", &[]);
    for id in [&first, &second] {
        attempt::cancel(&store, id).expect("each task cancels on its own");
    }
    assert!(
        !plan_is(&store, &plan, "cancelled"),
        "the plan's status is not rewritten"
    );
    assert!(
        store
            .plan_view(&plan)
            .expect("the plan reads")
            .is_cancelled_by_tasks(),
        "the tasks cancel the plan"
    );

    let refusals = [
        attempt::cancel_any(&store, &plan).expect_err("cancel"),
        plan_ops::confirm(&store, &plan).expect_err("confirm"),
        plan_ops::confirm_dry_run(&store, &plan).expect_err("confirm --dry-run"),
        plan_ops::set_auto(&store, &plan, "run").expect_err("auto"),
        plan_ops::set_priority(&store, &plan, "P1").expect_err("priority"),
        store
            .add_task(&plan, "new", "New", vec![], false, vec![], "brief")
            .expect_err("add task"),
        store
            .add_task_proposed(&plan, "mcp", "M", vec![], false, vec![], "brief")
            .expect_err("the lead's add task"),
        attempt_ops::reset_budget(&store, &plan).expect_err("reset-budget"),
        attempt::start(&store, &first).expect_err("start"),
    ];
    for error in refusals {
        assert_eq!(error.exit(), Exit::State, "{}", error.message());
        assert!(error.message().contains("cancelled"), "{}", error.message());
    }

    let server = radian_arc::mcp::ArcServer::new(store.clone());
    let call = |name: &str, arguments: serde_json::Value| {
        server
            .dispatch(name, arguments.as_object().cloned())
            .expect_err("the tool refuses")
    };
    let add_task = call(
        "add_task",
        json!({"plan": plan, "slug": "mcp", "title": "T", "brief": "b"}),
    );
    assert!(add_task.contains("cancelled"), "{add_task}");
    let update_plan = call(
        "update_plan",
        json!({"plan": plan, "change_record": "a change"}),
    );
    assert!(update_plan.contains("cancelled"), "{update_plan}");
    let add_rework = call("add_rework", json!({"task": second, "brief": "again"}));
    assert!(add_rework.contains("cancelled"), "{add_rework}");

    let view = store.plan_view(&plan).expect("the plan reads");
    assert_eq!(view.tasks.len(), 2, "no task was added");
    assert!(
        view.is_cancelled(),
        "the plan is still cancelled by its tasks"
    );
    assert!(
        !plan_is(&store, &plan, "cancelled"),
        "no command rewrote the status"
    );
}

/// Reconcile after a restart: a live wrapper is left alone; a finished wrapper's result is
/// applied.
#[test]
fn reconcile_leaves_a_live_wrapper_and_applies_a_finished_one() {
    let repo = Repo::new("reconcile");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    let mock = Mock::start(vec![Reply::SseThenHang { pieces: Vec::new() }]);
    tune(
        &repo,
        &[("stall_timeout_secs = 900", "stall_timeout_secs = 600")],
    );
    write_worker_args(&repo, mock.base_url());

    let mut start = repo.spawn(&["arc", "start", &task]);
    let identity = wait_for(&repo.attempt_dir(&task, 1).join("identity.json"));
    let wrapper = identity["wrapper_pid"].as_u64().expect("wrapper_pid") as u32;
    let _ = start.kill();
    let _ = start.wait();

    let applied = attempt::reconcile(&store).expect("reconcile");
    assert!(
        applied.is_empty(),
        "the live wrapper is left alone: {applied:?}"
    );
    assert_eq!(state(&store, &task), ("doing".into(), "running".into()));

    // The wrapper alone now ends: SIGTERM forwards to its worker and it writes its record.
    kill(wrapper, "-TERM");
    wait_for_file(&repo.run_dir(&task, 1).join("attempt_finished.json"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while store.load_task(&task).unwrap().item.state.as_str() == "doing" {
        assert!(
            Instant::now() < deadline,
            "reconcile never applied the result"
        );
        std::thread::sleep(Duration::from_millis(50));
        attempt::reconcile(&store).expect("reconcile");
    }
    assert_ne!(state(&store, &task), ("doing".into(), "running".into()));
}

/// A dead wrapper with no result is an `infrastructure` (terminated) retry. Liveness is the
/// heartbeat's age (ADR 0048 decision 9), so the test makes the last beat old: the wrapper is
/// dead even though `stall_timeout_secs` is long.
#[test]
fn reconcile_retries_a_dead_wrapper_without_a_result() {
    let repo = Repo::new("reconcile-dead");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = tasks[0].clone();
    let mock = Mock::start(vec![Reply::SseThenHang { pieces: Vec::new() }]);
    tune(
        &repo,
        &[("stall_timeout_secs = 900", "stall_timeout_secs = 600")],
    );
    write_worker_args(&repo, mock.base_url());

    let mut start = repo.spawn(&["arc", "start", &task]);
    let identity = wait_for(&repo.attempt_dir(&task, 1).join("identity.json"));
    let wrapper = identity["wrapper_pid"].as_u64().expect("wrapper_pid") as u32;
    let worker = identity["pid"].as_u64().expect("pid") as u32;
    let _ = start.kill();
    let _ = start.wait();
    kill(wrapper, "-9");
    kill(worker, "-9");
    std::thread::sleep(Duration::from_millis(200));
    // The wrapper stopped beating: an old beat is a dead attempt.
    let heartbeat = repo.attempt_dir(&task, 1).join("heartbeat.json");
    let mut value: Value = serde_json::from_str(
        &std::fs::read_to_string(&heartbeat).expect("the wrapper wrote a heartbeat"),
    )
    .expect("the heartbeat parses");
    value["last_beat"] = json!("2020-01-01T00:00:00Z");
    std::fs::write(&heartbeat, value.to_string()).expect("a stale heartbeat");

    let applied = attempt::reconcile(&store).expect("reconcile");
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0].class, Some(Class::Infrastructure));
    assert_eq!(state(&store, &task), ("todo".into(), "retry_wait".into()));
}

/// ADR 0048 decision 9: reconcile decides liveness from the heartbeat's age, and it calls no
/// `ps` (the `workspace` sandbox refuses it). A fake `ps` on `PATH` records any call, and the
/// recorded wrapper pid is a live process: the old process check would call it, find the
/// process alive, and leave the attempt alone; the stale heartbeat is what makes it dead.
#[test]
fn reconcile_judges_a_stale_heartbeat_dead_without_calling_ps() {
    let repo = Repo::new("heartbeat-stale");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let (_plan, tasks) = confirmed_plan_with_tasks(&store, "plan", &["work"]);
    let task = &tasks[0];
    attempt_ops::claim(&store, task).expect("claim");
    store
        .change_task(task, |task| {
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("running");

    let mut stand_in = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a stand-in wrapper process");
    let pid = stand_in.id();
    let dir = repo.attempt_dir(task, 1);
    std::fs::create_dir_all(&dir).expect("the attempt directory");
    // The wrapper is "alive" by pid, and its last beat is two hours old.
    let identity = json!({
        "attempt_id": format!("{task}-1"),
        "token": "t",
        "pid": pid,
        "wrapper_pid": pid,
        "wrapper_process_started_at": pid_start_or_empty(pid),
    });
    std::fs::write(dir.join("identity.json"), identity.to_string()).expect("identity");
    let heartbeat = json!({
        "attempt_id": format!("{task}-1"),
        "pid": pid,
        "last_beat": "2020-01-01T00:00:00Z",
    });
    std::fs::write(dir.join("heartbeat.json"), heartbeat.to_string()).expect("heartbeat");

    // A `ps` that records every call, and finds every process alive. `PATH` is set only on
    // the `radian arc run` child below: a process-wide `set_var` would be unsafe under plain
    // `cargo test`, where other tests share this process.
    let fake = repo.root.join("fake-bin");
    std::fs::create_dir_all(&fake).expect("the fake bin directory");
    let marker = repo.root.join("ps-called");
    let ps = fake.join("ps");
    std::fs::write(
        &ps,
        format!(
            "#!/bin/sh\ntouch '{}'\necho 'Thu Jan  1 00:00:00 1970'\nexit 0\n",
            marker.display()
        ),
    )
    .expect("the fake ps");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&ps).expect("the fake ps").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&ps, permissions).expect("executable");
    }
    let path = format!(
        "{}:{}",
        fake.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = repo.radian_with_env(&["arc", "run"], &[("PATH", path)]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);

    assert!(
        !marker.exists(),
        "reconcile called `ps`; it must decide liveness from the heartbeat"
    );
    assert_eq!(state(&store, task), ("todo".into(), "retry_wait".into()));

    kill(pid, "-9");
    let _ = stand_in.wait();
}

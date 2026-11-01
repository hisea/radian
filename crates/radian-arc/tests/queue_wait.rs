//! A task's queue wait (ADR 0048 decision 13): the scheduling pass records the first pass that
//! found a startable task and did not start it, the claim moves that time into the attempt's
//! `queue.json`, and the reader and `show` report it.
//!
//! Every test drives the real store and pass against a temp repository. The blocker a test
//! claims by hand fills `[limits] concurrency` without starting a wrapper, so a pass can be run
//! without a model.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Repo, work_args};
use radian_arc::read;
use radian_arc::run::LoopState;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, attempt_ops, cli, plan_ops};
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

/// A confirmed plan at `auto` `run`.
fn confirm_run(store: &Store, plan: &str) {
    plan_ops::confirm(store, plan).expect("confirm");
    plan_ops::set_auto(store, plan, "run").expect("auto run");
}

/// `runs/<task>/eligible_at.json`.
fn eligible(repo: &Repo, task: &str) -> PathBuf {
    repo.arc().join("runs").join(task).join("eligible_at.json")
}

/// `runs/<task>/attempts/<attempt>/queue.json`.
fn queue(repo: &Repo, task: &str, attempt: u32) -> PathBuf {
    repo.run_dir(task, attempt).join("queue.json")
}

fn read_json(path: &Path) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn utc(text: &str) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::parse_from_rfc3339(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

/// One scheduling pass, without a machine-wide cap and without a stopping signal.
fn pass(store: &Store) {
    let mut state = LoopState::new(store);
    let mut sink = |_: &str| {};
    state.pass(store, &mut sink, None).expect("the pass runs");
}

/// Fills the only `[limits] concurrency` slot without starting a wrapper.
fn claim_blocker(store: &Store, task: &str) {
    attempt_ops::claim(store, task).expect("the blocker claims");
}

/// Completes a claimed attempt's `queue.json` the way the wrapper does when the worker starts.
fn complete(repo: &Repo, task: &str, attempt: u32, started_at: &str) {
    attempt_ops::complete_queue_record(&repo.run_dir(task, attempt), started_at)
        .expect("the queue record completes");
}

/// Moves a task out of `doing` without a wrapper: the same state a cancel leaves.
fn release(store: &Store, task: &str) {
    store
        .change_task(task, |task| {
            task.enter(TaskState::Failed, "cancelled");
            Ok(())
        })
        .expect("the blocker is released");
}

/// The `radian` config's command worker: a real program that exits at once.
fn command_worker(repo: &Repo) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(
        text.contains("# command = [\"codex\", \"exec\"]"),
        "the worker command key is in the file"
    );
    fs::write(
        &path,
        text.replacen(
            "# command = [\"codex\", \"exec\"]",
            "command = [\"true\"]",
            1,
        ),
    )
    .expect("write config.toml");
}

/// Edge 1: with one concurrency slot and two startable tasks, the second gets
/// `eligible_at.json` on the first pass. When it starts, its `queue.json` holds the difference
/// from that time, and the record is gone.
#[test]
fn a_second_startable_task_waits_and_its_attempt_records_the_wait() {
    let repo = Repo::new("queue-wait-second");
    let store = setup(&repo);
    let plan = add_plan(&store, "second");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    let eligible_path = eligible(&repo, &waited);
    assert!(
        eligible_path.is_file(),
        "the second startable task records its wait"
    );
    let first = read_json(&eligible_path);
    assert!(
        first["eligible_at"].as_str().is_some(),
        "the record holds the pass's time: {first}"
    );

    release(&store, &blocker);
    let claim = attempt_ops::claim(&store, &waited).expect("the waited task claims");
    // The wrapper completes the record when it starts the worker; the test uses a time 3
    // seconds after the claim so the wait is visibly the difference from `eligible_at`.
    let started = utc(&plan_ops::utc_now()) + chrono::Duration::seconds(3);
    let started_text = started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    complete(&repo, &waited, claim.attempt, &started_text);
    let record = read_json(&queue(&repo, &waited, claim.attempt));
    let recorded = utc(record["started_at"].as_str().expect("started_at"));
    let eligible_at = utc(record["eligible_at"].as_str().expect("eligible_at"));
    let wait = record["queue_wait_secs"].as_u64().expect("queue_wait_secs");
    assert_eq!(recorded, started, "the wrapper's start time is recorded");
    assert_eq!(
        wait,
        u64::try_from((recorded - eligible_at).num_seconds()).unwrap_or(0),
        "the wait is started_at - eligible_at: {record}"
    );
    assert!(
        !eligible_path.exists(),
        "the claim consumed the eligibility record"
    );
}

/// Edge 2: a task the pass starts the first time it finds it startable waits `0`, and no
/// `eligible_at.json` is ever written. The pass is the real `radian arc run` with a command
/// worker, so the claim is the one a scheduling pass makes.
#[test]
fn a_task_the_pass_starts_at_once_waits_zero() {
    let repo = Repo::new("queue-wait-none");
    let store = setup(&repo);
    let plan = add_plan(&store, "none");
    let task = add_task(&store, &plan, "work", &[]);
    confirm_run(&store, &plan);
    command_worker(&repo);

    let run = repo.radian(&["arc", "run"]);
    let record = queue(&repo, &task, 1);
    assert!(
        record.is_file(),
        "the pass started the task and its claim wrote queue.json: {run:?}"
    );
    let value = read_json(&record);
    assert_eq!(
        value["queue_wait_secs"],
        json!(0),
        "a task the pass starts at once waits 0: {value}"
    );
    assert!(
        value.get("eligible_at").is_none(),
        "no eligibility record existed: {value}"
    );
    assert!(
        !eligible(&repo, &task).exists(),
        "the pass wrote no eligibility record for the task it started"
    );

    let canceled = repo.radian(&["arc", "cancel", &task]);
    assert_eq!(canceled.code, 0, "cancel: {canceled:?}");
}

/// Edge 3: a second skipping pass does not rewrite `eligible_at.json`; its time stays the first
/// pass's.
#[test]
fn a_second_skipping_pass_does_not_rewrite_the_record() {
    let repo = Repo::new("queue-wait-rewrite");
    let store = setup(&repo);
    let plan = add_plan(&store, "rewrite");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    let first = read_json(&eligible(&repo, &waited));
    // The stored time has whole-second resolution; wait so a rewrite would be visible.
    std::thread::sleep(Duration::from_millis(1100));
    pass(&store);
    let second = read_json(&eligible(&repo, &waited));
    assert_eq!(
        first["eligible_at"], second["eligible_at"],
        "the second pass kept the first pass's time"
    );
}

/// Edge 4: a task whose `after` task is not merged is not startable, so it gets no
/// `eligible_at.json`; the startable task held by the full slot does.
#[test]
fn a_task_whose_after_is_not_merged_is_not_startable() {
    let repo = Repo::new("queue-wait-after");
    let store = setup(&repo);
    let plan = add_plan(&store, "after");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let first = add_task(&store, &plan, "first", &[]);
    let second = add_task(&store, &plan, "second", &[&first]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    assert!(
        eligible(&repo, &first).is_file(),
        "the startable task held by the full slot records its wait"
    );
    assert!(
        !eligible(&repo, &second).exists(),
        "a task waiting for an unmerged `after` task is not startable"
    );
}

/// Edge 5: a paused plan's task gets no record, and a task that had one loses it when its plan
/// is paused; a later pass writes none.
#[test]
fn a_paused_plan_drops_the_wait_and_does_not_begin_one() {
    let repo = Repo::new("queue-wait-paused");
    let store = setup(&repo);
    let plan = add_plan(&store, "paused");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    let path = eligible(&repo, &waited);
    assert!(path.is_file(), "the wait began before the pause");
    plan_ops::pause(&store, &plan).expect("pause");
    assert!(!path.exists(), "the pause dropped the wait");
    pass(&store);
    assert!(!path.exists(), "a paused plan begins no wait");
}

/// Edge 6: a rework attempt waits again and gets its own `queue.json`; the first attempt's
/// record stays as it was.
#[test]
fn a_rework_attempt_waits_again_and_gets_its_own_queue_record() {
    let repo = Repo::new("queue-wait-rework");
    let store = setup(&repo);
    let plan = add_plan(&store, "rework");
    let task = add_task(&store, &plan, "work", &[]);
    let blocker = add_task(&store, &plan, "blocker", &[]);
    confirm_run(&store, &plan);

    let first = attempt_ops::claim(&store, &task).expect("the first attempt claims");
    let first_started = plan_ops::utc_now();
    complete(&repo, &task, first.attempt, &first_started);
    let first_record = read_json(&queue(&repo, &task, first.attempt));
    assert_eq!(first_record["queue_wait_secs"], json!(0));

    // The attempt ends and the lead gives the task a rework brief: review -> todo.ready.
    store
        .change_task(&task, |task| {
            task.enter(TaskState::Review, "rework_pending");
            Ok(())
        })
        .expect("the attempt ends");
    plan_ops::add_rework(&store, &task, "fix it").expect("the rework brief");

    claim_blocker(&store, &blocker);
    pass(&store);
    assert!(
        eligible(&repo, &task).is_file(),
        "the rework attempt begins its own wait"
    );

    release(&store, &blocker);
    let second = attempt_ops::claim(&store, &task).expect("the rework attempt claims");
    assert_eq!(second.attempt, 2);
    let second_started = utc(&plan_ops::utc_now()) + chrono::Duration::seconds(2);
    complete(
        &repo,
        &task,
        second.attempt,
        &second_started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    let second_record = read_json(&queue(&repo, &task, second.attempt));
    assert!(
        second_record.get("eligible_at").is_some(),
        "the rework attempt's record holds its own eligibility time: {second_record}"
    );
    assert!(
        second_record["queue_wait_secs"].as_u64().is_some(),
        "the rework attempt reports its wait: {second_record}"
    );
    assert_eq!(
        read_json(&queue(&repo, &task, first.attempt))["queue_wait_secs"],
        json!(0),
        "the first attempt's record is unchanged"
    );
    assert!(!eligible(&repo, &task).exists(), "the claim consumed it");
}

/// Edge 8: a torn `eligible_at.json` is consumed and the wait is not reported; the claim still
/// succeeds.
#[test]
fn a_torn_eligibility_record_is_consumed_and_the_wait_is_not_reported() {
    let repo = Repo::new("queue-wait-torn");
    let store = setup(&repo);
    let plan = add_plan(&store, "torn");
    let task = add_task(&store, &plan, "work", &[]);
    confirm_run(&store, &plan);
    let path = eligible(&repo, &task);
    fs::create_dir_all(path.parent().expect("a parent")).expect("the run directory");
    fs::write(&path, "{ not json").expect("the torn record");

    let claim = attempt_ops::claim(&store, &task).expect("the claim still succeeds");
    assert!(!path.exists(), "the claim deletes a torn record too");
    complete(&repo, &task, claim.attempt, &plan_ops::utc_now());
    let record = read_json(&queue(&repo, &task, claim.attempt));
    assert!(
        record.get("queue_wait_secs").is_none(),
        "a torn record reports no wait: {record}"
    );
    assert!(record.get("eligible_at").is_none());
    assert!(record["started_at"].as_str().is_some());

    let metrics = read::attempt_metrics(&store, &task, claim.attempt).expect("the metrics read");
    assert_eq!(metrics.queue_wait_secs, None);
    let text = cli::show_text(&store, &task).expect("show succeeds");
    assert!(
        text.contains("queue_wait_secs=-"),
        "an unreported wait prints `-`: {text}"
    );
}

/// Review, item 1: the wait ends when the wrapper starts the worker, not when the claim ran.
/// A task is queued behind a full slot, claimed, and only then does the wrapper start (after a
/// sleep that stands for its setup); the queue record must carry the wrapper's own `started_at`
/// — the value `execution.json` carries — and the wait must cover the setup.
#[test]
fn the_queue_wait_ends_when_the_wrapper_starts_the_worker() {
    let repo = Repo::new("queue-wait-wrapper");
    let store = setup(&repo);
    let plan = add_plan(&store, "wrapper");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    let first = read_json(&eligible(&repo, &waited));
    let eligible_at = utc(first["eligible_at"].as_str().expect("eligible_at"));

    release(&store, &blocker);
    let claim = attempt_ops::claim(&store, &waited).expect("the waited task claims");
    let claimed = read_json(&queue(&repo, &waited, claim.attempt));
    assert!(
        claimed.get("started_at").is_none(),
        "the claim alone records no start: {claimed}"
    );

    // The wrapper's setup after the claim takes time; the wait must cover it.
    std::thread::sleep(Duration::from_millis(1200));
    let args = work_args(
        &waited,
        claim.attempt,
        &claim.token,
        &["--flow", "default", "--command", "[\"true\"]"],
        &[],
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = repo.work(&refs, "the brief\n");
    // The command writes no `Result`, so the wrapper ends with the `terminated` outcome; it
    // still writes its records, which is what this test needs.
    assert!(
        repo.run_dir(&waited, claim.attempt)
            .join("execution.json")
            .is_file(),
        "the wrapper wrote its record: {run:?}"
    );

    let execution = read_json(&repo.run_dir(&waited, claim.attempt).join("execution.json"));
    let record = read_json(&queue(&repo, &waited, claim.attempt));
    let started_at = execution["started_at"]
        .as_str()
        .expect("the execution start");
    assert_eq!(
        record["started_at"],
        json!(started_at),
        "the queue wait ends where the attempt started: {record}"
    );
    let wait = record["queue_wait_secs"].as_u64().expect("queue_wait_secs");
    let expected = u64::try_from((utc(started_at) - eligible_at).num_seconds()).unwrap_or(0);
    assert_eq!(
        wait, expected,
        "the wait covers the wrapper setup: {record}"
    );
    assert!(
        wait >= 1,
        "the post-claim setup is part of the wait: {record}"
    );
}

/// Review, item 2: a plan set to `auto` `off` after a task began waiting drops the record, and
/// a later `run` begins a fresh wait rather than keeping the earlier time.
#[test]
fn a_plan_set_to_auto_off_drops_the_wait_and_a_later_run_starts_fresh() {
    let repo = Repo::new("queue-wait-auto-off");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-off");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    let path = eligible(&repo, &waited);
    let first = read_json(&path);
    let first_at = utc(first["eligible_at"].as_str().expect("eligible_at"));

    plan_ops::set_auto(&store, &plan, "off").expect("auto off");
    assert!(!path.exists(), "auto `off` drops the wait");

    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    std::thread::sleep(Duration::from_millis(1100));
    pass(&store);
    let second = read_json(&path);
    let second_at = utc(second["eligible_at"].as_str().expect("eligible_at"));
    assert!(
        second_at > first_at,
        "the resumed plan begins a fresh wait: {second}"
    );

    release(&store, &blocker);
    let claim = attempt_ops::claim(&store, &waited).expect("the waited task claims");
    let started = utc(&plan_ops::utc_now()) + chrono::Duration::seconds(1);
    complete(
        &repo,
        &waited,
        claim.attempt,
        &started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    let record = read_json(&queue(&repo, &waited, claim.attempt));
    let recorded_eligible = utc(record["eligible_at"].as_str().expect("eligible_at"));
    assert_eq!(
        recorded_eligible, second_at,
        "the claim consumes the fresh time, not the stale one: {record}"
    );
}

/// Review, item 2: an amendment that sends a confirmed plan back to `draft` makes it
/// unstartable, so the record a ready task began drops.
#[test]
fn a_plan_amendment_back_to_draft_drops_the_wait() {
    let repo = Repo::new("queue-wait-amend");
    let store = setup(&repo);
    let plan = add_plan(&store, "amend");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);

    pass(&store);
    let path = eligible(&repo, &waited);
    assert!(path.is_file(), "the wait began before the amendment");

    plan_ops::update_plan(&store, &plan, None, None, Some("the plan changed"))
        .expect("the amendment");
    let stored = store.load_plan(&plan).expect("the plan reads").item;
    assert_eq!(stored.status, "draft");
    assert_eq!(stored.auto, "off");
    assert!(
        !path.exists(),
        "a draft plan starts nothing, so the wait drops"
    );
}

/// Review, item 2: the automatic-rework limit pauses the plan, so a ready task's wait drops.
#[test]
fn an_automatic_rework_pause_drops_the_wait() {
    let repo = Repo::new("queue-wait-auto-rework");
    let store = setup(&repo);
    let plan = add_plan(&store, "auto-rework");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    let reworked = add_task(&store, &plan, "reworked", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);
    pass(&store);
    let path = eligible(&repo, &waited);
    assert!(path.is_file(), "the wait began before the pause");

    // The task is one automatic rework past the default limit of 2.
    store
        .change_task(&reworked, |task| {
            task.enter(TaskState::Review, "rework_pending");
            task.auto_rework = 2;
            Ok(())
        })
        .expect("the reworked task waits for a rework");
    let paused = attempt_ops::record_auto_rework(&store, &reworked)
        .expect("the rework is counted")
        .expect("the limit paused the plan");
    assert_eq!(paused.plan, plan);
    assert_eq!(
        store.load_plan(&plan).expect("the plan reads").item.auto,
        "off"
    );
    assert!(!path.exists(), "the paused plan drops the wait");
}

/// Review, item 2: a plan budget that pauses the plan drops a ready task's wait.
#[test]
fn a_plan_budget_pause_drops_the_wait() {
    let repo = Repo::new("queue-wait-budget");
    let store = setup(&repo);
    let plan = add_plan(&store, "budget");
    let blocker = add_task(&store, &plan, "blocker", &[]);
    let waited = add_task(&store, &plan, "waited", &[]);
    let budgeted = add_task(&store, &plan, "budgeted", &[]);
    confirm_run(&store, &plan);
    claim_blocker(&store, &blocker);
    pass(&store);
    let path = eligible(&repo, &waited);
    assert!(path.is_file(), "the wait began before the pause");

    store
        .change_plan(&plan, |plan| {
            plan.max_cost_usd = Some(0.5);
            Ok(())
        })
        .expect("the plan budget");
    store
        .change_task(&budgeted, |task| {
            task.enter(TaskState::Review, "verifying");
            Ok(())
        })
        .expect("the budgeted task waits for its review");
    let paused = attempt_ops::charge_review_cost(&store, &budgeted, "budgeted-1", 1.0)
        .expect("the review cost is charged");
    assert!(paused, "the plan budget paused the task");
    assert_eq!(
        store.load_plan(&plan).expect("the plan reads").item.auto,
        "off"
    );
    assert!(!path.exists(), "the paused plan drops the wait");
}

/// Edge 9: a task skipped only because the hub's machine-wide cap is full gets
/// `eligible_at.json`.
#[test]
fn a_task_skipped_only_by_the_machine_cap_gets_a_wait_record() {
    let repo = Repo::new("queue-wait-machine");
    let store = setup(&repo);
    let plan = add_plan(&store, "machine");
    let task = add_task(&store, &plan, "work", &[]);
    confirm_run(&store, &plan);

    let no_room = || Ok::<bool, radian_arc::store::Error>(false);
    let mut state = LoopState::new(&store);
    let mut sink = |_: &str| {};
    state
        .pass(&store, &mut sink, Some(&no_room))
        .expect("the pass runs");
    assert!(
        eligible(&repo, &task).is_file(),
        "the machine-wide cap is a skip path that begins the wait"
    );
}

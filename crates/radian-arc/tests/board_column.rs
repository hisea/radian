//! `board_column` in `radian arc list --json` and `show --json`: the arc TUI board's column for
//! each task, computed from the same store state the board reads, so a client of the CLI or the
//! hub's item routes does not have to guess it from reason text.
//!
//! The plans and tasks are real files in a scratch store (`Store::at`), and the records the rule
//! reads (a verdict, its `review.json`, a `paused.json`, the reject note) are real files too,
//! written the way arc writes them.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::gate::{Stage, Verdict, verdict_path};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, attempt_ops, config};
use serde_json::{Value, json};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-board-column-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn store(dir: &Path) -> Store {
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
    Store::at(dir)
}

fn add_plan(store: &Store, slug: &str) -> String {
    store.add_plan(slug, slug, "a request\n").unwrap()
}

fn confirm(store: &Store, plan: &str, auto: &str) {
    store
        .change_plan(plan, |plan| {
            plan.status = "confirmed".to_owned();
            plan.auto = auto.to_owned();
            Ok(())
        })
        .unwrap();
}

fn add_task(store: &Store, plan: &str, slug: &str, state: TaskState, phase: &str) -> String {
    let id = store
        .add_task(
            plan,
            slug,
            &format!("the {slug} task"),
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_task(&id, |task| {
            task.state = state;
            task.phase = phase.to_owned();
            task.attempt = 1;
            Ok(())
        })
        .unwrap();
    id
}

/// The `board_column` `list_json` writes for `task`.
fn listed_column(store: &Store, task: &str) -> Value {
    let value = radian_arc::store::cli::list_json(store, None, false).expect("the JSON builds");
    for plan in value.as_array().expect("an array of plans") {
        for entry in plan["tasks"].as_array().expect("an array of tasks") {
            if entry["id"] == task {
                return entry["board_column"].clone();
            }
        }
    }
    panic!("no task `{task}` in {value}");
}

/// The task's `review.json`, which names the failing stage and its verdict.
fn write_reason(store: &Store, task: &str, verdict: &Path) {
    let dir = attempt_ops::run_dir(store, task, 1);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(radian_arc::review::REVIEW_FILE),
        json!({
            "action": "verify",
            "stage": "commands",
            "reason": "command_failed",
            "verdict": verdict.to_string_lossy(),
            "text": "commands: command_failed",
        })
        .to_string(),
    )
    .unwrap();
}

/// A failed `commands` verdict for the task's `head_sha`, with the main it ran on: what a
/// scheduling pass can reverify once main moves.
fn write_command_failure(store: &Store, task: &str) {
    let head = "c".repeat(40);
    store
        .change_task(task, |task| {
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .unwrap();
    let label = format!("{task}-1");
    let mut verdict = Verdict::human(head, false, Some("command_failed".to_owned()));
    verdict.stage = Stage::Commands;
    verdict.main_sha = Some("a".repeat(40));
    let path = verdict_path(store.repo(), task, &label, Stage::Commands).unwrap();
    verdict.write_new(&path).unwrap();
    write_reason(store, task, &path);
}

/// The owner's reject note, which no automatic reverify follows.
fn write_reject_note(store: &Store, task: &str) {
    let dir = attempt_ops::run_dir(store, task, 1);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(radian_arc::review::REJECT_FILE), "not like this\n").unwrap();
}

#[test]
fn a_command_failure_with_reverifies_left_verifies_and_then_needs_you() {
    let scratch = Scratch::new("reverify");
    let store = store(&scratch.dir);
    let plan = add_plan(&store, "rework");
    let task = add_task(&store, &plan, "rework", TaskState::Review, "rework_pending");
    confirm(&store, &plan, "run");
    write_command_failure(&store, &task);
    assert_eq!(
        listed_column(&store, &task),
        json!("verifying"),
        "a scheduling pass reverifies it once main moves"
    );

    // The automatic reverify count is used up: nothing automatic is coming.
    let dir = store.root().join("runs").join(&task);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(radian_arc::review::AUTO_REVERIFY_FILE),
        json!({
            "reverified": [
                {"head_sha": "c".repeat(40), "main_sha": "d".repeat(40), "at": "2026-10-20T10:00:00Z"},
                {"head_sha": "c".repeat(40), "main_sha": "e".repeat(40), "at": "2026-10-20T11:00:00Z"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        listed_column(&store, &task),
        json!("needs_you"),
        "the count is used up: the owner acts"
    );
}

#[test]
fn a_reject_note_puts_the_task_in_needs_you() {
    let scratch = Scratch::new("reject");
    let store = store(&scratch.dir);
    let plan = add_plan(&store, "rework");
    let task = add_task(&store, &plan, "rework", TaskState::Review, "rework_pending");
    confirm(&store, &plan, "run");
    write_command_failure(&store, &task);
    assert_eq!(
        listed_column(&store, &task),
        json!("verifying"),
        "without a note the command failure is one a pass can reverify"
    );

    write_reject_note(&store, &task);
    assert_eq!(
        listed_column(&store, &task),
        json!("needs_you"),
        "a reject note is no automatic rework"
    );
}

#[test]
fn an_awaiting_human_task_is_verifying_at_merge_and_needs_you_at_run() {
    let scratch = Scratch::new("awaiting");
    let store = store(&scratch.dir);
    let merge = add_plan(&store, "merge");
    let running = add_plan(&store, "run");
    let merging = add_task(
        &store,
        &merge,
        "merging",
        TaskState::Review,
        "awaiting_human",
    );
    let waiting = add_task(
        &store,
        &running,
        "waiting",
        TaskState::Review,
        "awaiting_human",
    );
    confirm(&store, &merge, "merge");
    confirm(&store, &running, "run");
    assert_eq!(
        listed_column(&store, &merging),
        json!("verifying"),
        "auto merge merges it once the review passed"
    );
    assert_eq!(
        listed_column(&store, &waiting),
        json!("needs_you"),
        "auto run stops at awaiting_human"
    );
}

#[test]
fn every_open_task_of_a_plan_paused_through_a_task_needs_you() {
    let scratch = Scratch::new("paused");
    let store = store(&scratch.dir);
    let plan = add_plan(&store, "paused");
    let rework = add_task(&store, &plan, "rework", TaskState::Review, "rework_pending");
    let ready = add_task(&store, &plan, "ready", TaskState::Todo, "ready");
    let verifying = add_task(&store, &plan, "verifying", TaskState::Review, "verifying");
    let merged = add_task(&store, &plan, "merged", TaskState::Done, "merged");
    let failed = add_task(&store, &plan, "failed", TaskState::Failed, "exhausted");
    confirm(&store, &plan, "run");
    // The automatic rework limit is reached: arc pauses the plan through this task.
    store
        .change_task(&rework, |task| {
            task.auto_rework = store.config().unwrap().limits.max_auto_rework;
            Ok(())
        })
        .unwrap();
    let paused = attempt_ops::record_auto_rework(&store, &rework)
        .expect("the pause is recorded")
        .expect("the limit was reached");
    assert_eq!(paused.plan, plan, "the record names the plan");

    for task in [&rework, &ready, &verifying] {
        assert_eq!(
            listed_column(&store, task),
            json!("needs_you"),
            "every open task of a paused plan waits for the owner: {task}"
        );
    }
    for task in [&merged, &failed] {
        assert_eq!(
            listed_column(&store, task),
            Value::Null,
            "a finished task has no column: {task}"
        );
    }
}

#[test]
fn each_state_and_phase_maps_to_its_column() {
    let scratch = Scratch::new("states");
    let store = store(&scratch.dir);
    let plan = add_plan(&store, "states");
    let proposed = add_task(&store, &plan, "proposed", TaskState::Todo, "proposed");
    let ready = add_task(&store, &plan, "ready", TaskState::Todo, "ready");
    let doing = add_task(&store, &plan, "doing", TaskState::Doing, "running");
    let merged = add_task(&store, &plan, "merged", TaskState::Done, "merged");
    confirm(&store, &plan, "run");

    assert_eq!(listed_column(&store, &proposed), json!("needs_you"));
    assert_eq!(listed_column(&store, &ready), json!("todo"));
    assert_eq!(listed_column(&store, &doing), json!("doing"));
    assert_eq!(listed_column(&store, &merged), Value::Null);
}

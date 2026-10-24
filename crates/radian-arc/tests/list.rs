//! `radian arc list`'s text: the default hides every `done.merged` task, every finished plan
//! and every cancelled plan, and counts what it left out; `--all` prints everything. The JSON
//! follows the same cancelled-plan filter and never hides a merged task.
//!
//! Every test builds a throwaway repository with a throwaway bare `origin` under the system
//! temp directory, and removes it at the end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::store::Store;
use radian_arc::store::cli::{list_json, list_plan_text, list_text};
use radian_arc::store::model::TaskState;

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-list-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the parent is creatable");
        }
        fs::write(&path, text).expect("the file is writable");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "arc test")
        .env("GIT_AUTHOR_EMAIL", "arc@example.com")
        .env("GIT_COMMITTER_NAME", "arc test")
        .env("GIT_COMMITTER_EMAIL", "arc@example.com")
        .args(["-c", "core.hooksPath="])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit on `main`, pushed to a local bare `origin`.
fn fixture(label: &str) -> Scratch {
    let scratch = Scratch::new(label);
    git(&scratch.dir, &["init", "-q", "--bare", "origin.git"]);
    git(
        &scratch.path("origin.git"),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(&scratch.dir, &["init", "-q", "repo"]);
    git(
        &scratch.repo(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    scratch.write("repo/README.md", "hello\n");
    git(&scratch.repo(), &["add", "README.md"]);
    git(&scratch.repo(), &["commit", "-q", "-m", "first"]);
    let origin = scratch.path("origin.git");
    git(
        &scratch.repo(),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&scratch.repo(), &["push", "-q", "-u", "origin", "main"]);
    git(&scratch.repo(), &["remote", "set-head", "origin", "main"]);
    scratch
}

fn initialized(scratch: &Scratch) -> Store {
    Store::init(&scratch.repo(), false).expect("init succeeds");
    Store::at(scratch.repo())
}

/// Moves a task to one state and phase, the way arc's own transitions do.
fn set_state(store: &Store, task: &str, state: TaskState, phase: &str) {
    store
        .change_task(task, |task| {
            task.state = state;
            task.phase = phase.to_owned();
            Ok(())
        })
        .expect("the task changes");
}

/// Three plans: a finished one with two merged tasks, one with a merged task, a task ready to
/// start, and a failed task, and one with no tasks at all.
struct Fixture {
    store: Store,
    finished: String,
    mixed: String,
    empty: String,
    mixed_merged: String,
    mixed_ready: String,
    mixed_failed: String,
}

fn three_plans(label: &str) -> (Scratch, Fixture) {
    let scratch = fixture(label);
    let store = initialized(&scratch);

    let finished = store
        .add_plan("finished", "Finished plan", "request")
        .unwrap();
    let done_a = store
        .add_task(&finished, "one", "First", vec![], false, vec![], "brief")
        .unwrap();
    let done_b = store
        .add_task(&finished, "two", "Second", vec![], false, vec![], "brief")
        .unwrap();
    set_state(&store, &done_a, TaskState::Done, "merged");
    set_state(&store, &done_b, TaskState::Done, "merged");

    let mixed = store.add_plan("mixed", "Mixed plan", "request").unwrap();
    let mixed_merged = store
        .add_task(&mixed, "one", "Merged one", vec![], false, vec![], "brief")
        .unwrap();
    let mixed_ready = store
        .add_task(&mixed, "two", "Ready two", vec![], false, vec![], "brief")
        .unwrap();
    let mixed_failed = store
        .add_task(
            &mixed,
            "three",
            "Failed three",
            vec![],
            false,
            vec![],
            "brief",
        )
        .unwrap();
    set_state(&store, &mixed_merged, TaskState::Done, "merged");
    set_state(&store, &mixed_ready, TaskState::Todo, "ready");
    set_state(&store, &mixed_failed, TaskState::Failed, "exhausted");

    let empty = store.add_plan("empty", "Empty plan", "request").unwrap();

    let fixture = Fixture {
        store,
        finished,
        mixed,
        empty,
        mixed_merged,
        mixed_ready,
        mixed_failed,
    };
    (scratch, fixture)
}

/// The default leaves out every merged task and the whole finished plan, counts both in its
/// last line, and keeps a `failed.*` task visible; an empty plan is shown.
#[test]
fn the_default_hides_merged_tasks_and_finished_plans() {
    let (_scratch, fixture) = three_plans("default");
    let views = fixture.store.plans().expect("the plans read");
    let text = list_text(&views, false);

    let expected = format!(
        concat!(
            "{}  draft  auto: off  priority: P3  1/3 merged  total_cost_usd: 0  Mixed plan\n",
            "  {}  todo.ready  attempt 0  Ready two\n",
            "  {}  failed.exhausted  attempt 0  Failed three\n",
            "{}  draft  auto: off  priority: P3  0/0 merged  total_cost_usd: 0  Empty plan\n",
            "1 finished plan and 3 merged tasks hidden (--all to show)\n",
        ),
        fixture.mixed, fixture.mixed_ready, fixture.mixed_failed, fixture.empty
    );
    assert_eq!(text, expected);
    assert!(
        !text.contains(&fixture.finished),
        "the finished plan is hidden: {text}"
    );
    assert!(
        !text.contains(&fixture.mixed_merged),
        "a merged task of a shown plan is hidden: {text}"
    );
}

/// `--all` prints every plan and every task, with the same headers, and no hidden line.
#[test]
fn all_prints_every_plan_and_task() {
    let (_scratch, fixture) = three_plans("all");
    let views = fixture.store.plans().expect("the plans read");
    let text = list_text(&views, true);

    for id in [
        &fixture.finished,
        &fixture.mixed,
        &fixture.empty,
        &fixture.mixed_merged,
        &fixture.mixed_ready,
        &fixture.mixed_failed,
    ] {
        assert!(text.contains(id.as_str()), "`{id}` is printed: {text}");
    }
    assert!(
        text.contains(&format!(
            "{}  draft  auto: off  priority: P3  2/2 merged  total_cost_usd: 0  Finished plan\n",
            fixture.finished
        )),
        "the finished plan keeps its header and its count: {text}"
    );
    assert!(
        text.contains(&format!(
            "{}  draft  auto: off  priority: P3  1/3 merged  total_cost_usd: 0  Mixed plan\n",
            fixture.mixed
        )),
        "the mixed plan's header counts its merged task: {text}"
    );
    assert!(
        !text.contains("hidden"),
        "nothing is hidden with --all: {text}"
    );
}

/// `--plan` of a finished plan still prints its header line, so the plan is visible, and the
/// hidden line counts only its tasks.
#[test]
fn the_plan_of_a_finished_plan_keeps_its_header() {
    let (_scratch, fixture) = three_plans("plan-finished");
    let view = fixture.store.plan_view(&fixture.finished).expect("reads");
    let text = list_plan_text(&view, false);

    assert_eq!(
        text,
        format!(
            concat!(
                "{}  draft  auto: off  priority: P3  2/2 merged  total_cost_usd: 0  Finished plan\n",
                "2 merged tasks hidden (--all to show)\n",
            ),
            fixture.finished
        )
    );
}

/// When every plan is finished, the hidden line is the whole output.
#[test]
fn the_hidden_line_is_the_whole_output_when_everything_is_hidden() {
    let scratch = fixture("all-hidden");
    let store = initialized(&scratch);
    let plan = store.add_plan("solo", "Solo", "request").unwrap();
    let task = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief")
        .unwrap();
    set_state(&store, &task, TaskState::Done, "merged");

    let views = store.plans().expect("the plans read");
    assert_eq!(
        list_text(&views, false),
        "1 finished plan and 1 merged task hidden (--all to show)\n"
    );
}

/// One plan and one task each use the singular wording; a zero part is left out.
#[test]
fn the_hidden_line_uses_singular_and_omits_a_zero_part() {
    let scratch = fixture("singular");
    let store = initialized(&scratch);

    // One merged task in a plan that also has unfinished work: no plan is hidden.
    let mixed = store.add_plan("mixed", "Mixed", "request").unwrap();
    let merged = store
        .add_task(&mixed, "one", "One", vec![], false, vec![], "brief")
        .unwrap();
    store
        .add_task(&mixed, "two", "Two", vec![], false, vec![], "brief")
        .unwrap();
    set_state(&store, &merged, TaskState::Done, "merged");

    let views = store.plans().expect("the plans read");
    let text = list_text(&views, false);
    assert!(
        text.ends_with("1 merged task hidden (--all to show)\n"),
        "{text}"
    );
    assert!(!text.contains("finished plan"), "{text}");

    // Two finished plans with one merged task each: the plan count is plural, the task not.
    let scratch = fixture("singular-plans");
    let store = initialized(&scratch);
    for (slug, title) in [("one", "One"), ("two", "Two")] {
        let plan = store.add_plan(slug, title, "request").unwrap();
        let task = store
            .add_task(&plan, "one", "One", vec![], false, vec![], "brief")
            .unwrap();
        set_state(&store, &task, TaskState::Done, "merged");
    }
    let views = store.plans().expect("the plans read");
    assert_eq!(
        list_text(&views, false),
        "2 finished plans and 2 merged tasks hidden (--all to show)\n"
    );
}

/// A store with no plans at all says so, instead of printing an empty hidden line.
#[test]
fn an_empty_store_says_there_are_no_plans() {
    let scratch = fixture("no-plans");
    let store = initialized(&scratch);
    let views = store.plans().expect("the plans read");
    assert_eq!(list_text(&views, false), "no plans\n");
    assert_eq!(list_text(&views, true), "no plans\n");
}

/// The JSON follows the text filter for a cancelled plan: it is hidden without `--all` and
/// present with it; `--plan` and `show` still read the plan and its tasks.
#[test]
fn a_cancelled_plan_is_hidden_until_all_in_text_and_json() {
    let scratch = fixture("cancelled");
    let store = initialized(&scratch);
    let plan = store
        .add_plan("cancelled", "Cancelled plan", "request")
        .unwrap();
    let task = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief")
        .unwrap();
    radian_arc::attempt::cancel_any(&store, &plan).expect("the plan cancels");

    let views = store.plans().expect("the plans read");
    let text = list_text(&views, false);
    assert!(!text.contains(&plan), "the plan is hidden: {text}");
    assert_eq!(text, "1 cancelled plan hidden (--all to show)\n");
    let all = list_text(&views, true);
    assert!(all.contains(&plan), "`--all` shows it: {all}");
    assert!(all.contains(&task), "`--all` shows its tasks: {all}");
    assert!(!all.contains("hidden"), "{all}");

    let hidden = list_json(&store, None, false).expect("the JSON builds");
    assert_eq!(hidden.as_array().map(Vec::len), Some(0), "{hidden}");
    let shown = list_json(&store, None, true).expect("the JSON builds");
    assert_eq!(shown.as_array().map(Vec::len), Some(1), "{shown}");
    assert_eq!(shown[0]["status"], "cancelled");
    assert_eq!(shown[0]["tasks"][0]["state"], "failed");
    assert_eq!(shown[0]["tasks"][0]["phase"], "cancelled");

    let named = list_plan_text(&store.plan_view(&plan).expect("the plan reads"), false);
    assert!(named.contains(&plan), "`--plan` names it: {named}");
    assert!(
        store.show(&plan).is_ok(),
        "`show` still reads a cancelled plan"
    );
    assert!(store.show(&task).is_ok(), "`show` still reads its tasks");
}

/// The JSON holds every plan and every task when no plan is cancelled: the merged ones
/// included.
#[test]
fn the_json_holds_every_plan_and_every_task() {
    let (_scratch, fixture) = three_plans("json");
    let value = list_json(&fixture.store, None, false).expect("the JSON builds");

    let plans = value.as_array().expect("an array of plans");
    assert_eq!(plans.len(), 3, "every plan is in the JSON: {value}");
    let finished = plans
        .iter()
        .find(|plan| plan["id"].as_str() == Some(fixture.finished.as_str()))
        .expect("the finished plan is in the JSON");
    assert_eq!(finished["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(finished["tasks"][0]["state"], "done");
    assert_eq!(finished["tasks"][0]["phase"], "merged");
    let mixed = plans
        .iter()
        .find(|plan| plan["id"].as_str() == Some(fixture.mixed.as_str()))
        .expect("the mixed plan is in the JSON");
    assert_eq!(mixed["tasks"].as_array().unwrap().len(), 3);
    let empty = plans
        .iter()
        .find(|plan| plan["id"].as_str() == Some(fixture.empty.as_str()))
        .expect("the empty plan is in the JSON");
    assert_eq!(empty["tasks"].as_array().unwrap().len(), 0);
}

/// A merged task and a cancelled follow-up make a finished plan (ADR 0048 decision 3): the
/// default list leaves it out, `--all` shows it with both tasks, and its progress reads `1/1`
/// because a cancelled task is not work left to do.
#[test]
fn a_merged_task_and_a_cancelled_follow_up_finish_the_plan_and_read_1_of_1() {
    let scratch = fixture("merged-cancelled");
    let store = initialized(&scratch);
    let plan = store.add_plan("done", "Done plan", "request").unwrap();
    let merged = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief")
        .unwrap();
    let cancelled = store
        .add_task(&plan, "two", "Two", vec![], false, vec![], "brief")
        .unwrap();
    set_state(&store, &merged, TaskState::Done, "merged");
    set_state(&store, &cancelled, TaskState::Failed, "cancelled");

    let views = store.plans().expect("the plans read");
    let text = list_text(&views, false);
    assert!(!text.contains(&plan), "the finished plan is hidden: {text}");
    assert_eq!(
        text,
        "1 finished plan and 1 merged task hidden (--all to show)\n"
    );

    let all = list_text(&views, true);
    assert!(
        all.contains(&format!(
            "{plan}  draft  auto: off  priority: P3  1/1 merged  total_cost_usd: 0  Done plan"
        )),
        "a cancelled task is not in the progress total: {all}"
    );
    assert!(all.contains(&merged), "the merged task: {all}");
    assert!(
        all.contains(&format!("{cancelled}  failed.cancelled  attempt 0  Two")),
        "the cancelled task stays listed: {all}"
    );
    assert!(!all.contains("hidden"), "{all}");

    let named = list_plan_text(&store.plan_view(&plan).expect("the plan reads"), false);
    assert!(
        named.contains("1/1 merged"),
        "a named plan keeps its header: {named}"
    );
    assert!(
        named.contains(&cancelled),
        "its cancelled task stays listed: {named}"
    );
    assert_eq!(
        named.lines().last(),
        Some("1 merged task hidden (--all to show)")
    );
}

/// A merged task with an open follow-up is not a finished plan: it stays in the default list,
/// its merged task is hidden, and its progress reads `1/2`.
#[test]
fn a_merged_task_and_a_ready_task_keep_the_plan_shown_and_read_1_of_2() {
    let scratch = fixture("merged-ready");
    let store = initialized(&scratch);
    let plan = store.add_plan("open", "Open plan", "request").unwrap();
    let merged = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief")
        .unwrap();
    let ready = store
        .add_task(&plan, "two", "Two", vec![], false, vec![], "brief")
        .unwrap();
    set_state(&store, &merged, TaskState::Done, "merged");

    let views = store.plans().expect("the plans read");
    let text = list_text(&views, false);
    assert!(
        text.contains(&format!(
            "{plan}  draft  auto: off  priority: P3  1/2 merged  total_cost_usd: 0  Open plan"
        )),
        "{text}"
    );
    assert!(text.contains(&ready), "the open task is shown: {text}");
    assert!(!text.contains(&merged), "the merged task is hidden: {text}");
    assert_eq!(
        text.lines().last(),
        Some("1 merged task hidden (--all to show)")
    );
}

/// A plan whose tasks are all `failed.cancelled` is cancelled by its tasks (ADR 0048 decision
/// 3): the default list hides it as it hides a `cancelled` plan, `--all` shows it, and its
/// progress reads `0/0`.
#[test]
fn a_plan_whose_tasks_are_all_cancelled_is_cancelled_and_reads_0_of_0() {
    let scratch = fixture("tasks-cancelled");
    let store = initialized(&scratch);
    let plan = store
        .add_plan("stopped", "Stopped plan", "request")
        .unwrap();
    let one = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief")
        .unwrap();
    let two = store
        .add_task(&plan, "two", "Two", vec![], false, vec![], "brief")
        .unwrap();
    set_state(&store, &one, TaskState::Failed, "cancelled");
    set_state(&store, &two, TaskState::Failed, "cancelled");

    let views = store.plans().expect("the plans read");
    assert!(
        views[0].is_cancelled(),
        "all its tasks cancelled: it is cancelled"
    );
    assert!(!views[0].is_finished(), "nothing merged: not finished");
    let text = list_text(&views, false);
    assert!(!text.contains(&plan), "it is hidden: {text}");
    assert_eq!(text, "1 cancelled plan hidden (--all to show)\n");

    let all = list_text(&views, true);
    assert!(
        all.contains(&format!(
            "{plan}  draft  auto: off  priority: P3  0/0 merged  total_cost_usd: 0  Stopped plan"
        )),
        "{all}"
    );
    assert!(all.contains(&one) && all.contains(&two), "{all}");
    assert!(all.contains("failed.cancelled"), "{all}");

    let hidden = list_json(&store, None, false).expect("the JSON builds");
    assert_eq!(hidden.as_array().map(Vec::len), Some(0), "{hidden}");
    let shown = list_json(&store, None, true).expect("the JSON builds");
    assert_eq!(shown.as_array().map(Vec::len), Some(1), "{shown}");
    assert!(store.show(&plan).is_ok(), "`show` still reads it");
}

/// A plan with no tasks is never finished and never cancelled: the default list shows it, with
/// `0/0` progress.
#[test]
fn a_plan_with_no_tasks_is_shown() {
    let scratch = fixture("no-tasks");
    let store = initialized(&scratch);
    let plan = store.add_plan("empty", "Empty plan", "request").unwrap();
    let view = store.plan_view(&plan).expect("the plan reads");
    assert!(!view.is_finished(), "no task means no finished plan");
    assert!(!view.is_cancelled(), "no task means no cancelled plan");
    assert_eq!(view.progress(), (0, 0));
    let text = list_text(&store.plans().expect("the plans read"), false);
    assert!(
        text.contains(&format!(
            "{plan}  draft  auto: off  priority: P3  0/0 merged  total_cost_usd: 0  Empty plan"
        )),
        "{text}"
    );
    assert!(!text.contains("hidden"), "{text}");
}

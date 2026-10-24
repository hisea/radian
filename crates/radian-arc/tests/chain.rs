//! The `after` chain of a task, both ways, and `radian arc confirm --dry-run`.
//!
//! The store tests build a throwaway repository under the system temp directory; the CLI
//! tests run the `radian` binary against a [`common::Repo`]. A dry run must refuse exactly as
//! a real confirm does and must leave every file under `.radian/arc/` byte for byte alone.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use common::Repo;
use radian_arc::store::cli::{show_json, show_text};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};

// ---- a store under a throwaway git repository ---------------------------------------

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-chain-{label}-{}-{n}",
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

fn add_task(store: &Store, plan: &str, slug: &str, after: &[String]) -> String {
    store
        .add_task(
            plan,
            slug,
            &format!("Task {slug}"),
            after.to_vec(),
            false,
            Vec::new(),
            "brief",
        )
        .expect("the task is added")
}

fn set_state(store: &Store, task: &str, state: TaskState, phase: &str) {
    store
        .change_task(task, |task| {
            task.state = state;
            task.phase = phase.to_owned();
            Ok(())
        })
        .expect("the task changes");
}

/// The task's file text, edited in place the way a person's hand edit would, then adopted.
fn hand_edit_and_adopt(scratch: &Scratch, store: &Store, task: &str, from: &str, to: &str) {
    let path = scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{task}.md"));
    let text = fs::read_to_string(&path).expect("the task file reads");
    assert!(text.contains(from), "the fixture has `{from}`: {text}");
    fs::write(&path, text.replace(from, to)).expect("the task file is editable");
    store.adopt(task).expect("an edit of `after` is adoptable");
}

// ---- show: both directions of the chain --------------------------------------------

/// A three-task chain: `show` prints every task a task waits on, nearest first, and the last
/// task's `blocks` lists both others, in the same order in JSON.
#[test]
fn show_prints_the_full_wait_chain_and_every_task_that_blocks_it() {
    let scratch = fixture("chain");
    let store = initialized(&scratch);
    let plan = store.add_plan("chain", "Chain", "request").unwrap();
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", std::slice::from_ref(&a));
    let c = add_task(&store, &plan, "c", std::slice::from_ref(&b));

    let text = show_text(&store, &c).expect("show reads the task");
    assert!(
        text.contains(&format!("waits_on: {b} todo.ready, {a} todo.ready\n")),
        "the chain is not nearest first and complete:\n{text}"
    );

    let text = show_text(&store, &a).expect("show reads the task");
    assert!(
        text.contains(&format!("blocks: {b} todo.ready, {c} todo.ready\n")),
        "the last task does not block both others:\n{text}"
    );

    let value = show_json(&store, &c).expect("show --json builds");
    assert_eq!(
        value["waits_on"],
        serde_json::json!([
            {"id": b, "state": "todo", "phase": "ready"},
            {"id": a, "state": "todo", "phase": "ready"},
        ])
    );
    let value = show_json(&store, &a).expect("show --json builds");
    assert_eq!(
        value["blocks"],
        serde_json::json!([
            {"id": b, "state": "todo", "phase": "ready"},
            {"id": c, "state": "todo", "phase": "ready"},
        ])
    );
    assert_eq!(value["never_runs"], serde_json::Value::Null);
    assert_eq!(value["cycle"], serde_json::Value::Null);
}

/// A `failed.cancelled` task in the chain can never merge, so `show` names it as
/// `never_runs`.
#[test]
fn a_failed_task_in_the_chain_is_reported_as_never_running() {
    let scratch = fixture("never-failed");
    let store = initialized(&scratch);
    let plan = store.add_plan("never", "Never", "request").unwrap();
    let failed = add_task(&store, &plan, "failed", &[]);
    set_state(&store, &failed, TaskState::Failed, "cancelled");
    let waiting = add_task(&store, &plan, "waiting", std::slice::from_ref(&failed));

    let text = show_text(&store, &waiting).expect("show reads the task");
    assert!(
        text.contains(&format!("never_runs: {failed} failed.cancelled\n")),
        "{text}"
    );
    let value = show_json(&store, &waiting).expect("show --json builds");
    assert_eq!(
        value["never_runs"],
        serde_json::json!({"id": failed, "state": "failed", "phase": "cancelled"})
    );
}

/// An adopted `after` id that names no task is `missing`, and it can never finish either.
#[test]
fn an_adopted_after_id_that_names_no_task_is_missing() {
    let scratch = fixture("missing");
    let store = initialized(&scratch);
    let plan = store.add_plan("missing", "Missing", "request").unwrap();
    let task = add_task(&store, &plan, "task", &[]);
    hand_edit_and_adopt(
        &scratch,
        &store,
        &task,
        "after: []\n",
        "after:\n  - TK0099-missing\n",
    );

    let text = show_text(&store, &task).expect("show reads the task");
    assert!(
        text.contains("waits_on: TK0099-missing missing\n"),
        "{text}"
    );
    assert!(
        text.contains("never_runs: TK0099-missing missing\n"),
        "{text}"
    );
    let value = show_json(&store, &task).expect("show --json builds");
    assert_eq!(
        value["waits_on"],
        serde_json::json!([{"id": "TK0099-missing", "state": null, "phase": null}])
    );
    assert_eq!(
        value["never_runs"],
        serde_json::json!({"id": "TK0099-missing", "state": null, "phase": null})
    );
}

/// An adopted cycle prints `cycle:` once and the walk returns: the test is bounded so a walk
/// that follows the cycle again fails here instead of hanging the suite.
#[test]
fn an_adopted_cycle_is_printed_once_and_the_walk_returns() {
    let scratch = fixture("cycle");
    let store = initialized(&scratch);
    let plan = store.add_plan("cycle", "Cycle", "request").unwrap();
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", &[]);
    hand_edit_and_adopt(
        &scratch,
        &store,
        &a,
        "after: []\n",
        &format!("after:\n  - {b}\n"),
    );
    hand_edit_and_adopt(
        &scratch,
        &store,
        &b,
        "after: []\n",
        &format!("after:\n  - {a}\n"),
    );

    let (sender, receiver) = std::sync::mpsc::channel();
    let bounded = store.clone();
    let id = a.clone();
    std::thread::spawn(move || {
        let _ = sender.send(show_text(&bounded, &id).map_err(|error| error.message()));
    });
    let result = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the chain walk returned");
    let text = result.expect("show reads the task");
    let cycle_line = format!("cycle: {a} -> {b} -> {a}\n");
    assert!(text.contains(&cycle_line), "{text}");
    assert_eq!(
        text.matches("cycle:").count(),
        1,
        "the cycle is printed once:\n{text}"
    );
}

// ---- confirm --dry-run and the real confirm's warnings -----------------------------

fn cli_fixture(label: &str) -> (Repo, Store) {
    let repo = Repo::new(label);
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    (repo, store)
}

/// Every file under `.radian/arc/`, relative path to bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let relative = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
                files.insert(relative, fs::read(&path).expect("the file reads"));
            }
        }
    }
    files
}

fn confirmed_plan(store: &Store, slug: &str) -> String {
    store.add_plan(slug, "Plan", "request").expect("add a plan")
}

/// `confirm --dry-run` on a `todo.proposed` task prints what the confirm would do, the chain
/// and the queue place, exits 0, writes nothing, and leaves the task proposed.
#[test]
fn dry_run_on_a_proposed_task_prints_the_chain_and_writes_nothing() {
    let (repo, store) = cli_fixture("dry-task");
    let plan = confirmed_plan(&store, "dry");
    let first = add_task(&store, &plan, "first", &[]);
    plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
    plan_ops::set_auto(&store, &plan, "run").expect("the plan runs");
    let proposed = store
        .add_task_proposed(
            &plan,
            "second",
            "Second",
            vec![first.clone()],
            false,
            Vec::new(),
            "brief",
        )
        .expect("a proposed task is added");

    // The lock file is created by the first state change; removing it proves the dry run does
    // not create one either.
    let lock = repo.arc().join("state.lock");
    fs::remove_file(&lock).expect("the lock file is removable");
    let before = snapshot(&repo.arc());
    let run = repo.radian(&["arc", "confirm", "--dry-run", &proposed]);
    assert_eq!(run.code, 0, "dry run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("would: {proposed} todo.ready\n")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(&format!("{proposed} waits on: {first} todo.ready\n")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(&format!("{proposed} queue: 2 of 2 todo.ready")),
        "{}",
        run.stdout
    );
    let after = snapshot(&repo.arc());
    assert_eq!(before, after, "the dry run wrote a file under .radian/arc/");
    assert!(!lock.exists(), "the dry run created state.lock");
    assert_eq!(
        store
            .load_task(&proposed)
            .expect("the task reads")
            .item
            .phase,
        "proposed",
        "the dry run confirmed the task"
    );
}

/// `confirm --dry-run` on a draft plan prints `would: … confirmed` and one `waits on:` line
/// per `todo.ready` task, in plan order, and writes nothing.
#[test]
fn dry_run_on_a_draft_plan_prints_every_task_and_writes_nothing() {
    let (repo, store) = cli_fixture("dry-plan");
    let plan = confirmed_plan(&store, "draft");
    let a = add_task(&store, &plan, "a", &[]);
    let b = add_task(&store, &plan, "b", std::slice::from_ref(&a));
    let c = add_task(&store, &plan, "c", std::slice::from_ref(&b));

    let before = snapshot(&repo.arc());
    let run = repo.radian(&["arc", "confirm", "--dry-run", &plan]);
    assert_eq!(run.code, 0, "dry run: {}", run.stderr);
    assert!(
        run.stdout.contains(&format!("would: {plan} confirmed\n")),
        "{}",
        run.stdout
    );
    for expected in [
        format!("{a} waits on: nothing\n"),
        format!("{b} waits on: {a} todo.ready\n"),
        format!("{c} waits on: {b} todo.ready, {a} todo.ready\n"),
    ] {
        assert!(run.stdout.contains(&expected), "{expected}\n{}", run.stdout);
    }
    for id in [&a, &b, &c] {
        assert!(
            run.stdout.contains(&format!("{id} queue: ")),
            "{}\n{}",
            id,
            run.stdout
        );
    }
    assert_eq!(before, snapshot(&repo.arc()), "the dry run wrote a file");
    assert_eq!(
        store.load_plan(&plan).expect("the plan reads").item.status,
        "draft",
        "the dry run confirmed the plan"
    );
}

/// A refusal is the same message and exit code as a real confirm's: a `todo.ready` task, an
/// empty plan, a file that changed outside arc, and an id that names nothing.
#[test]
fn dry_run_refuses_exactly_as_confirm_does() {
    let (repo, store) = cli_fixture("dry-refusals");
    let plan = confirmed_plan(&store, "plan");
    let ready = add_task(&store, &plan, "ready", &[]);
    let empty = confirmed_plan(&store, "empty");

    for id in [ready.as_str(), empty.as_str(), "TK0099-missing"] {
        let real = repo.radian(&["arc", "confirm", id]);
        let dry = repo.radian(&["arc", "confirm", "--dry-run", id]);
        assert_ne!(real.code, 0, "{id} was confirmed: {real:?}");
        assert_eq!(dry.code, real.code, "{id}: {real:?} vs {dry:?}");
        assert_eq!(dry.stderr, real.stderr, "{id}");
        assert_eq!(dry.stdout, "", "{id}: the refusal printed a line");
    }

    // A file changed outside arc refuses both the same way, and neither adopts it.
    let path = repo.arc().join("tasks").join(format!("{ready}.md"));
    let text = fs::read_to_string(&path).expect("the task reads");
    fs::write(&path, text.replace("brief", "an edited brief")).expect("the file is editable");
    let real = repo.radian(&["arc", "confirm", &ready]);
    let dry = repo.radian(&["arc", "confirm", "--dry-run", &ready]);
    assert_eq!(real.code, 3, "{real:?}");
    assert_eq!(dry.code, real.code, "{dry:?}");
    assert_eq!(dry.stderr, real.stderr);
    assert!(dry.stderr.contains("changed outside arc"), "{}", dry.stderr);
}

/// A held `state.lock` refuses the dry run as it refuses the real confirm: exit 4.
#[test]
fn dry_run_exits_like_confirm_while_state_lock_is_held() {
    let (repo, store) = cli_fixture("dry-lock");
    let plan = confirmed_plan(&store, "locked");
    add_task(&store, &plan, "task", &[]);

    let lock = store.lock_state().expect("the test takes the lock");
    let real = repo.radian(&["arc", "confirm", &plan]);
    let dry = repo.radian(&["arc", "confirm", "--dry-run", &plan]);
    drop(lock);
    assert_eq!(real.code, 4, "{real:?}");
    assert_eq!(dry.code, 4, "{dry:?}");
    assert_eq!(dry.stderr, real.stderr);
    assert_eq!(
        store.load_plan(&plan).expect("the plan reads").item.status,
        "draft"
    );
}

/// A real confirm of a task whose chain cannot finish prints the `never runs:` warning on
/// stderr, confirms the task, and still exits 0.
#[test]
fn a_real_confirm_warns_about_a_chain_that_can_never_finish() {
    let (repo, store) = cli_fixture("confirm-warning");
    let plan = confirmed_plan(&store, "warn");
    let failed = add_task(&store, &plan, "failed", &[]);
    set_state(&store, &failed, TaskState::Failed, "cancelled");
    plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
    let proposed = store
        .add_task_proposed(
            &plan,
            "proposed",
            "Proposed",
            vec![failed.clone()],
            false,
            Vec::new(),
            "brief",
        )
        .expect("a proposed task is added");

    let run = repo.radian(&["arc", "confirm", &proposed]);
    assert_eq!(run.code, 0, "confirm: {}", run.stderr);
    assert!(
        run.stdout.contains(&format!("{proposed} todo.ready\n")),
        "{}",
        run.stdout
    );
    assert!(
        run.stderr
            .contains(&format!("never runs: {failed} failed.cancelled")),
        "the warning is not on stderr: {}",
        run.stderr
    );
    assert_eq!(
        store
            .load_task(&proposed)
            .expect("the task reads")
            .item
            .phase,
        "ready",
        "the warning stopped the confirm"
    );
}

/// The queue place and its reason are the scheduling loop's words: a task waiting for an
/// `after` task, and a task of an `auto` `off` plan.
#[test]
fn a_dry_run_queue_line_names_the_reason_the_pass_would_wait() {
    let (repo, store) = cli_fixture("dry-queue");
    let plan = confirmed_plan(&store, "queue");
    let first = add_task(&store, &plan, "first", &[]);
    plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
    plan_ops::set_auto(&store, &plan, "run").expect("the plan runs");
    let second = store
        .add_task_proposed(
            &plan,
            "second",
            "Second",
            vec![first.clone()],
            false,
            Vec::new(),
            "brief",
        )
        .expect("a proposed task is added");
    let run = repo.radian(&["arc", "confirm", "--dry-run", &second]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("waiting for {first} to merge")),
        "{}",
        run.stdout
    );

    plan_ops::pause(&store, &plan).expect("the plan is off");
    let run = repo.radian(&["arc", "confirm", "--dry-run", &second]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.contains("`auto` is `off`"), "{}", run.stdout);
}

//! The lead's MCP `update_task` tool (ADR 0048 decision 11) against a fixture store: every
//! field, the phase it keeps, the states and plans it refuses, the shared `after` check, the
//! brief it replaces, the `## Changes` records, and the one transaction it writes.
//!
//! Every test builds a throwaway repository with a throwaway bare `origin` under the system
//! temp directory, and calls the tool through the MCP handler the server dispatches with.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::mcp::ArcServer;
use radian_arc::store::model::{self, TaskState};
use radian_arc::store::{self, Exit, Store, plan_ops};
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
            "radian-arc-update-task-{label}-{}-{n}",
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

/// A fixture store with one plan and no tasks. The `Scratch` guard keeps the repository for
/// the whole test.
fn setup(label: &str) -> (Scratch, Store, String) {
    let scratch = fixture(label);
    Store::init(&scratch.repo(), false).expect("init succeeds");
    let store = Store::at(scratch.repo());
    let plan = store
        .add_plan("plan", "Plan", "the request")
        .expect("a plan");
    (scratch, store, plan)
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
            "the brief",
        )
        .expect("a task")
}

fn set_state(store: &Store, task: &str, state: TaskState, phase: &str) {
    store
        .change_task(task, |task| {
            task.state = state;
            task.phase = phase.to_owned();
            Ok(())
        })
        .expect("the test moves the task");
}

/// One `update_task` call through the MCP handler.
fn update(server: &ArcServer, arguments: Value) -> Result<Value, String> {
    server.dispatch("update_task", arguments.as_object().cloned())
}

fn update_ok(server: &ArcServer, arguments: Value) -> Value {
    update(server, arguments).expect("the update succeeds")
}

fn update_err(server: &ArcServer, arguments: Value) -> String {
    update(server, arguments).expect_err("the update is refused")
}

/// The `- <time> update_task: <fields>` lines of a task body, in order.
fn change_lines(body: &str) -> Vec<(&str, &str)> {
    body.lines()
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(|line| line.split_once(" update_task: "))
        .collect()
}

/// The last change record's fields, with its UTC time checked.
fn last_change_fields(body: &str) -> String {
    let (time, fields) = change_lines(body)
        .pop()
        .expect("the call wrote a change record");
    assert!(model::valid_utc(time), "`{time}` is an RFC 3339 UTC time");
    fields.to_owned()
}

#[test]
fn every_field_changes_and_is_recorded() {
    let (_scratch, store, plan) = setup("fields");
    let task = add_task(&store, &plan, "work", &[]);
    let dependency = add_task(&store, &plan, "first", &[]);
    let server = ArcServer::new(store.clone());

    let updated = update_ok(&server, json!({"task": task, "title": "A better title"}));
    assert_eq!(updated["title"], "A better title");
    assert_eq!(updated["state"], "todo");
    assert_eq!(updated["phase"], "ready", "the phase does not change");
    assert_eq!(
        last_change_fields(updated["body"].as_str().unwrap()),
        "title"
    );

    let updated = update_ok(&server, json!({"task": task, "brief": "a better brief"}));
    let body = updated["body"].as_str().expect("a body");
    assert!(body.starts_with("a better brief\n\n## Changes\n"), "{body}");
    assert!(
        !body.contains("the brief\n"),
        "the old brief is gone: {body}"
    );
    assert_eq!(
        last_change_fields(updated["body"].as_str().unwrap()),
        "brief"
    );

    let updated = update_ok(&server, json!({"task": task, "after": [dependency]}));
    assert_eq!(updated["after"], json!([dependency]));
    assert_eq!(
        last_change_fields(updated["body"].as_str().unwrap()),
        "after"
    );

    let updated = update_ok(&server, json!({"task": task, "allow_noop": true}));
    assert_eq!(updated["allow_noop"], true);
    assert_eq!(
        last_change_fields(updated["body"].as_str().unwrap()),
        "allow_noop"
    );

    let updated = update_ok(&server, json!({"task": task, "extra_stages": ["mutation"]}));
    assert_eq!(updated["extra_stages"], json!(["mutation"]));
    assert_eq!(
        last_change_fields(updated["body"].as_str().unwrap()),
        "extra_stages"
    );

    // Every call appended its own line, in order, and the file the store reads is the file
    // arc wrote.
    let loaded = store.load_task(&task).expect("the task reads");
    assert!(!loaded.changed_outside_arc);
    assert_eq!(
        change_lines(&loaded.item.body).len(),
        5,
        "{}",
        loaded.item.body
    );
    assert_eq!(loaded.item.after, vec![dependency]);
    assert_eq!(loaded.item.allow_noop, Some(true));
    assert_eq!(loaded.item.extra_stages, vec!["mutation".to_owned()]);
}

#[test]
fn a_call_with_no_optional_field_is_refused() {
    let (_scratch, store, plan) = setup("nothing");
    let task = add_task(&store, &plan, "work", &[]);
    let path = store.root().join("tasks").join(format!("{task}.md"));
    let before = fs::read_to_string(&path).expect("the task file reads");
    let server = ArcServer::new(store.clone());

    let error = update_err(&server, json!({"task": task}));
    assert!(error.contains("nothing to update"), "{error}");
    assert_eq!(
        fs::read_to_string(&path).expect("the task file reads"),
        before,
        "a refused call writes nothing"
    );
}

#[test]
fn a_ready_task_stays_ready_and_a_proposed_task_stays_proposed() {
    let (_scratch, store, plan) = setup("phases");
    let ready = add_task(&store, &plan, "ready", &[]);
    let proposed = add_task(&store, &plan, "proposed", &[]);
    set_state(&store, &proposed, TaskState::Todo, model::PROPOSED_PHASE);
    let server = ArcServer::new(store.clone());

    let updated = update_ok(&server, json!({"task": ready, "title": "Again"}));
    assert_eq!(updated["state"], "todo");
    assert_eq!(updated["phase"], "ready");

    let updated = update_ok(&server, json!({"task": proposed, "brief": "a new brief"}));
    assert_eq!(updated["state"], "todo");
    assert_eq!(
        updated["phase"], "proposed",
        "an update never confirms the task"
    );
}

#[test]
fn every_other_state_and_a_cancelled_plan_are_refused() {
    let (_scratch, store, plan) = setup("refusals");
    let doing = add_task(&store, &plan, "doing", &[]);
    let review = add_task(&store, &plan, "review", &[]);
    let done = add_task(&store, &plan, "done", &[]);
    let failed = add_task(&store, &plan, "failed", &[]);
    set_state(&store, &doing, TaskState::Doing, "running");
    set_state(&store, &review, TaskState::Review, "awaiting_human");
    set_state(&store, &done, TaskState::Done, "merged");
    set_state(&store, &failed, TaskState::Failed, "exhausted");
    let server = ArcServer::new(store.clone());

    let error = update_err(&server, json!({"task": doing, "title": "no"}));
    assert!(error.contains("doing.running"), "{error}");

    let error = update_err(&server, json!({"task": review, "title": "no"}));
    assert!(error.contains("review.awaiting_human"), "{error}");
    assert!(
        error.contains("add_rework"),
        "a review task names add_rework: {error}"
    );

    let error = update_err(&server, json!({"task": done, "title": "no"}));
    assert!(error.contains("done.merged"), "{error}");

    let error = update_err(&server, json!({"task": failed, "title": "no"}));
    assert!(error.contains("failed.exhausted"), "{error}");

    // A task of a cancelled plan is refused, and the plan's status is in the error.
    let cancelled = add_task(&store, &plan, "cancelled", &[]);
    store
        .change_plan(&plan, |plan| {
            plan.status = "cancelled".to_owned();
            Ok(())
        })
        .expect("the test cancels the plan");
    let error = update_err(&server, json!({"task": cancelled, "title": "no"}));
    assert!(error.contains("cancelled"), "{error}");
    assert!(
        store.load_task(&cancelled).expect("reads").item.title != "no",
        "the refused call wrote nothing"
    );

    // The refusals are state refusals, not usage errors.
    assert_eq!(
        plan_ops::update_task(&store, &review, Some("no"), None, None, None, None)
            .expect_err("review is refused")
            .exit(),
        Exit::State
    );
}

#[test]
fn a_bad_after_list_is_refused_through_the_shared_check() {
    let (_scratch, store, plan) = setup("after");
    let first = add_task(&store, &plan, "first", &[]);
    let second = add_task(&store, &plan, "second", &[]);
    let other_plan = store
        .add_plan("other", "Other", "the request")
        .expect("a plan");
    let other = add_task(&store, &other_plan, "other", &[]);
    let server = ArcServer::new(store.clone());

    // Another plan's task that is not merged yet.
    let error = update_err(&server, json!({"task": first, "after": [other]}));
    assert!(error.contains("neither a task"), "{error}");
    assert_eq!(
        store.load_task(&first).expect("reads").item.after,
        Vec::<String>::new(),
        "a refused call writes nothing"
    );

    // The task itself.
    let error = update_err(&server, json!({"task": first, "after": [first]}));
    assert!(error.contains("itself"), "{error}");

    // A cycle: `second` waits on `first`, so `first` may not wait on `second`.
    update_ok(&server, json!({"task": second, "after": [first]}));
    let error = update_err(&server, json!({"task": first, "after": [second]}));
    assert!(error.contains("cycle"), "{error}");
    assert_eq!(
        store.load_task(&first).expect("reads").item.after,
        Vec::<String>::new(),
        "the cycle was refused before the write"
    );

    // A merged task of another plan is a valid dependency.
    set_state(&store, &other, TaskState::Done, "merged");
    let updated = update_ok(&server, json!({"task": first, "after": [other]}));
    assert_eq!(updated["after"], json!([other]));
}

#[test]
fn a_brief_change_keeps_every_rework_section_and_the_change_lines_stay_last() {
    let (_scratch, store, plan) = setup("rework");
    let task = add_task(&store, &plan, "work", &[]);
    store
        .change_task(&task, |task| {
            task.body = "the original brief\n\n## Rework 1\n\nfirst rework\n\n\
                         ## Rework 2\n\nsecond rework\n"
                .to_owned();
            Ok(())
        })
        .expect("the test writes two rework sections");
    let server = ArcServer::new(store.clone());

    let updated = update_ok(&server, json!({"task": task, "brief": "a better brief"}));
    let body = updated["body"].as_str().expect("a body");
    assert!(!body.contains("the original brief"), "{body}");
    assert!(body.contains("## Rework 1\n\nfirst rework"), "{body}");
    assert!(body.contains("## Rework 2\n\nsecond rework"), "{body}");
    assert_eq!(body.matches("## Rework").count(), 2, "{body}");

    // A second call keeps both rework sections and appends below the first record.
    let updated = update_ok(&server, json!({"task": task, "title": "Another title"}));
    let body = updated["body"].as_str().expect("a body");
    assert_eq!(body.matches("## Rework").count(), 2, "{body}");
    let changes_at = body.find("## Changes").expect("the changes heading");
    let last_rework = body.rfind("## Rework").expect("the rework headings");
    assert!(changes_at > last_rework, "the records stay last: {body}");
    assert_eq!(change_lines(body).len(), 2, "{body}");
    let (_, fields) = change_lines(body).pop().expect("the second record");
    assert_eq!(fields, "title");
    assert!(
        body.trim_end().ends_with("update_task: title"),
        "the newest record is the body's last line: {body}"
    );
}

#[test]
fn a_bad_title_or_stage_is_refused_the_way_add_task_refuses_it() {
    let (_scratch, store, plan) = setup("checks");
    let task = add_task(&store, &plan, "work", &[]);
    let server = ArcServer::new(store.clone());

    let error = update_err(&server, json!({"task": task, "title": "  "}));
    assert!(error.contains("the title is empty"), "{error}");

    let error = update_err(&server, json!({"task": task, "extra_stages": ["nope"]}));
    assert!(error.contains("is not a gate stage"), "{error}");
    assert_eq!(
        store.load_task(&task).expect("reads").item.title,
        "Task work",
        "a refused call writes nothing"
    );
}

/// The tool contract calls the original brief the text before the first `## Rework <n>`
/// section. A heading that only starts `## Rework `, such as `## Rework notes`, is brief text,
/// so a new brief replaces it.
#[test]
fn a_brief_heading_that_is_not_a_numbered_rework_is_replaced() {
    let (_scratch, store, plan) = setup("rework-notes");
    let task = add_task(&store, &plan, "work", &[]);
    store
        .change_task(&task, |task| {
            task.body = "the original brief\n\n## Rework notes\n\nkeep both sides\n\n\
                         ## Rework 1\n\nfirst rework\n"
                .to_owned();
            Ok(())
        })
        .expect("the test writes a notes heading above a rework");
    let server = ArcServer::new(store.clone());

    let updated = update_ok(&server, json!({"task": task, "brief": "a better brief"}));
    let body = updated["body"].as_str().expect("a body");
    assert!(!body.contains("the original brief"), "{body}");
    assert!(!body.contains("## Rework notes"), "{body}");
    assert!(!body.contains("keep both sides"), "{body}");
    assert!(
        body.starts_with("a better brief\n\n## Rework 1\n"),
        "{body}"
    );
    assert_eq!(body.matches("## Rework").count(), 1, "{body}");
    let changes_at = body.find("## Changes").expect("the changes heading");
    assert!(
        body.find("## Rework 1").expect("the rework heading") < changes_at,
        "{body}"
    );
    assert!(body.trim_end().ends_with("update_task: brief"), "{body}");
}

#[test]
fn a_rework_added_after_a_change_keeps_the_records_last() {
    let (_scratch, store, plan) = setup("late-rework");
    let task = add_task(&store, &plan, "work", &[]);
    let server = ArcServer::new(store.clone());

    update_ok(&server, json!({"task": task, "title": "First change"}));
    set_state(&store, &task, TaskState::Review, "awaiting_human");
    let reworked = server
        .dispatch(
            "add_rework",
            json!({"task": task, "brief": "do it again"})
                .as_object()
                .cloned(),
        )
        .expect("the rework succeeds");
    let body = reworked["body"].as_str().expect("a body");
    let rework_at = body.find("## Rework 1").expect("the rework heading");
    let changes_at = body.find("## Changes").expect("the changes heading");
    assert!(
        rework_at < changes_at,
        "the rework goes above the records: {body}"
    );
    assert!(
        body.trim_end().ends_with("update_task: title"),
        "the record stays the last line: {body}"
    );

    let updated = update_ok(&server, json!({"task": task, "brief": "a better brief"}));
    let body = updated["body"].as_str().expect("a body");
    assert!(
        body.starts_with("a better brief\n\n## Rework 1\n"),
        "{body}"
    );
    assert_eq!(body.matches("## Changes").count(), 1, "{body}");
    let changes_at = body.find("## Changes").expect("the changes heading");
    let last_rework = body.rfind("## Rework").expect("the rework heading");
    assert!(last_rework < changes_at, "the records stay last: {body}");
    assert_eq!(change_lines(body).len(), 2, "{body}");
    assert!(body.trim_end().ends_with("update_task: brief"), "{body}");
    assert!(
        !store.load_task(&task).expect("reads").changed_outside_arc,
        "the file is the one arc wrote"
    );
}

#[test]
fn an_update_repairs_records_a_later_rework_follows() {
    let (_scratch, store, plan) = setup("repair");
    let task = add_task(&store, &plan, "work", &[]);
    store
        .change_task(&task, |task| {
            task.body = "the brief\n\n## Changes\n\
                         - 2026-01-02T03:04:05Z update_task: title\n\n\
                         ## Rework 1\n\ndo it again\n"
                .to_owned();
            Ok(())
        })
        .expect("the test writes a record above a rework");
    let server = ArcServer::new(store.clone());

    let updated = update_ok(&server, json!({"task": task, "allow_noop": true}));
    let body = updated["body"].as_str().expect("a body");
    assert_eq!(
        body.lines().take(6).collect::<Vec<_>>(),
        vec!["the brief", "", "## Rework 1", "", "do it again", ""],
        "{body}"
    );
    assert_eq!(body.matches("## Changes").count(), 1, "{body}");
    let changes_at = body.find("## Changes").expect("the changes heading");
    assert!(
        body.rfind("## Rework").expect("the rework heading") < changes_at,
        "the whole section moved below the rework: {body}"
    );
    assert_eq!(change_lines(body).len(), 2, "{body}");
    assert_eq!(change_lines(body)[0].1, "title", "{body}");
    assert_eq!(change_lines(body)[1].1, "allow_noop", "{body}");
    assert!(
        body.trim_end().ends_with("update_task: allow_noop"),
        "{body}"
    );
}

#[test]
fn show_reads_the_file_arc_wrote() {
    let (_scratch, store, plan) = setup("show");
    let task = add_task(&store, &plan, "work", &[]);
    let server = ArcServer::new(store.clone());

    update_ok(&server, json!({"task": task, "title": "A better title"}));
    let text = store::cli::show_text(&store, &task).expect("show reads the task");
    assert!(text.contains("changed outside arc: no"), "{text}");
    assert!(text.contains("A better title"), "{text}");
    let json = store::cli::show_json(&store, &task).expect("show --json reads the task");
    assert_eq!(json["changed_outside_arc"], false);
    assert_eq!(json["title"], "A better title");
}

#[test]
fn a_held_state_lock_refuses_the_update() {
    let (_scratch, store, plan) = setup("locked");
    let task = add_task(&store, &plan, "work", &[]);
    let server = ArcServer::new(store.clone());

    let lock = store.lock_state().expect("the test takes the lock");
    let error = update_err(&server, json!({"task": task, "title": "no"}));
    drop(lock);
    assert!(
        error.contains("state.lock is locked by another process"),
        "{error}"
    );
    assert_eq!(
        store.load_task(&task).expect("reads").item.title,
        "Task work"
    );
}

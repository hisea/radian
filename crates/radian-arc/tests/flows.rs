//! `[flows.<name>]` and a task's `flow` field (ADR 0048 decisions 7, 14 and 15).
//!
//! Every test builds a throwaway repository with a throwaway bare `origin` under the system
//! temp directory. The end-to-end tests drive the real `radian` binary: a command flow's
//! script commits its own change, and a `radian exec` flow runs under the mock model.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use radian_arc::mcp::ArcServer;
use radian_arc::store::Store;
use radian_arc::store::config::{self, DEFAULT_FLOW};
use radian_arc::store::plan_ops;
use serde_json::{Value, json};

fn setup(prefix: &str) -> (Repo, Store) {
    let repo = Repo::new(prefix);
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    (repo, store)
}

/// Appends `[flows.<name>]` sections to the repository's `config.toml`.
fn add_flows(repo: &Repo, text: &str) {
    let path = repo.arc().join("config.toml");
    let mut config = fs::read_to_string(&path).expect("config.toml");
    config.push('\n');
    config.push_str(text);
    fs::write(&path, config).expect("write config.toml");
}

/// Replaces one piece of `config.toml`.
fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// Writes an executable shell script and returns its path.
fn script(repo: &Repo, name: &str, body: &str) -> std::path::PathBuf {
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

/// The command worker's body: record that it ran, commit one file, and print a completed
/// `Result`.
fn command_body(marker: &Path) -> String {
    format!(
        "set -e\n\
         touch '{}'\n\
         echo change > flow.txt\n\
         git add flow.txt\n\
         git commit -m change >/dev/null\n\
         printf '{{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"command done\"}}\\n'\n",
        marker.display()
    )
}

/// The command worker's body for the concurrency tests: record that it started, wait until the
/// test writes `<release>/go-<task>`, then commit one file and print a completed `Result`. Two
/// flows share the script; `ARC_TASK` tells their runs apart, so the test decides when each
/// attempt ends (ADR 0048 decision 13).
fn waiting_body(dir: &Path, release: &Path) -> String {
    format!(
        "set -e\n\
         d=\"{dir}\"\n\
         r=\"{release}\"\n\
         touch \"$d/started-$ARC_TASK\"\n\
         while [ ! -f \"$r/go-$ARC_TASK\" ]; do sleep 0.05; done\n\
         echo change > \"change-$ARC_TASK.txt\"\n\
         git add \"change-$ARC_TASK.txt\"\n\
         git commit -m \"change from $ARC_TASK\" >/dev/null\n\
         printf '{{\"type\":\"Result\",\"outcome\":\"completed\",\"text\":\"done\"}}\\n'\n",
        dir = dir.display(),
        release = release.display(),
    )
}

/// Runs `radian arc run` again and again until `done` holds, the way the next passes of a loop
/// would, and returns everything the passes printed.
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
            "the condition did not hold in {limit:?}: {output}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

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

/// Writes the release file a waiting command worker polls for.
fn release(repo: &Repo, task: &str) {
    fs::write(repo.root.join(format!("go-{task}")), "").expect("release the worker");
}

/// Waits for a worker's own end record, so no attempt outlives the test.
fn wait_for_end(repo: &Repo, task: &str) {
    wait_until(Duration::from_secs(90), "the worker's end record", || {
        repo.run_dir(task, 1).join("attempt_finished.json").exists()
    });
}

/// The task's state and phase.
fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

fn start_json(repo: &Repo, task: &str, attempt: u32) -> Value {
    let text =
        fs::read_to_string(repo.attempt_dir(task, attempt).join("start.json")).expect("start.json");
    serde_json::from_str(&text).expect("start.json parses")
}

fn record(dir: &Path, name: &str) -> Value {
    let path = dir.join(name);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A file without `[flows]`: the attempt starts the same `radian exec` worker it always did,
/// and the claim and wrapper records name the `default` flow.
#[test]
fn a_file_without_flows_keeps_the_worker_line_and_names_the_default_flow() {
    let (repo, store) = setup("flows-default");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    tune(
        &repo,
        "exec_args = []",
        &format!(
            "exec_args = [{}]",
            model_args(mock.base_url())
                .iter()
                .map(|arg| format!("\"{arg}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task(
            &plan,
            "default",
            "Default",
            vec![],
            false,
            vec![],
            "brief\n",
        )
        .unwrap();
    plan_ops::confirm(&store, &plan).expect("confirm");

    let run = repo.radian(&["arc", "start", &task]);
    assert_eq!(run.code, 0, "start: {}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        run.stdout
    );
    let expected = json!({"kind": "radian", "flow": DEFAULT_FLOW});
    assert_eq!(start_json(&repo, &task, 1)["worker"], expected);
    assert_eq!(
        record(&repo.attempt_dir(&task, 1), "identity.json")["worker"],
        expected
    );
    assert_eq!(
        record(&repo.run_dir(&task, 1), "execution.json")["worker"],
        expected
    );
    let file = fs::read_to_string(repo.clone.join(format!(".radian/arc/tasks/{task}.md"))).unwrap();
    assert!(!file.contains("flow:"), "the field stays absent:\n{file}");
}

/// Two flows, one command and one `radian exec`, with one task each: every attempt starts its
/// own flow's worker, and the claim and wrapper records name that flow.
#[test]
fn each_task_runs_its_flows_worker_and_the_records_name_the_flow() {
    let (repo, store) = setup("flows-two");
    let marker = repo.root.join("command-ran");
    let worker = script(&repo, "command-worker", &command_body(&marker));
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("radian done"),
    ]);
    add_flows(
        &repo,
        &format!(
            "[flows.codex]\ncommand = [\"{}\"]\n\n[flows.radian]\nexec_args = [{}]\n",
            worker.display(),
            model_args(mock.base_url())
                .iter()
                .map(|arg| format!("\"{arg}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let codex = store
        .add_task_with_flow(
            &plan,
            "codex",
            "Codex",
            vec![],
            false,
            vec![],
            "brief\n",
            Some("codex"),
        )
        .unwrap();
    let radian = store
        .add_task_with_flow(
            &plan,
            "radian",
            "Radian",
            vec![],
            false,
            vec![],
            "brief\n",
            Some("radian"),
        )
        .unwrap();
    plan_ops::confirm(&store, &plan).expect("confirm");

    let run = repo.radian(&["arc", "start", &codex]);
    assert_eq!(run.code, 0, "codex start: {}", run.stderr);
    assert_eq!(
        state(&store, &codex),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        run.stdout
    );
    assert!(marker.exists(), "the codex flow's script ran");
    let expected = json!({
        "kind": "command",
        "flow": "codex",
        "program": worker.display().to_string(),
    });
    assert_eq!(start_json(&repo, &codex, 1)["worker"], expected);
    assert_eq!(
        record(&repo.attempt_dir(&codex, 1), "identity.json")["worker"],
        expected
    );
    assert_eq!(
        record(&repo.run_dir(&codex, 1), "execution.json")["worker"],
        expected
    );
    assert_eq!(
        record(&repo.run_dir(&codex, 1), "execution.json")["session_id"],
        json!(format!("{codex}-1"))
    );

    let run = repo.radian(&["arc", "start", &radian]);
    assert_eq!(run.code, 0, "radian start: {}", run.stderr);
    assert_eq!(
        state(&store, &radian),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        run.stdout
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "the mock model served the radian flow"
    );
    let expected = json!({"kind": "radian", "flow": "radian"});
    assert_eq!(start_json(&repo, &radian, 1)["worker"], expected);
    assert_eq!(
        record(&repo.attempt_dir(&radian, 1), "identity.json")["worker"],
        expected
    );
    assert_eq!(
        record(&repo.run_dir(&radian, 1), "execution.json")["worker"],
        expected
    );

    // `show --json` prints the field, and `list` gives it its own column.
    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &codex, "--json"]).stdout).unwrap();
    assert_eq!(shown["flow"], json!("codex"));
    let listed = repo.radian(&["arc", "list", "--all"]);
    assert!(
        listed
            .stdout
            .contains(&format!("{codex}  codex  review.awaiting_human")),
        "list's flow column: {}",
        listed.stdout
    );
    assert!(
        listed
            .stdout
            .contains(&format!("{radian}  radian  review.awaiting_human")),
        "list's flow column: {}",
        listed.stdout
    );
}

/// A task added without a flow prints `default` in `show --json`, and `list` prints no flow
/// column when the repository defines only the default flow.
#[test]
fn show_prints_the_default_flow_and_list_has_no_column_with_only_default() {
    let (repo, store) = setup("flows-show-default");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief\n")
        .unwrap();
    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &task, "--json"]).stdout).unwrap();
    assert_eq!(shown["flow"], json!(DEFAULT_FLOW));
    let listed = repo.radian(&["arc", "list", "--all"]);
    assert!(
        listed
            .stdout
            .contains(&format!("  {task}  todo.ready  attempt 0  One")),
        "no flow column: {}",
        listed.stdout
    );
}

/// `add_task`, `update_task`, `arc add` and `adopt` refuse a flow the file does not define,
/// and name the flows it does.
#[test]
fn an_unknown_flow_is_refused_by_every_writer() {
    let (repo, store) = setup("flows-unknown");
    add_flows(&repo, "[flows.codex]\nexec_args = []\n");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();

    let error = store
        .add_task_with_flow(
            &plan,
            "one",
            "One",
            vec![],
            false,
            vec![],
            "brief\n",
            Some("nope"),
        )
        .expect_err("add_task refuses an unknown flow");
    for name in ["nope", "codex", DEFAULT_FLOW] {
        assert!(
            error.message().contains(name),
            "{name}: {}",
            error.message()
        );
    }

    let server = ArcServer::new(store.clone());
    let error = server
        .dispatch(
            "add_task",
            json!({"plan": plan, "slug": "two", "title": "Two", "brief": "b", "flow": "nope"})
                .as_object()
                .cloned(),
        )
        .expect_err("the MCP add_task refuses an unknown flow");
    for name in ["nope", "codex", DEFAULT_FLOW] {
        assert!(error.contains(name), "{name}: {error}");
    }

    let task = store
        .add_task(&plan, "three", "Three", vec![], false, vec![], "b")
        .unwrap();
    let error = server
        .dispatch(
            "update_task",
            json!({"task": task, "flow": "nope"}).as_object().cloned(),
        )
        .expect_err("the MCP update_task refuses an unknown flow");
    for name in ["nope", "codex", DEFAULT_FLOW] {
        assert!(error.contains(name), "{name}: {error}");
    }

    let cli = repo.radian(&[
        "arc", "add", "task", "--plan", &plan, "--slug", "four", "--title", "Four", "--flow",
        "nope",
    ]);
    assert_ne!(cli.code, 0, "arc add refuses an unknown flow");
    for name in ["nope", "codex", DEFAULT_FLOW] {
        assert!(cli.stderr.contains(name), "{name}: {}", cli.stderr);
    }

    // A hand edit is refused by `adopt` under the same rule.
    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("---\n", "---\nflow: nope\n")).unwrap();
    let error = store
        .adopt(&task)
        .expect_err("adopt refuses an unknown flow");
    for name in ["nope", "codex", DEFAULT_FLOW] {
        assert!(
            error.message().contains(name),
            "{name}: {}",
            error.message()
        );
    }
}

/// A flow changes only while the task has had no attempt; `update_task` and `adopt` both
/// refuse a later change and say to add a new task.
#[test]
fn a_flow_change_after_the_first_attempt_is_refused() {
    let (repo, store) = setup("flows-attempt");
    add_flows(&repo, "[flows.codex]\nexec_args = []\n");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task_with_flow(
            &plan,
            "one",
            "One",
            vec![],
            false,
            vec![],
            "brief\n",
            Some("codex"),
        )
        .unwrap();
    store
        .change_task(&task, |task| {
            task.attempt = 1;
            task.phase = "ready".to_owned();
            Ok(())
        })
        .expect("the test sets the attempt");

    let server = ArcServer::new(store.clone());
    let error = server
        .dispatch(
            "update_task",
            json!({"task": task, "flow": DEFAULT_FLOW})
                .as_object()
                .cloned(),
        )
        .expect_err("a later flow change is refused");
    assert!(error.contains("add a new task"), "{error}");

    // The same field edited by hand, through `adopt`.
    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let text = fs::read_to_string(&path).unwrap();
    let text = if text.contains("flow: codex") {
        text.replace("flow: codex", "flow: default")
    } else {
        text.replace("---\n", "---\nflow: default\n")
    };
    fs::write(&path, text).unwrap();
    let error = store
        .adopt(&task)
        .expect_err("adopt refuses a later flow change");
    assert!(
        error.message().contains("add a new task"),
        "{}",
        error.message()
    );
}

/// The four config errors a flow can carry are refused when the file is read, each by name.
#[test]
fn a_broken_flow_config_is_refused_by_name() {
    let cases = [
        (
            "schema_version = 1\n[worker]\ncommand = [\"/usr/bin/codex\"]\n[flows.default]\nexec_args = []\n",
            vec!["flows.default", "worker.command"],
        ),
        (
            "schema_version = 1\n[flows.art]\ncommand = [\"/usr/bin/codex\"]\nexec_args = [\"--model\", \"m\"]\n",
            vec!["flows.art.command", "flows.art.exec_args"],
        ),
        (
            "schema_version = 1\n[flows.art]\nconcurrency = 0\n",
            vec!["flows.art.concurrency"],
        ),
        (
            "schema_version = 1\n[flows.art]\ngate_profile = \"missing\"\n",
            vec!["flows.art.gate_profile", "missing"],
        ),
    ];
    for (text, keys) in cases {
        let error = config::parse(text).expect_err("the config is refused");
        for key in keys {
            assert!(error.contains(key), "the error names `{key}`: {error}");
        }
    }
}

/// `adopt` accepts a hand-edited flow that names a defined flow while the task has had no
/// attempt, and reads it back.
#[test]
fn adopt_accepts_a_hand_edited_flow_before_the_first_attempt() {
    let (repo, store) = setup("flows-adopt");
    add_flows(&repo, "[flows.codex]\nexec_args = []\n");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task(&plan, "one", "One", vec![], false, vec![], "brief\n")
        .unwrap();
    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("---\n", "---\nflow: codex\n")).unwrap();
    store
        .adopt(&task)
        .expect("adopt accepts the hand-edited flow");
    assert_eq!(
        store.load_task(&task).unwrap().item.flow.as_deref(),
        Some("codex")
    );
}

/// A task whose flow was removed from `config.toml` is not claimed: it stays `todo.ready`,
/// the loop and `confirm --dry-run` name the task and the missing flow, and `list` and
/// `show` print no slot reason, because a missing flow is not a full limit — not even while
/// `[limits] concurrency` is full from another flow's running task.
#[test]
fn a_task_whose_flow_was_removed_stays_ready_and_says_so() {
    let (repo, store) = setup("flows-removed");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    add_flows(
        &repo,
        &format!(
            "[flows.codex]\nexec_args = []\n\n\
             [flows.spare]\nexec_args = []\n\n\
             [flows.hold]\ncommand = [\"{}\"]\nconcurrency = 1\n",
            worker.display()
        ),
    );
    // One running task of a defined flow fills `[limits] concurrency` (1), so the ordinary
    // limit calculation would name a slot for any other ready task.
    let hold = store
        .add_plan("hold", "Hold", "the holding request")
        .unwrap();
    let holder = add_flow_task(&store, &hold, "hold-one", "hold");
    plan_ops::confirm(&store, &hold).expect("confirm");
    plan_ops::set_auto(&store, &hold, "run").expect("auto run");
    let started = repo.radian(&["arc", "run"]);
    assert_eq!(
        started.code, 0,
        "the holding task starts: {}",
        started.stderr
    );
    assert!(
        started
            .stdout
            .contains(&format!("{holder}-1 started -> doing.running")),
        "the holding task starts: {}",
        started.stdout
    );
    wait_until(
        Duration::from_secs(60),
        "the holding worker to start",
        || repo.root.join(format!("started-{holder}")).exists(),
    );

    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task_with_flow(
            &plan,
            "one",
            "One",
            vec![],
            false,
            vec![],
            "brief\n",
            Some("codex"),
        )
        .unwrap();
    // A ready task whose own flow has room: with `[limits] concurrency` full it names the
    // shared limit, which proves the limit really is full for the assertions below.
    let spare = add_flow_task(&store, &plan, "spare", "spare");
    plan_ops::confirm(&store, &plan).expect("confirm");
    // A proposed task with the flow, added before the section disappears.
    let proposed = store
        .add_task_proposed_with_flow(
            &plan,
            "two",
            "Two",
            vec![],
            false,
            vec![],
            "brief\n",
            Some("codex"),
        )
        .unwrap();
    // Remove the flow the tasks name.
    tune(&repo, "[flows.codex]\nexec_args = []\n", "");

    let run = repo.radian(&["arc", "start", &task]);
    assert_eq!(run.code, 3, "the claim is refused: {}", run.stderr);
    assert!(
        run.stderr.contains(&task) && run.stderr.contains("codex"),
        "the refusal names both: {}",
        run.stderr
    );
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "ready".to_owned()),
        "nothing changed"
    );

    let pass = repo.radian(&["arc", "run"]);
    assert_eq!(pass.code, 0, "the pass itself succeeds: {}", pass.stderr);
    assert!(
        pass.stdout.contains(&task) && pass.stdout.contains("codex"),
        "the loop names the task and the missing flow: {}",
        pass.stdout
    );
    assert!(
        pass.stdout.contains("does not define"),
        "the loop says the flow is missing: {}",
        pass.stdout
    );

    // The queue reason `confirm --dry-run` prints is the same line.
    let loaded = store.load_task(&task).unwrap().item;
    let plan_file = store.load_plan(&plan).unwrap().item;
    let reason = radian_arc::run::waiting_reason(&store, &plan_file, &loaded).unwrap();
    assert!(
        reason.contains(&task) && reason.contains("codex"),
        "{reason}"
    );
    assert!(reason.contains("does not define"), "{reason}");

    // A proposed task with the same removed flow shows the same reason on its queue line.
    let dry = repo.radian(&["arc", "confirm", "--dry-run", &proposed]);
    assert_eq!(dry.code, 0, "dry run: {}", dry.stderr);
    assert!(
        dry.stdout.contains(&proposed) && dry.stdout.contains("codex"),
        "the queue line names the task and the missing flow: {}",
        dry.stdout
    );
    assert!(
        dry.stdout.contains("does not define"),
        "the queue reason says the flow is missing: {}",
        dry.stdout
    );

    // `list`'s task line and `show`'s `queue_reason` name a slot only when a full limit holds
    // the task (ADR 0048 decision 13): a missing flow is not a limit, so neither prints one.
    // The plan runs, and the holding task keeps `[limits] concurrency` full, so the ordinary
    // calculation would name the shared limit for any other ready task.
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    let list = repo.radian(&["arc", "list"]);
    assert_eq!(list.code, 0, "list: {}", list.stderr);
    let spare_line = list
        .stdout
        .lines()
        .find(|line| line.contains(spare.as_str()))
        .unwrap_or_else(|| panic!("`{spare}` is listed: {}", list.stdout));
    assert!(
        spare_line.contains("(waiting for a free concurrency slot)"),
        "the shared limit is full: {}",
        list.stdout
    );
    let line = list
        .stdout
        .lines()
        .find(|line| line.contains(task.as_str()))
        .unwrap_or_else(|| panic!("`{task}` is listed: {}", list.stdout));
    assert!(
        !line.contains("waiting for a free"),
        "a missing flow is not a full limit: {}",
        list.stdout
    );
    let show = repo.radian(&["arc", "show", &task]);
    assert_eq!(show.code, 0, "show: {}", show.stderr);
    assert!(
        show.stdout.contains("queue_reason: -"),
        "`show` names no slot: {}",
        show.stdout
    );
    assert!(
        !show.stdout.contains("waiting for a free"),
        "`show` names no slot: {}",
        show.stdout
    );

    release(&repo, &holder);
    wait_for_end(&repo, &holder);
}

/// Adds an `image` and a `code` flow, each running the same waiting command worker.
fn two_flows(repo: &Repo, worker: &Path, image: u32, code: u32) {
    add_flows(
        repo,
        &format!(
            "[flows.image]\ncommand = [\"{}\"]\nconcurrency = {image}\n\n\
             [flows.code]\ncommand = [\"{}\"]\nconcurrency = {code}\n",
            worker.display(),
            worker.display()
        ),
    );
}

/// Adds one ready task of `flow` to `plan`.
fn add_flow_task(store: &Store, plan: &str, slug: &str, flow: &str) -> String {
    store
        .add_task_with_flow(
            plan,
            slug,
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
            Some(flow),
        )
        .expect("add a task")
}

/// Two flows at `concurrency` 1 and `[limits] concurrency` 2: one pass claims one `image` task
/// and the first `code` task in the same pass, leaves the second `code` task ready with the
/// flow's reason (both limits are full, and the flow's reason wins), and starts it once the
/// first `code` worker leaves `doing`. Mutations: count every `doing` task against the flow
/// (the second `code` task's reason is not the flow's), `break` at the full flow (the second
/// `code` task never starts), or drop the total check (the pass claims three tasks).
#[test]
fn a_full_flow_skips_its_later_task_with_the_flows_reason() {
    let (repo, store) = setup("flows-concurrency-two");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    tune(&repo, "concurrency = 1", "concurrency = 2");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let image = add_flow_task(&store, &plan, "image", "image");
    let first = add_flow_task(&store, &plan, "code-one", "code");
    let second = add_flow_task(&store, &plan, "code-two", "code");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    for task in [&image, &first] {
        assert!(
            run.stdout
                .contains(&format!("{task}-1 started -> doing.running")),
            "{}",
            run.stdout
        );
    }
    assert!(
        !run.stdout.contains(&format!("{second}-1 started")),
        "the code flow already had a task in `doing`: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!(
            "{second} todo.ready: waiting for a free code slot"
        )),
        "the flow's reason wins when both limits are full: {}",
        run.stdout
    );
    assert_eq!(
        state(&store, &second),
        ("todo".to_owned(), "ready".to_owned())
    );
    wait_until(Duration::from_secs(60), "the two workers to start", || {
        repo.root.join(format!("started-{image}")).exists()
            && repo.root.join(format!("started-{first}")).exists()
    });

    // The first code worker ends; the code flow has room again, and the next pass claims the
    // second code task while the image task still holds the image flow's only slot.
    release(&repo, &first);
    let passes = run_until(&repo, Duration::from_secs(120), || {
        state(&store, &second).0 == "doing"
    });
    assert!(
        passes.contains(&format!("{second}-1 started -> doing.running")),
        "{passes}"
    );

    for task in [&image, &second] {
        release(&repo, task);
        wait_for_end(&repo, task);
    }
}

/// The same flows with `[limits] concurrency` 1: one pass starts only the first task in claim
/// order (a `code` one here), the `image` task names the shared limit because its own flow has
/// room, and the second `code` task names its flow's limit because that flow is full.
#[test]
fn one_total_slot_names_the_shared_limit_or_the_full_flow() {
    let (repo, store) = setup("flows-concurrency-total");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let first = add_flow_task(&store, &plan, "code-one", "code");
    let image = add_flow_task(&store, &plan, "image", "image");
    let second = add_flow_task(&store, &plan, "code-two", "code");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("{first}-1 started -> doing.running")),
        "{}",
        run.stdout
    );
    for task in [&image, &second] {
        assert!(
            !run.stdout.contains(&format!("{task}-1 started")),
            "one total slot starts one task: {}",
            run.stdout
        );
    }
    assert!(
        run.stdout.contains(&format!(
            "{image} todo.ready: waiting for a free concurrency slot"
        )),
        "the image flow has room, so the shared total is the reason: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!(
            "{second} todo.ready: waiting for a free code slot"
        )),
        "the code flow is full, so its reason wins: {}",
        run.stdout
    );

    release(&repo, &first);
    wait_for_end(&repo, &first);
}

/// A full flow's ready task earlier in claim order does not stop a task of another flow further
/// down the order: with `[limits] concurrency` raised to 2, the `image` flow still holds its
/// only slot, and the later `code` task is claimed in the same pass while the second `image`
/// task stays ready with the flow's reason. Mutation: `break` at the full flow.
#[test]
fn a_full_flow_does_not_block_another_flows_later_task() {
    let (repo, store) = setup("flows-concurrency-skip");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let image_one = add_flow_task(&store, &plan, "image-one", "image");
    let image_two = add_flow_task(&store, &plan, "image-two", "image");
    let code = add_flow_task(&store, &plan, "code", "code");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("{image_one}-1 started -> doing.running")),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(&format!("{code}-1 started")),
        "{}",
        run.stdout
    );
    wait_until(Duration::from_secs(60), "the image worker to start", || {
        repo.root.join(format!("started-{image_one}")).exists()
    });

    tune(&repo, "concurrency = 1", "concurrency = 2");
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("{code}-1 started -> doing.running")),
        "the full image flow must not stop the later code task: {}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(&format!("{image_two}-1 started")),
        "the image flow is full: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!(
            "{image_two} todo.ready: waiting for a free image slot"
        )),
        "{}",
        run.stdout
    );

    for task in [&image_one, &code] {
        release(&repo, task);
        wait_for_end(&repo, task);
    }
}

/// The owner's `start` obeys the flow limit too: with the `code` flow full and the repository
/// total free, it refuses the second `code` task with exit `3` and names the flow's limit and
/// its count. Mutation: drop the flow check (the claim succeeds).
#[test]
fn start_refuses_a_full_flows_task_naming_the_flow_limit() {
    let (repo, store) = setup("flows-concurrency-start");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    tune(&repo, "concurrency = 1", "concurrency = 2");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let first = add_flow_task(&store, &plan, "code-one", "code");
    let second = add_flow_task(&store, &plan, "code-two", "code");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("{first}-1 started -> doing.running")),
        "{}",
        run.stdout
    );
    wait_until(Duration::from_secs(60), "the code worker to start", || {
        repo.root.join(format!("started-{first}")).exists()
    });

    let refused = repo.radian(&["arc", "start", &second]);
    assert_eq!(refused.code, 3, "start: {}", refused.stderr);
    assert!(
        refused.stderr.contains("flows.code.concurrency"),
        "the refusal names the flow's limit: {}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("1 task(s)"),
        "the refusal names its count: {}",
        refused.stderr
    );
    assert_eq!(
        state(&store, &second),
        ("todo".to_owned(), "ready".to_owned()),
        "the refused claim writes nothing"
    );
    assert_eq!(
        store
            .load_task(&second)
            .expect("the task reads")
            .item
            .attempt,
        0
    );

    release(&repo, &first);
    wait_for_end(&repo, &first);
}

/// `confirm --dry-run` checks the same two limits: a proposed `code` task whose flow is full
/// gets `waiting for a free code slot` on its queue line.
#[test]
fn a_confirm_dry_run_names_a_full_flows_queue_reason() {
    let (repo, store) = setup("flows-concurrency-dry");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    tune(&repo, "concurrency = 1", "concurrency = 2");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let first = add_flow_task(&store, &plan, "code-one", "code");
    let proposed = add_flow_task(&store, &plan, "code-two", "code");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    store
        .change_task(&proposed, |task| {
            task.phase = "proposed".to_owned();
            Ok(())
        })
        .expect("the task waits for confirm");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("{first}-1 started -> doing.running")),
        "{}",
        run.stdout
    );
    wait_until(Duration::from_secs(60), "the code worker to start", || {
        repo.root.join(format!("started-{first}")).exists()
    });

    let dry = repo.radian(&["arc", "confirm", "--dry-run", &proposed]);
    assert_eq!(dry.code, 0, "dry run: {}", dry.stderr);
    assert!(
        dry.stdout.contains("waiting for a free code slot"),
        "the dry run uses the flow's reason: {}",
        dry.stdout
    );

    release(&repo, &first);
    wait_for_end(&repo, &first);
}

/// `list` and `show` name the flow that holds a ready task, in the loop summary's words:
/// `waiting for a free code slot`, in the task line, the `queue_reason` field and both JSON
/// documents. With both limits full in claim order, the flow's reason wins.
#[test]
fn list_and_show_name_a_full_flows_queue_reason() {
    let (repo, store) = setup("flows-concurrency-list");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    tune(&repo, "concurrency = 1", "concurrency = 2");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let first = add_flow_task(&store, &plan, "code-one", "code");
    let second = add_flow_task(&store, &plan, "code-two", "code");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    // Nothing is `doing`, so both limits have room and neither command names one.
    let list = repo.radian(&["arc", "list"]);
    assert_eq!(list.code, 0, "list: {}", list.stderr);
    assert!(
        !list.stdout.contains("waiting for a free"),
        "no slot is full: {}",
        list.stdout
    );
    let show = repo.radian(&["arc", "show", &second]);
    assert_eq!(show.code, 0, "show: {}", show.stderr);
    assert!(
        show.stdout.contains("queue_reason: -"),
        "no slot holds the task: {}",
        show.stdout
    );

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    wait_until(Duration::from_secs(60), "the code worker to start", || {
        repo.root.join(format!("started-{first}")).exists()
    });

    let list = repo.radian(&["arc", "list"]);
    assert_eq!(list.code, 0, "list: {}", list.stderr);
    let line = list
        .stdout
        .lines()
        .find(|line| line.contains(&second))
        .unwrap_or_else(|| panic!("`{second}` is listed: {}", list.stdout));
    assert!(
        line.contains("(waiting for a free code slot)"),
        "the flow's reason wins when both limits are full: {}",
        list.stdout
    );
    let first_line = list
        .stdout
        .lines()
        .find(|line| line.contains(&first))
        .expect("the running task is listed");
    assert!(
        !first_line.contains("waiting for a free"),
        "a `doing` task waits for no slot: {}",
        list.stdout
    );

    let list_json = repo.radian(&["arc", "list", "--json"]);
    assert_eq!(list_json.code, 0, "list --json: {}", list_json.stderr);
    let value: Value = serde_json::from_str(&list_json.stdout).expect("list --json parses");
    let task = value[0]["tasks"]
        .as_array()
        .expect("the plan's tasks")
        .iter()
        .find(|task| task["id"] == json!(second))
        .expect("the waiting task is in list --json");
    assert_eq!(
        task["queue_reason"],
        json!("waiting for a free code slot"),
        "{value}"
    );

    let show = repo.radian(&["arc", "show", &second]);
    assert_eq!(show.code, 0, "show: {}", show.stderr);
    assert!(
        show.stdout
            .contains("queue_reason: waiting for a free code slot"),
        "{}",
        show.stdout
    );
    let show_json = repo.radian(&["arc", "show", &second, "--json"]);
    assert_eq!(show_json.code, 0, "show --json: {}", show_json.stderr);
    let value: Value = serde_json::from_str(&show_json.stdout).expect("show --json parses");
    assert_eq!(
        value["queue_reason"],
        json!("waiting for a free code slot"),
        "{value}"
    );

    release(&repo, &first);
    wait_for_end(&repo, &first);
}

/// The shared limit keeps its own words in `list` and `show`: a ready `image` task whose flow
/// has room waits for `waiting for a free concurrency slot` while `[limits] concurrency` is
/// full.
#[test]
fn list_and_show_name_the_shared_limit_when_the_flow_has_room() {
    let (repo, store) = setup("flows-concurrency-list-total");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let first = add_flow_task(&store, &plan, "code", "code");
    let image = add_flow_task(&store, &plan, "image", "image");
    plan_ops::confirm(&store, &plan).expect("confirm");
    plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    wait_until(Duration::from_secs(60), "the code worker to start", || {
        repo.root.join(format!("started-{first}")).exists()
    });

    let list = repo.radian(&["arc", "list"]);
    assert_eq!(list.code, 0, "list: {}", list.stderr);
    let line = list
        .stdout
        .lines()
        .find(|line| line.contains(&image))
        .unwrap_or_else(|| panic!("`{image}` is listed: {}", list.stdout));
    assert!(
        line.contains("(waiting for a free concurrency slot)"),
        "the image flow has room, so the shared total is the reason: {}",
        list.stdout
    );
    let list_json = repo.radian(&["arc", "list", "--json"]);
    let value: Value = serde_json::from_str(&list_json.stdout).expect("list --json parses");
    let task = value[0]["tasks"]
        .as_array()
        .expect("the plan's tasks")
        .iter()
        .find(|task| task["id"] == json!(image))
        .expect("the waiting task is in list --json");
    assert_eq!(
        task["queue_reason"],
        json!("waiting for a free concurrency slot"),
        "{value}"
    );

    let show = repo.radian(&["arc", "show", &image]);
    assert_eq!(show.code, 0, "show: {}", show.stderr);
    assert!(
        show.stdout
            .contains("queue_reason: waiting for a free concurrency slot"),
        "{}",
        show.stdout
    );
    let show_json = repo.radian(&["arc", "show", &image, "--json"]);
    let value: Value = serde_json::from_str(&show_json.stdout).expect("show --json parses");
    assert_eq!(
        value["queue_reason"],
        json!("waiting for a free concurrency slot"),
        "{value}"
    );

    release(&repo, &first);
    wait_for_end(&repo, &first);
}

/// A file changed outside arc is refused by the scheduler whatever the limits say, so neither
/// `list` nor `show` names a slot that is not what holds the task: a ready task whose own file
/// changed, and a ready task whose plan changed, get no slot reason and a `null`
/// `queue_reason`, while the same state without the change names the full flow.
#[test]
fn a_changed_outside_arc_file_names_no_slot() {
    let (repo, store) = setup("flows-concurrency-changed");
    let worker = script(
        &repo,
        "waiting-worker",
        &waiting_body(&repo.root, &repo.root),
    );
    two_flows(&repo, &worker, 1, 1);
    tune(&repo, "concurrency = 1", "concurrency = 2");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let first = add_flow_task(&store, &plan, "code-one", "code");
    let second = add_flow_task(&store, &plan, "code-two", "code");
    // A second plan, so the plan's own changed marker can be the reason on its own.
    let other = store
        .add_plan("other", "Other", "the other request")
        .unwrap();
    let third = add_flow_task(&store, &other, "code-three", "code");
    for id in [&plan, &other] {
        plan_ops::confirm(&store, id).expect("confirm");
        plan_ops::set_auto(&store, id, "run").expect("auto run");
    }

    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    wait_until(Duration::from_secs(60), "the code worker to start", || {
        repo.root.join(format!("started-{first}")).exists()
    });

    // Before any file changes outside arc, `code` is full and both later tasks say so.
    let list = repo.radian(&["arc", "list"]);
    assert_eq!(list.code, 0, "list: {}", list.stderr);
    for id in [&second, &third] {
        let line = list
            .stdout
            .lines()
            .find(|line| line.contains(id.as_str()))
            .unwrap_or_else(|| panic!("`{id}` is listed: {}", list.stdout));
        assert!(
            line.contains("(waiting for a free code slot)"),
            "`{id}` waits for the full flow: {}",
            list.stdout
        );
    }

    // Edit the task file and the other plan file outside arc.
    let task_path = repo.arc().join("tasks").join(format!("{second}.md"));
    let text = fs::read_to_string(&task_path).expect("the task reads");
    fs::write(&task_path, text.replacen("A task", "An edited task", 1)).expect("edit the task");
    let plan_path = repo.arc().join("plans").join(format!("{other}.md"));
    let text = fs::read_to_string(&plan_path).expect("the plan reads");
    fs::write(
        &plan_path,
        text.replacen("the other request", "an edited request", 1),
    )
    .expect("edit the plan");
    assert!(
        store.load_task(&second).expect("reads").changed_outside_arc,
        "the task edit is seen"
    );
    assert!(
        store.load_plan(&other).expect("reads").changed_outside_arc,
        "the plan edit is seen"
    );

    let list = repo.radian(&["arc", "list"]);
    assert_eq!(list.code, 0, "list: {}", list.stderr);
    for id in [&second, &third] {
        let line = list
            .stdout
            .lines()
            .find(|line| line.contains(id.as_str()))
            .unwrap_or_else(|| panic!("`{id}` is listed: {}", list.stdout));
        assert!(
            !line.contains("waiting for a free"),
            "`{id}` changed outside arc, so no slot holds it: {}",
            list.stdout
        );
    }
    assert!(
        list.stdout.contains("(changed outside arc)"),
        "the changed marker is what holds them: {}",
        list.stdout
    );

    let list_json = repo.radian(&["arc", "list", "--json"]);
    assert_eq!(list_json.code, 0, "list --json: {}", list_json.stderr);
    let value: Value = serde_json::from_str(&list_json.stdout).expect("list --json parses");
    let tasks: Vec<&Value> = value
        .as_array()
        .expect("the plans")
        .iter()
        .flat_map(|plan| plan["tasks"].as_array().expect("the plan's tasks"))
        .collect();
    for id in [&second, &third] {
        let task = tasks
            .iter()
            .find(|task| task["id"] == json!(id.as_str()))
            .unwrap_or_else(|| panic!("`{id}` is in list --json: {value}"));
        assert_eq!(task["queue_reason"], Value::Null, "{value}");
    }

    for id in [&second, &third] {
        let show = repo.radian(&["arc", "show", id.as_str()]);
        assert_eq!(show.code, 0, "show: {}", show.stderr);
        assert!(
            show.stdout.contains("queue_reason: -"),
            "`{id}` names no slot: {}",
            show.stdout
        );
        assert!(
            !show.stdout.contains("waiting for a free"),
            "`{id}` names no slot: {}",
            show.stdout
        );
        let show_json = repo.radian(&["arc", "show", id.as_str(), "--json"]);
        assert_eq!(show_json.code, 0, "show --json: {}", show_json.stderr);
        let value: Value = serde_json::from_str(&show_json.stdout).expect("show --json parses");
        assert_eq!(value["queue_reason"], Value::Null, "{value}");
    }

    release(&repo, &first);
    wait_for_end(&repo, &first);
}

/// The manual names every new config key with the code's default, and the task table names
/// `flow`.
#[test]
fn the_manual_names_the_flow_keys_and_the_task_field() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let page = std::fs::read_to_string(manifest.join("../../docs/user/arc/files.md"))
        .expect("docs/user/arc/files.md");
    let table = radian_coding::drift::table_under(&page, "arc/files.md", "## `config.toml`");
    let row = |key: &str| {
        table
            .iter()
            .find(|row| row.first().is_some_and(|cell| cell == &format!("`{key}`")))
            .unwrap_or_else(|| panic!("the config table has no `{key}` row: {table:?}"))
    };
    let default_cell = |key: &str| row(key).get(2).expect("a Default cell").clone();
    assert_eq!(default_cell("flows.<name>.exec_args"), "`[]`");
    assert!(default_cell("flows.<name>.command").contains("not set"));
    assert_eq!(default_cell("flows.<name>.concurrency"), "`1`");
    assert!(default_cell("flows.<name>.gate_profile").contains("\"default\""));

    let task_fields = radian_coding::drift::table_under(&page, "arc/files.md", "### Task fields");
    assert!(
        task_fields
            .iter()
            .any(|row| row.first().is_some_and(|cell| cell == "`flow`")),
        "the task fields table has no `flow` row: {task_fields:?}"
    );
    assert_eq!(default_cell("flows.<name>.allow_noop"), "`false`");
    let allow_noop = task_fields
        .iter()
        .find(|row| row.first().is_some_and(|cell| cell == "`allow_noop`"))
        .unwrap_or_else(|| {
            panic!("the task fields table has no `allow_noop` row: {task_fields:?}")
        });
    assert!(
        allow_noop
            .iter()
            .any(|cell| cell.contains("flows.<name>.allow_noop")),
        "the task field's row names the flow's value it falls back to: {allow_noop:?}"
    );
    let index = std::fs::read_to_string(manifest.join("../../docs/user/arc/index.md"))
        .expect("docs/user/arc/index.md");
    let source_line = index
        .lines()
        .find(|line| line.contains("`allow_noop_source` is"))
        .expect("arc/index.md explains `allow_noop_source`");
    for source in radian_arc::store::model::ALLOW_NOOP_SOURCES {
        assert!(
            source_line.contains(&format!("`{source}`")),
            "arc/index.md names the source `{source}`: {source_line}"
        );
    }
}

/// `model_args` as the inside of a TOML array of strings.
fn toml_strings(args: &[String]) -> String {
    args.iter()
        .map(|arg| format!("\"{arg}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A task without its own `allow_noop` takes its flow's `true`: an attempt that adds no commit
/// passes the delivery check, reaches `review.awaiting_human`, and the task file stays without
/// the field (ADR 0048 decisions 8 and 15).
///
/// Mutation TK0168-M1: the task's unset value reads as `false` instead of the flow's.
#[test]
fn a_flow_allow_noop_lets_a_task_without_the_field_deliver_nothing() {
    let (repo, store) = setup("flows-noop-flow");
    let mock = Mock::start(vec![text_reply("nothing to do")]);
    add_flows(
        &repo,
        &format!(
            "[flows.quiet]\nexec_args = [{}]\nallow_noop = true\n",
            toml_strings(&model_args(mock.base_url()))
        ),
    );
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task_opt(
            &plan,
            "quiet",
            "Quiet",
            vec![],
            None,
            vec![],
            "brief\n",
            Some("quiet"),
        )
        .unwrap();
    plan_ops::confirm(&store, &plan).expect("confirm");

    let run = repo.radian(&["arc", "start", &task]);
    assert_eq!(run.code, 0, "start: {}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        run.stdout
    );
    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let file = fs::read_to_string(&path).unwrap();
    assert!(
        !file.contains("allow_noop:"),
        "the field stays absent:\n{file}"
    );

    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &task, "--json"]).stdout).unwrap();
    assert_eq!(shown["allow_noop"], json!(true), "{shown}");
    assert_eq!(shown["allow_noop_source"], json!("flow"), "{shown}");
    let text = repo.radian(&["arc", "show", &task]);
    assert!(text.stdout.contains("allow_noop: true"), "{}", text.stdout);
    assert!(
        text.stdout.contains("allow_noop_source: flow"),
        "{}",
        text.stdout
    );
}

/// The same permissive flow with a task that sets `allow_noop: false`: the task's own value
/// wins, so an empty attempt fails the delivery check and waits for rework.
///
/// Mutation TK0168-M2: the flow's value overrides an explicit task `false`.
#[test]
fn an_explicit_task_false_beats_a_permissive_flow() {
    let (repo, store) = setup("flows-noop-explicit");
    let mock = Mock::start(vec![text_reply("nothing to do")]);
    add_flows(
        &repo,
        &format!(
            "[flows.quiet]\nexec_args = [{}]\nallow_noop = true\n",
            toml_strings(&model_args(mock.base_url()))
        ),
    );
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task_opt(
            &plan,
            "loud",
            "Loud",
            vec![],
            Some(false),
            vec![],
            "brief\n",
            Some("quiet"),
        )
        .unwrap();
    plan_ops::confirm(&store, &plan).expect("confirm");

    let run = repo.radian(&["arc", "start", &task]);
    assert_eq!(run.code, 0, "start: {}", run.stderr);
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "rework_pending".to_owned()),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("no change"), "{}", run.stdout);
    let result = record(&repo.run_dir(&task, 1), "attempt_result.json");
    assert_eq!(result["class"], json!("verification"), "{result}");

    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let file = fs::read_to_string(&path).unwrap();
    assert!(file.contains("allow_noop: false"), "{file}");
    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &task, "--json"]).stdout).unwrap();
    assert_eq!(shown["allow_noop"], json!(false), "{shown}");
    assert_eq!(shown["allow_noop_source"], json!("task"), "{shown}");
}

/// A task file an earlier arc wrote carries `allow_noop: false`: it reads as the task's own
/// explicit `false`, not as "not set", and a later write keeps the line (ADR 0048 decision 5).
#[test]
fn an_old_task_file_keeps_its_explicit_false() {
    let (repo, store) = setup("flows-noop-old");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let task = store
        .add_task_opt(&plan, "old", "Old", vec![], None, vec![], "brief\n", None)
        .unwrap();
    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let file = fs::read_to_string(&path).unwrap();
    assert!(!file.contains("allow_noop:"), "{file}");
    // What an older build wrote: the field present and `false`.
    let old = file.replace("---\n", "---\nallow_noop: false\n");
    fs::write(&path, &old).unwrap();
    store.adopt(&task).expect("adopt the old file");

    let loaded = store.load_task(&task).unwrap().item;
    assert_eq!(loaded.allow_noop, Some(false), "the field stays explicit");
    assert_eq!(loaded.allow_noop_source(), "task");
    plan_ops::update_task(&store, &task, Some("Old, renamed"), None, None, None, None)
        .expect("update the title");
    let rewritten = fs::read_to_string(&path).unwrap();
    assert!(
        rewritten.contains("allow_noop: false"),
        "a later write keeps the explicit false:\n{rewritten}"
    );
}

/// The lead's MCP `add_task` without `allow_noop` writes no field, so the task uses its flow's
/// value (ADR 0048 decisions 8, 11 and 15).
#[test]
fn the_mcp_add_task_without_allow_noop_writes_no_field() {
    let (repo, store) = setup("flows-noop-mcp");
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let server = ArcServer::new(store.clone());
    let added = server
        .dispatch(
            "add_task",
            json!({"plan": plan, "slug": "quiet", "title": "Quiet", "brief": "b"})
                .as_object()
                .cloned(),
        )
        .expect("add_task");
    let task = added["id"].as_str().expect("an id").to_owned();
    let path = repo.clone.join(format!(".radian/arc/tasks/{task}.md"));
    let file = fs::read_to_string(&path).unwrap();
    assert!(
        !file.contains("allow_noop:"),
        "the argument is absent, so the field is:\n{file}"
    );
    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &task, "--json"]).stdout).unwrap();
    assert_eq!(shown["allow_noop"], json!(false), "{shown}");
    assert_eq!(shown["allow_noop_source"], json!("flow"), "{shown}");
}

/// A repository without `[flows]` keeps today's behavior: the implicit `default` flow's
/// `allow_noop` is `false`, a task without the field uses it, and `add_task` still writes the
/// explicit value a caller gives (ADR 0048 decisions 8 and 15).
#[test]
fn a_repository_without_flows_uses_the_default_flows_false() {
    let (repo, store) = setup("flows-noop-none");
    assert!(!store.config().unwrap().flows_configured);
    let plan = store.add_plan("plan", "Plan", "request").unwrap();
    let inherited = store
        .add_task_opt(
            &plan,
            "inherited",
            "Inherited",
            vec![],
            None,
            vec![],
            "brief\n",
            None,
        )
        .unwrap();
    let explicit = store
        .add_task(
            &plan,
            "explicit",
            "Explicit",
            vec![],
            true,
            vec![],
            "brief\n",
        )
        .unwrap();
    plan_ops::confirm(&store, &plan).expect("confirm");
    let claim = radian_arc::store::attempt_ops::claim(&store, &inherited).expect("the claim");
    assert!(
        !claim.allow_noop,
        "the default flow's allow_noop is false, as before"
    );

    let path = repo.clone.join(format!(".radian/arc/tasks/{inherited}.md"));
    assert!(!fs::read_to_string(&path).unwrap().contains("allow_noop:"));
    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &inherited, "--json"]).stdout).unwrap();
    assert_eq!(shown["allow_noop"], json!(false), "{shown}");
    assert_eq!(shown["allow_noop_source"], json!("flow"), "{shown}");

    let explicit_file = repo.clone.join(format!(".radian/arc/tasks/{explicit}.md"));
    assert!(
        fs::read_to_string(&explicit_file)
            .unwrap()
            .contains("allow_noop: true")
    );
    let shown: Value =
        serde_json::from_str(&repo.radian(&["arc", "show", &explicit, "--json"]).stdout).unwrap();
    assert_eq!(shown["allow_noop"], json!(true), "{shown}");
    assert_eq!(shown["allow_noop_source"], json!("task"), "{shown}");
}

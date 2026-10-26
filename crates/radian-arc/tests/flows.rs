//! `[flows.<name>]` and a task's `flow` field (ADR 0048 decisions 7, 14 and 15).
//!
//! Every test builds a throwaway repository with a throwaway bare `origin` under the system
//! temp directory. The end-to-end tests drive the real `radian` binary: a command flow's
//! script commits its own change, and a `radian exec` flow runs under the mock model.

mod common;

use std::fs;
use std::path::Path;

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
/// the loop names the task and the missing flow, and so does its queue reason.
#[test]
fn a_task_whose_flow_was_removed_stays_ready_and_says_so() {
    let (repo, store) = setup("flows-removed");
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
}

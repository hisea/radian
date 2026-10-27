//! A flow's `acp`: the wrapper is an ACP client and starts the flow's agent as the worker
//! (ADR 0048 decision 7).
//!
//! Every test configures a small fake agent (a Python script in `tests/common/`) as a flow's
//! `acp` and drives the real `radian arc start`, so the claim, the client, the permission
//! policy, the `stopReason` mapping, cancel and the records are exercised end to end.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use radian_arc::store::{Store, plan_ops};
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

fn add_task(store: &Store, plan: &str, slug: &str, flow: Option<&str>) -> String {
    store
        .add_task_opt(
            plan,
            slug,
            "A task",
            Vec::new(),
            Some(false),
            Vec::new(),
            "the brief\n",
            flow,
        )
        .expect("add a task")
}

/// Replaces one piece of `config.toml`.
fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// The fake agent, written next to the clone and made executable.
fn fake_agent(repo: &Repo) -> PathBuf {
    let path = repo.root.join("fake-acp-agent.py");
    fs::write(&path, include_str!("common/fake_acp_agent.py")).expect("the fake agent");
    make_executable(&path);
    path
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).expect("the file").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make it executable");
    }
}

fn state_dir(repo: &Repo) -> PathBuf {
    repo.root.join("acp-state")
}

/// A TOML array of strings.
fn toml_list(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Writes a `[flows.<name>]` section with `acp` before `[ids]`.
fn set_flow_acp(repo: &Repo, flow: &str, agent: &Path, extra: &[&str]) {
    let mut words = vec![
        agent.to_str().expect("a path").to_owned(),
        "--state".to_owned(),
        state_dir(repo).to_str().expect("a path").to_owned(),
    ];
    words.extend(extra.iter().map(|word| (*word).to_owned()));
    let words = words.iter().map(String::as_str).collect::<Vec<_>>();
    tune(
        repo,
        "[ids]\n",
        &format!("[flows.{flow}]\nacp = [{}]\n\n[ids]\n", toml_list(&words)),
    );
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path)).expect("JSON")
}

fn finished(repo: &Repo, task: &str) -> Value {
    read_json(&repo.run_dir(task, 1).join("attempt_finished.json"))
}

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

/// Waits until `path` exists, failing after `limit`.
fn wait_for(path: &Path, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The attempt's `result.jsonl`, one parsed line each.
fn result_lines(repo: &Repo, task: &str) -> Vec<Value> {
    read(&repo.run_dir(task, 1).join("result.jsonl"))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a result line"))
        .collect()
}

/// An ACP `session/update`'s update object, when the line carries one.
fn update_of(line: &Value) -> Option<&Value> {
    (line.get("type").and_then(Value::as_str) == Some("acp"))
        .then(|| line.pointer("/message/params/update"))
        .flatten()
}

/// `end_turn` with a file written and `commit_dirty = true`: the attempt passes, the branch has
/// the commit, `result.jsonl` holds the updates and both permission pairs, and `start.json`
/// names the ACP worker (ADR 0048 decision 7).
///
/// Mutation TK0202-M3: `end_turn` maps to a failed outcome → this fails.
#[test]
fn an_acp_turn_that_ends_with_end_turn_passes_and_commits() {
    let repo = Repo::new("acp-pass");
    let store = setup(&repo);
    let plan = add_plan(&store, "acp");
    let task = add_task(&store, &plan, "pass", None);
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let agent = fake_agent(&repo);
    set_flow_acp(&repo, "default", &agent, &["--write-file", "image.txt"]);
    tune(
        &repo,
        "[flows.default]\nacp = [",
        "[flows.default]\ncommit_dirty = true\nacp = [",
    );

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished = finished(&repo, &task);
    assert_eq!(finished["pushed"], json!(true), "{finished}");
    assert!(finished["failure"].is_null(), "{finished}");
    assert_eq!(
        state(&store, &task),
        ("review".to_owned(), "awaiting_human".to_owned()),
        "{}",
        run.stdout
    );

    // The claim's record and the wrapper's records name the ACP worker and its program.
    let start = read_json(&repo.attempt_dir(&task, 1).join("start.json"));
    assert_eq!(
        start["worker"],
        json!({
            "kind": "acp",
            "flow": "default",
            "program": agent.to_str().unwrap(),
        }),
        "{start}"
    );
    let execution = read_json(&repo.run_dir(&task, 1).join("execution.json"));
    assert_eq!(execution["worker"], start["worker"], "{execution}");

    // The branch has the wrapper's commit, with the last message chunk as its subject.
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s", &format!("work/{task}")])
            .trim(),
        "the image is ready"
    );

    // Every `session/update` is one line, and the two permission requests are each followed by
    // their answer: inside the worktree allow once, outside reject once.
    let lines = result_lines(&repo, &task);
    let updates = lines
        .iter()
        .filter_map(update_of)
        .map(|update| update["sessionUpdate"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    for expected in ["tool_call", "tool_call_update", "agent_message_chunk"] {
        assert!(
            updates.contains(&expected),
            "no `{expected}` update: {updates:?}"
        );
    }
    let requests = lines
        .iter()
        .filter(|line| {
            line.pointer("/message/method").and_then(Value::as_str)
                == Some("session/request_permission")
        })
        .count();
    assert_eq!(requests, 2, "{lines:#?}");
    let answers = lines
        .iter()
        .filter_map(|line| line.get("answer"))
        .map(|answer| {
            (
                answer["requestId"].as_u64(),
                answer["kind"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        answers.contains(&(Some(1100), "allow_once".to_owned())),
        "{answers:?}"
    );
    assert!(
        answers.contains(&(Some(1101), "reject_once".to_owned())),
        "{answers:?}"
    );
    let last = lines.last().expect("a result line");
    assert_eq!(last["type"], json!("Result"), "{last}");
    assert_eq!(last["outcome"], json!("completed"), "{last}");
    assert_eq!(last["text"], json!("the image is ready"), "{last}");

    // The agent's side agrees: it got both options it offered back as the client chose.
    let inside = read_json(&state_dir(&repo).join("permission-1100.json"));
    assert_eq!(inside["outcome"]["optionId"], json!("allow"), "{inside}");
    let outside = read_json(&state_dir(&repo).join("permission-1101.json"));
    assert_eq!(outside["outcome"]["optionId"], json!("reject"), "{outside}");
}

/// `session/prompt` answering `refusal` is a failed attempt named after the reason, through the
/// retry table (ADR 0048 decisions 7 and 9).
#[test]
fn a_refusal_is_a_failed_outcome_named_after_the_reason() {
    let repo = Repo::new("acp-refusal");
    let store = setup(&repo);
    let plan = add_plan(&store, "acp");
    let task = add_task(&store, &plan, "refusal", None);
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let agent = fake_agent(&repo);
    set_flow_acp(&repo, "default", &agent, &["--stop-reason", "refusal"]);

    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let finished = finished(&repo, &task);
    assert_eq!(finished["outcome"], json!("refusal"), "{finished}");
    assert_eq!(finished["pushed"], json!(false), "{finished}");
    assert_eq!(repo.remote_branch(&task), None, "nothing was pushed");
    assert_eq!(
        state(&store, &task),
        ("todo".to_owned(), "retry_wait".to_owned())
    );
    let last = result_lines(&repo, &task)
        .pop()
        .expect("the wrapper's Result line");
    assert_eq!(last["type"], json!("Result"), "{last}");
    assert_eq!(last["outcome"], json!("refusal"), "{last}");
    let applied = read_json(&repo.run_dir(&task, 1).join("attempt_result.json"));
    assert_eq!(applied["class"], json!("infrastructure"), "{applied}");
}

/// `radian arc cancel` while the agent waits: the wrapper sends `session/cancel` first, the
/// agent ends its turn with `cancelled`, and the attempt is recorded as cancelled, not
/// terminated (ADR 0048 decision 9).
///
/// Mutation TK0202-M2: send no `session/cancel` before SIGTERM → the agent never sees the
/// protocol cancel and the marker is missing.
#[test]
fn a_cancel_sends_session_cancel_and_records_cancelled() {
    let repo = Repo::new("acp-cancel");
    let store = setup(&repo);
    tune(
        &repo,
        "stall_timeout_secs = 900",
        "stall_timeout_secs = 600",
    );
    tune(&repo, "cancel_grace_secs = 10", "cancel_grace_secs = 1");
    let plan = add_plan(&store, "acp");
    let task = add_task(&store, &plan, "cancel", None);
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let agent = fake_agent(&repo);
    set_flow_acp(&repo, "default", &agent, &["--wait-for-cancel"]);

    let mut start = repo.spawn(&["arc", "start", task.as_str()]);
    wait_for(
        &state_dir(&repo).join("prompt-seen"),
        Duration::from_secs(60),
    );
    let cancel = repo.radian(&["arc", "cancel", task.as_str()]);
    assert_eq!(cancel.code, 0, "cancel failed: {}", cancel.stderr);
    wait_for(
        &repo.run_dir(&task, 1).join("attempt_finished.json"),
        Duration::from_secs(30),
    );
    let _ = start.wait();
    assert_eq!(
        state(&store, &task),
        ("failed".to_owned(), "cancelled".to_owned()),
        "{}",
        cancel.stdout
    );
    assert!(
        state_dir(&repo).join("cancel-seen").is_file(),
        "the agent never received `session/cancel`"
    );
    let last = result_lines(&repo, &task)
        .pop()
        .expect("the agent's cancelled turn is the attempt's Result");
    assert_eq!(last["outcome"], json!("cancelled"), "{last}");
}

/// The three refusals before `start` claims anything: two worker lines on one flow, an ACP flow
/// beside `rework_session = "resume"`, and a program that does not resolve. Each exits `3` and
/// names its keys (ADR 0048 decisions 1, 4 and 7).
#[test]
fn the_acp_config_errors_are_refused_before_start() {
    // A flow with `acp` and `command`: both keys are named.
    let repo = Repo::new("acp-both-keys");
    let store = setup(&repo);
    let agent = fake_agent(&repo);
    set_flow_acp(&repo, "art", &agent, &[]);
    let plan = add_plan(&store, "acp");
    let task = add_task(&store, &plan, "both", Some("art"));
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    tune(
        &repo,
        "[flows.art]\nacp = [",
        "[flows.art]\ncommand = [\"/bin/sh\"]\nacp = [",
    );
    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 3, "start: {run:?}");
    assert!(run.stderr.contains("flows.art.acp"), "{}", run.stderr);
    assert!(run.stderr.contains("flows.art.command"), "{}", run.stderr);

    // `acp` beside `rework_session = "resume"`: both keys are named.
    let repo = Repo::new("acp-resume");
    let store = setup(&repo);
    let plan = add_plan(&store, "acp");
    let task = add_task(&store, &plan, "resume", None);
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    let agent = fake_agent(&repo);
    set_flow_acp(&repo, "default", &agent, &[]);
    tune(
        &repo,
        "rework_session = \"fresh\"",
        "rework_session = \"resume\"",
    );
    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 3, "start: {run:?}");
    assert!(
        run.stderr.contains("worker.rework_session"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("flows.default.acp"), "{}", run.stderr);

    // A program that does not resolve: `flows.<name>.acp` and the program are named.
    let repo = Repo::new("acp-missing");
    let store = setup(&repo);
    let plan = add_plan(&store, "acp");
    let task = add_task(&store, &plan, "missing", None);
    plan_ops::confirm(&store, &plan).expect("confirm the plan");
    tune(
        &repo,
        "[ids]\n",
        "[flows.default]\nacp = [\"definitely-not-a-program-xyz\"]\n\n[ids]\n",
    );
    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 3, "start: {run:?}");
    assert!(run.stderr.contains("flows.default.acp"), "{}", run.stderr);
    assert!(
        run.stderr.contains("definitely-not-a-program-xyz"),
        "{}",
        run.stderr
    );
}

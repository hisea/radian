//! ADR 0043 decision 1a: the `subagent` tool — a blocking, one-shot child `radian exec`.
//!
//! Every test drives the real binary against the mock provider: the parent is `radian exec`
//! and each child is the same binary started by the tool. The assertions are on facts the
//! model can see (the tools each session advertised, the tool results) and on durable facts
//! (the child's journal), never on timing.
//!
//! The provider's own testing module carries [`Barrier`](agent_provider_openai_compatible::testing::Barrier),
//! which releases the children only when all of them have reached the model at once. The
//! parallel test asserts its recorded peak, so "three children overlapped" is a fact.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{Barrier, MockServer, Reply, chunk};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

/// A hang watchdog, not a performance assertion (the same shape as `cli.rs`'s).
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);
const STATE_WATCHDOG: Duration = Duration::from_secs(60);

// ---------- helpers ----------

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agent-cli-subagent-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One isolated environment: `HOME`/`RADIAN_HOME`, the session directory, a workspace, and a
/// catalog URL pointing at a closed port (no run reads the user's state or the network).
struct Env {
    home: PathBuf,
    sessions: PathBuf,
    workspace: PathBuf,
}

impl Env {
    fn new(name: &str) -> Self {
        let home = scratch(&format!("{name}-home"));
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let workspace = scratch(&format!("{name}-ws"));
        Self {
            home,
            sessions,
            workspace,
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

fn spawn_parent(env: &Env, args: &[&str]) -> Child {
    Command::new(common::radian_bin())
        .arg("exec")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("RADIAN_CONFIG")
        .env("HOME", &env.home)
        .env("RADIAN_HOME", &env.home)
        .env("RADIAN_SESSION_DIR", &env.sessions)
        .env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn radian")
}

/// Writes no input, closes stdin, and waits with a cap so a stuck process fails the test.
async fn run(mut child: Child) -> (i32, String, String) {
    let _ = child.stdin.take().unwrap().write_all(b"").await;
    let output = tokio::time::timeout(EXIT_WATCHDOG, child.wait_with_output())
        .await
        .expect("the process did not exit within the watchdog")
        .expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

/// The tool names a request advertised.
fn advertised(server: &MockServer, n: usize) -> Vec<String> {
    let mut names: Vec<String> = server.requests()[n].json()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The index of the request whose last user message is `task` (a child's first request).
fn child_request(server: &MockServer, task: &str) -> usize {
    server
        .requests()
        .iter()
        .enumerate()
        .find(|(_, request)| {
            let body = request.json();
            body["messages"]
                .as_array()
                .and_then(|messages| messages.iter().rev().find(|m| m["role"] == "user"))
                .and_then(|m| m["content"].as_str())
                .is_some_and(|text| text.contains(task))
        })
        .map(|(index, _)| index)
        .unwrap_or_else(|| panic!("no request carried the child task {task:?}"))
}

/// Every `role: "tool"` message's content in one recorded request.
fn tool_contents(server: &MockServer, n: usize) -> String {
    server.requests()[n].json()["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|m| m["role"] == "tool")
                .filter_map(|m| m["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The child session files (everything but `parent_id`).
fn child_journals(env: &Env, parent_id: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(&env.sessions)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|path| path.file_stem().and_then(|s| s.to_str()) != Some(parent_id))
        .collect();
    out.sort();
    out
}

fn created(journal: &Path) -> Value {
    jsonl(journal)
        .into_iter()
        .find(|event| event["event"] == "SessionCreated")
        .expect("SessionCreated")
}

/// Polls `predicate` until it holds, bounded; the sleep is only the poll interval.
async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(STATE_WATCHDOG, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn sigint(child: &Child) {
    // SAFETY: signal delivery to our own child process.
    unsafe {
        libc::kill(child.id().expect("child pid") as libc::pid_t, libc::SIGINT);
    }
}

// ---------- the tests ----------

/// ADR 0043 1a: `explore` is exactly the four read-only tools, so a `write` call is an unknown
/// tool rather than a permission decision. The mutation (mounting the general set for
/// `explore`, e.g. by dropping the hidden `--tools`) fails the exact-list assertion.
#[tokio::test]
async fn an_explore_child_has_only_the_four_read_tools_and_write_is_refused() {
    let server = MockServer::start(vec![
        tool_reply(
            "p1",
            "subagent",
            r#"{"agent":"explore","task":"EXPLORE-TASK"}"#,
        ),
        tool_reply("c1", "write", r#"{"path":"x.txt","content":"x"}"#),
        text_reply("EXPLORE-CONCLUSION"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let env = Env::new("explore");
    let workspace = env.workspace.to_str().unwrap().to_owned();
    let (code, stdout, stderr) = run(spawn_parent(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            &workspace,
            "-p",
            "go",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");

    let index = child_request(&server, "EXPLORE-TASK");
    assert_eq!(
        advertised(&server, index),
        ["glob", "grep", "list", "read"],
        "an explore child's tool list is exactly the read tools"
    );
    assert!(
        tool_contents(&server, index + 1).contains("no tool named `write`"),
        "a write call must be refused as unknown: {}",
        tool_contents(&server, index + 1)
    );
}

/// ADR 0043 1a point 4: a child never has `subagent` — depth 1. The mutation (removing the
/// depth check in `open_session`) mounts it in the child and this fails.
#[tokio::test]
async fn a_general_child_has_no_subagent_tool() {
    let server = MockServer::start(vec![
        tool_reply(
            "p1",
            "subagent",
            r#"{"agent":"general","task":"GENERAL-TASK"}"#,
        ),
        text_reply("GENERAL-CONCLUSION"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let env = Env::new("general");
    let workspace = env.workspace.to_str().unwrap().to_owned();
    let (code, stdout, stderr) = run(spawn_parent(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            &workspace,
            "--session-id",
            "parent",
            "-p",
            "go",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");

    let index = child_request(&server, "GENERAL-TASK");
    let names = advertised(&server, index);
    assert!(
        !names.iter().any(|name| name == "subagent"),
        "a child must not have `subagent`: {names:?}"
    );
    assert!(
        names.iter().any(|name| name == "write"),
        "`general` keeps the parent's ordinary tools: {names:?}"
    );

    // ADR 0043 1a point 6: the lineage is durable, and it is what makes the depth rule hold
    // on a resume as well.
    let journals = child_journals(&env, "parent");
    assert_eq!(journals.len(), 1, "{journals:?}");
    let created = created(&journals[0]);
    assert_eq!(created["data"]["parent"]["session_id"], "parent");
    assert_eq!(created["data"]["parent"]["relation"], "subagent");
}

/// ADR 0043 1a point 5: the child records the parent's resolved tier, never a fixed one. The
/// mutation (`--profile full` always) fails the `workspace` case.
#[tokio::test]
async fn a_child_records_the_parents_tier() {
    for (tier, expected) in [("workspace", "workspace"), ("full", "full")] {
        let server = MockServer::start(vec![
            tool_reply(
                "p1",
                "subagent",
                &format!(r#"{{"agent":"general","task":"TIER-{tier}"}}"#),
            ),
            text_reply("CHILD"),
            text_reply("PARENT-DONE"),
        ])
        .await;
        let env = Env::new(&format!("tier-{tier}"));
        let workspace = env.workspace.to_str().unwrap().to_owned();
        let (code, _, stderr) = run(spawn_parent(
            &env,
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                &workspace,
                "--profile",
                tier,
                "--session-id",
                "parent",
                "-p",
                "go",
            ],
        ))
        .await;
        assert_eq!(code, 0, "{tier}: {stderr}");
        let journals = child_journals(&env, "parent");
        assert_eq!(journals.len(), 1, "{tier}: {journals:?}");
        assert_eq!(
            created(&journals[0])["data"]["config_snapshot"]["access"],
            expected,
            "{tier}: the child's SessionCreated records the parent's tier"
        );
    }
}

/// ADR 0043 1a point 3: several `subagent` calls in one reply run in parallel. The mock holds
/// every child's model request until all three have arrived, and records the peak. The
/// mutation (`ToolConcurrency::Exclusive`) makes the peak 1 and this fails.
#[tokio::test]
async fn three_subagent_calls_in_one_reply_run_at_the_same_time() {
    let barrier = Barrier::new(3, Duration::from_secs(10));
    let mut replies = vec![
        // The parent asks for three children in one assistant message.
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [
                    {"index": 0, "id": "p1", "type": "function",
                     "function": {"name": "subagent", "arguments": "{\"task\":\"ONE\"}"}},
                    {"index": 1, "id": "p2", "type": "function",
                     "function": {"name": "subagent", "arguments": "{\"task\":\"TWO\"}"}},
                    {"index": 2, "id": "p3", "type": "function",
                     "function": {"name": "subagent", "arguments": "{\"task\":\"THREE\"}"}}
                ]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
    ];
    for _ in 0..3 {
        replies.push(Reply::Barrier {
            barrier: barrier.clone(),
            reply: Box::new(text_reply("CHILD-DONE")),
        });
    }
    replies.push(text_reply("PARENT-DONE"));
    let server = MockServer::start(replies).await;
    let env = Env::new("parallel");
    let (code, stdout, stderr) = run(spawn_parent(
        &env,
        &["--model", "m", "--base-url", &server.base_url, "-p", "go"],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");
    assert_eq!(
        barrier.peak(),
        3,
        "three children must be inside the model call at the same time"
    );
}

/// ADR 0043 1a point 1: a conclusion longer than the bound is cut and the cut is visible to the
/// model. The parent's second request carries the tool result the model actually sees.
#[tokio::test]
async fn a_long_conclusion_is_cut_and_marked() {
    let long = "x".repeat(4500);
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"LONG-TASK"}"#),
        text_reply(&long),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let env = Env::new("long");
    let (code, _, stderr) = run(spawn_parent(
        &env,
        &["--model", "m", "--base-url", &server.base_url, "-p", "go"],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    let result = tool_contents(&server, 2);
    assert!(
        result.contains("[truncated: 500 more chars]"),
        "the marker names the remaining characters: {result}"
    );
    assert!(
        result.starts_with(&"x".repeat(4000)),
        "the first 4000 characters are kept: {}",
        &result[..result.len().min(80)]
    );
}

/// ADR 0043 1a point 7: a non-auto parent gives the child no `--yes`, so the child's `ask` is
/// denied (it has no terminal) and the result says what was denied. The mutation (passing
/// `--yes` always) lets the shell run and the result has no `denied` line.
#[tokio::test]
async fn a_non_auto_parent_denies_the_childs_ask_and_says_so() {
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"ASK-TASK"}"#),
        tool_reply("c1", "shell", r#"{"command":"echo SHOULD-NOT-RUN"}"#),
        text_reply("CHILD-COULD-NOT"),
        text_reply("PARENT-SAW"),
    ])
    .await;
    let env = Env::new("ask");
    let workspace = env.workspace.to_str().unwrap().to_owned();
    let (code, stdout, stderr) = run(spawn_parent(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            &workspace,
            "-p",
            "go",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-SAW"), "{stdout}");
    let result = tool_contents(&server, 3);
    assert!(
        result.contains("denied: shell"),
        "the result must name the denied tool: {result}"
    );
    assert!(
        result.contains("no terminal to ask in"),
        "the reason comes from the child's own PermissionResolved: {result}"
    );
}

/// ADR 0043 1a: a cancelled parent turn kills the child's whole session. The child is parked
/// on a model request that never answers; the test finds its pid from the control socket it
/// binds at startup, cancels the parent, and polls (bounded) for the process to go away. The
/// mutation (not propagating cancellation) leaves it alive.
#[tokio::test]
async fn a_cancelled_parent_turn_kills_the_child() {
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"CANCEL-TASK"}"#),
        Reply::SseThenHang { pieces: Vec::new() },
    ])
    .await;
    let env = Env::new("cancel");
    let parent = spawn_parent(
        &env,
        &["--model", "m", "--base-url", &server.base_url, "-p", "go"],
    );
    let parent_pid = parent.id().expect("parent pid");
    let run_dir = env.home.join("run");

    // The child binds `<RADIAN_HOME>/run/<pid>.sock` before its first model call; the parent's
    // own socket is already there. A second socket is the child.
    wait_until("the child's control socket", || {
        std::fs::read_dir(&run_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.path().file_stem()?.to_str()?.parse::<u32>().ok())
            .any(|pid| pid != parent_pid)
    })
    .await;
    let child_pid = std::fs::read_dir(&run_dir)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.path().file_stem()?.to_str()?.parse::<u32>().ok())
        .find(|pid| *pid != parent_pid)
        .expect("the child pid");

    sigint(&parent);
    // The deadline is shorter than the core's 5 s `DEFAULT_TOOL_CANCEL_GRACE`: the tool's own
    // cancellation branch kills the session at once, while a tool that ignores the token would
    // only be dropped (and its leader killed by `kill_on_drop`) after the grace. A bounded
    // poll, not a guess: the kill is a few milliseconds of process scheduling.
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(child_pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive(child_pid),
        "the child process {child_pid} must be gone after the parent turn was cancelled"
    );
    let _ = run(parent).await;
}

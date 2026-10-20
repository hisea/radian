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
    spawn_parent_env(env, args, &[])
}

/// [`spawn_parent`] with extra environment (e.g. `RADIAN_CONFIG`, which the child inherits too).
fn spawn_parent_env(env: &Env, args: &[&str], extra: &[(&str, String)]) -> Child {
    let mut command = Command::new(common::radian_bin());
    command
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
        .kill_on_drop(true);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.spawn().expect("spawn radian")
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

fn session_of(pid: u32) -> i32 {
    // SAFETY: `getsid` takes a pid and has no memory-safety preconditions.
    unsafe { libc::getsid(pid as libc::pid_t) }
}

/// The child radian's pid: it binds `<RADIAN_HOME>/run/<pid>.sock` before its first model call,
/// and the parent's own socket is already there, so the other socket is the child.
async fn child_pid(env: &Env, parent_pid: u32) -> u32 {
    let run_dir = env.home.join("run");
    let others = || -> Vec<u32> {
        std::fs::read_dir(&run_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.path().file_stem()?.to_str()?.parse::<u32>().ok())
            .filter(|pid| *pid != parent_pid)
            .collect()
    };
    wait_until("the child's control socket", || !others().is_empty()).await;
    others().into_iter().next().expect("the child pid")
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
async fn an_explore_child_has_only_the_three_read_tools_and_write_is_refused() {
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
        ["glob", "grep", "read"],
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
/// denied (it has no terminal) and the result says what was denied. Runs under
/// `--profile workspace` with an `outside_sandbox` call — a sandboxed shell no longer
/// asks there since slice 1 of `sandbox-auto-allow-shell` (`full` has nothing left to
/// deny). The mutation (passing `--yes` always) lets the call run and the result has no
/// `denied` line.
#[tokio::test]
async fn a_non_auto_parent_denies_the_childs_ask_and_says_so() {
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"ASK-TASK"}"#),
        tool_reply(
            "c1",
            "shell",
            r#"{"command":"echo SHOULD-NOT-RUN","outside_sandbox":true,"outside_sandbox_reason":"the test leaves the sandbox"}"#,
        ),
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
            "--profile",
            "workspace",
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
    let child_pid = child_pid(&env, parent_pid).await;

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

/// ADR 0043 1a (gap 1, fix round 1 A): the child reads the parent's header variable at startup
/// and **removes it from its own environment**, so a `shell` command it starts cannot inherit
/// the secret. The config names the variable in `[sandbox] allow_env` — that is the only way the
/// value could reach such a command, and without it this test would pass against the old code
/// too. The mutation (leaving the variable in the child's environment) writes the secret into
/// the file this asserts is clean.
#[tokio::test]
async fn the_childs_own_environment_no_longer_carries_the_parents_header() {
    const SECRET: &str = "child-env-secret-9";
    let env = Env::new("child-env");
    let dump = env.home.join("dump.txt");
    let command = format!(
        "printf '%s' \"$RADIAN_CHILD_HEADERS\" > '{}'",
        dump.display()
    );
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"LEAK-TASK"}"#),
        tool_reply("c1", "shell", &json!({"command": command}).to_string()),
        text_reply("CHILD-DONE"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let config = env.home.join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[sandbox]\nallow_env = [\"RADIAN_CHILD_HEADERS\"]\n",
    )
    .unwrap();
    let workspace = env.workspace.to_str().unwrap().to_owned();
    let (code, stdout, stderr) = run(spawn_parent_env(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            &workspace,
            "--header",
            &format!("X-Test={SECRET}"),
            "--yes",
            "--session-id",
            "parent",
            "-p",
            "go",
        ],
        &[("RADIAN_CONFIG", config.to_string_lossy().into_owned())],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");

    // The child did read the header: its own request carries it.
    let index = child_request(&server, "LEAK-TASK");
    assert_eq!(
        server.requests()[index].header("x-test"),
        Some(SECRET),
        "the child must send the parent's header"
    );

    // ... and its environment no longer holds it, so the command that named it did not get it.
    let dump = std::fs::read_to_string(&dump).unwrap_or_default();
    assert!(
        !dump.contains(SECRET),
        "the child's own environment must not carry the header: {dump}"
    );
}

/// ADR 0043 1a (gap 1, fix round 1 A, the belt and braces): an MCP server the child starts must
/// not inherit the parent's header secret, even when the config names the variable in
/// `inherit_env` — the spawn clears the environment, so a name in that list is the only path
/// left. The wrapper around the repo's mcp fixture appends the variable to a file; both the
/// parent's and the child's server process write there, and neither may see the secret. The
/// mutation (removing neither the child's startup removal nor the spawn's own removal) writes it.
#[tokio::test]
async fn an_mcp_server_the_child_starts_does_not_get_the_parents_header() {
    const SECRET: &str = "mcp-env-secret-5";
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"MCP-TASK"}"#),
        text_reply("CHILD-DONE"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let env = Env::new("mcp-env");
    let fixture = common::target_dir().join("mcp-fixture");
    assert!(
        fixture.exists(),
        "mcp-fixture is not built; run: cargo build -p agent-extensions --bin mcp-fixture"
    );
    let dump = env.home.join("mcp-env.txt");
    let wrapper = env.home.join("mcp-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$RADIAN_CHILD_HEADERS\" >> '{dump}'\nexec '{fixture}'\n",
            dump = dump.display(),
            fixture = fixture.display(),
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).unwrap();
    }
    let config = env.home.join("mcp.json");
    std::fs::write(
        &config,
        json!({"servers": [{
            "id": "fx",
            "command": wrapper,
            "inherit_env": ["RADIAN_CHILD_HEADERS"],
            "env": {"FIXTURE_LOG": env.home.join("fixture.log")},
            "call_timeout_secs": 30
        }]})
        .to_string(),
    )
    .unwrap();
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
            "--header",
            &format!("X-Test={SECRET}"),
            "--mcp-config",
            config.to_str().unwrap(),
            "--session-id",
            "parent",
            "-p",
            "go",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");

    // The child read the header: its own request carries it.
    let index = child_request(&server, "MCP-TASK");
    assert_eq!(
        server.requests()[index].header("x-test"),
        Some(SECRET),
        "the child must send the parent's header"
    );

    // Neither MCP server process (the parent's or the child's) inherited the variable.
    let dump = std::fs::read_to_string(&dump).unwrap_or_default();
    assert!(
        !dump.contains(SECRET),
        "an MCP server must not inherit the parent's header: {dump}"
    );
}

/// ADR 0043 1a (gap 1): a `--header` on the parent's command line reaches the child. The value
/// is a secret, so it must not be in the child's argv (the tool carries it in the environment)
/// and must not be written to the child's journal; the child's own request to the mock is where
/// it is allowed to appear. The mutation (not forwarding it) fails the request assertion.
#[tokio::test]
async fn the_parents_header_reaches_the_child_and_nothing_else_writes_it() {
    const SECRET: &str = "subagent-header-secret-42";
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"HEADER-TASK"}"#),
        text_reply("CHILD-DONE"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let env = Env::new("header");
    let (code, stdout, stderr) = run(spawn_parent(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--header",
            &format!("X-Test={SECRET}"),
            "--session-id",
            "parent",
            "-p",
            "go",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");

    // The child's own request carries the header — the provider that needs it does not fail.
    let index = child_request(&server, "HEADER-TASK");
    assert_eq!(
        server.requests()[index].header("x-test"),
        Some(SECRET),
        "the child must send the parent's header"
    );

    // The child's journal is the only durable thing it writes, and the header must not be in
    // it — nor in what the parent printed.
    let journals = child_journals(&env, "parent");
    assert_eq!(journals.len(), 1, "{journals:?}");
    let journal = std::fs::read_to_string(&journals[0]).unwrap();
    assert!(
        !journal.contains(SECRET),
        "the child's journal must not carry the header value"
    );
    assert!(
        !stdout.contains(SECRET) && !stderr.contains(SECRET),
        "the secret must never be printed: {stdout}{stderr}"
    );
}

/// The two pids the stub command records: its own (the command runs in its own session, so this
/// is that session's leader) and `$PPID` (the child radian), or `None` until the file is
/// complete. No control socket is involved: a long temp path makes `run/<pid>.sock` exceed
/// `SUN_LEN`, and the pid is what this test needs.
fn recorded_pids(path: &Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    let command = parts.next()?.parse().ok()?;
    let child = parts.next()?.parse().ok()?;
    Some((command, child))
}

async fn wait_for_recorded_pids(path: &Path, what: &str) -> (u32, u32) {
    wait_until(what, || recorded_pids(path).is_some()).await;
    recorded_pids(path).expect("waited for it")
}

/// ADR 0043 1a (fix round 1 B): cancelling the parent's turn must stop what the **child's**
/// `shell` tool started. That command runs in its own session (`setsid`), so the SIGKILL of the
/// child's session the tool used to send could not reach it: the child has to get SIGTERM first,
/// cancel its own turn — which is what kills the command's session — and then close its session.
/// The command records its own pid and its parent's; the assertion is that its pid is gone within
/// a bound, and the shape (its session is not the child's) is checked first, because a command
/// that stayed in the child's session would make the test prove nothing.
#[tokio::test]
async fn a_cancelled_parent_turn_stops_the_command_the_childs_shell_started() {
    let env = Env::new("shell-cancel");
    let pid_path = env.home.join("stub.pid");
    let command = format!("echo $$ $PPID > '{}'; sleep 300", pid_path.display());
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"SHELL-TASK"}"#),
        tool_reply("c1", "shell", &json!({"command": command}).to_string()),
        text_reply("CHILD-DONE"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let workspace = env.workspace.to_str().unwrap().to_owned();
    let parent = spawn_parent(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            &workspace,
            "--yes",
            "-p",
            "go",
        ],
    );

    let (stub, child) = wait_for_recorded_pids(&pid_path, "the child's shell command").await;
    assert_ne!(
        session_of(stub),
        session_of(child),
        "the command {stub} must be in its own session (the child {child} is {}), or this test \
         proves nothing",
        session_of(child)
    );

    sigint(&parent);
    // A bounded poll, not a guess: the parent cancels its turn, SIGTERMs the child, the child
    // cancels its own turn (killing the command's session) and exits.
    wait_until("the child's shell command to be gone", || !alive(stub)).await;
    assert!(
        !alive(stub),
        "the command {stub} outlived the cancelled turn"
    );
    let (code, _, _) = run(parent).await;
    assert_ne!(code, 0, "a cancelled turn is not a clean exit");
}

/// ADR 0043 1a (fix round 1 B): the same two-step stop reaches a **background job** the child
/// started. The child is parked in a model call that never answers, so its only cleanup path is
/// the session end: SIGTERM cancels the turn, the driver closes the session, and `kill_all`
/// SIGKILLs the whole session of every running job (ADR 0037 decision 6) — a job that a SIGKILL
/// of the child could never reach, because the job registry runs `setsid` too.
#[tokio::test]
async fn a_cancelled_parent_turn_stops_the_childs_background_job() {
    let env = Env::new("job-cancel");
    let pid_path = env.home.join("job.pid");
    let command = format!("echo $$ $PPID > '{}'; sleep 300", pid_path.display());
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"JOB-TASK"}"#),
        tool_reply(
            "c1",
            "shell",
            &json!({"command": command, "background": true}).to_string(),
        ),
        Reply::SseThenHang { pieces: Vec::new() },
    ])
    .await;
    let workspace = env.workspace.to_str().unwrap().to_owned();
    let parent = spawn_parent(
        &env,
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            &workspace,
            "--yes",
            "-p",
            "go",
        ],
    );

    let (job, child) = wait_for_recorded_pids(&pid_path, "the child's background job").await;
    assert_ne!(
        session_of(job),
        session_of(child),
        "the job {job} must be in its own session (the child {child} is {}), or this test proves \
         nothing",
        session_of(child)
    );

    sigint(&parent);
    wait_until("the child's background job to be gone", || !alive(job)).await;
    assert!(!alive(job), "the job {job} outlived the cancelled turn");
    let (code, _, _) = run(parent).await;
    assert_ne!(code, 0, "a cancelled turn is not a clean exit");
}

/// Slice 1 of `sandbox-auto-allow-shell`: a `general` child of a non-auto `workspace`
/// parent runs a sandboxed shell with no approval at all — the tier's blanket `allow`
/// answers it, so the headless child never meets the no-terminal denial. No extra
/// wiring: the child inherits the tier (0043 1a) and builds the sandboxed profile.
/// Before the slice the same call was denied; denying it again fails this test.
#[tokio::test]
async fn a_non_auto_workspace_child_runs_a_sandboxed_shell() {
    let server = MockServer::start(vec![
        tool_reply(
            "p1",
            "subagent",
            r#"{"agent":"general","task":"CHECK-TASK"}"#,
        ),
        tool_reply("c1", "shell", r#"{"command":"cargo check"}"#),
        text_reply("CHILD-DONE"),
        text_reply("PARENT-DONE"),
    ])
    .await;
    let env = Env::new("child-shell");
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
            "--profile",
            "workspace",
            "--session-id",
            "parent",
            "-p",
            "go",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PARENT-DONE"), "{stdout}");
    // The child's own follow-up request carries the shell result: it ran.
    let index = child_request(&server, "CHECK-TASK");
    let result = tool_contents(&server, index + 1);
    assert!(
        result.contains("exit code"),
        "the child's shell must have run: {result}"
    );
    assert!(
        !result.contains("denied"),
        "nothing in the child's shell may be denied: {result}"
    );
    assert!(
        !result.contains("no terminal to ask in"),
        "the child must not have been asked: {result}"
    );
    // …and it stayed in the parent's tier.
    let journals = child_journals(&env, "parent");
    assert_eq!(journals.len(), 1, "{journals:?}");
    assert_eq!(
        created(&journals[0])["data"]["config_snapshot"]["access"],
        "workspace",
        "the child's SessionCreated records the parent's tier"
    );
}

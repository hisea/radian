//! Integration tests against real `mcp-fixture` processes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::*;
use agent_extensions::{McpServerConfig, McpSession, McpToolProvider, tool_alias};
use serde_json::{Value, json};

// ---------- helpers ----------

fn fixture(id: &str) -> McpServerConfig {
    McpServerConfig::new(id, env!("CARGO_BIN_EXE_mcp-fixture"))
}

fn legacy(id: &str) -> McpServerConfig {
    fixture(id).arg("--legacy")
}

/// A fresh log file the fixture appends received methods to.
fn log_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-extensions-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn logged(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn logged_pids(path: &PathBuf) -> Vec<u32> {
    logged(path)
        .iter()
        .filter_map(|l| l.strip_prefix("pid:")?.parse().ok())
        .collect()
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

async fn gone_within(pid: u32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

async fn open(configs: Vec<McpServerConfig>) -> McpSession {
    McpToolProvider::new(configs).open_session().await.unwrap()
}

async fn call(session: &McpSession, name: &str, args: Value) -> Result<ToolOutput, ToolFailure> {
    call_with(session, name, args, CancellationToken::new()).await
}

async fn call_with(
    session: &McpSession,
    name: &str,
    args: Value,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolFailure> {
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments: args,
    };
    let cx = ToolContext {
        cancellation,
        events: &NoopSink,
        index: 0,
    };
    session.execute(&call, cx).await
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn pid_of(session: &McpSession, server: &str) -> u32 {
    let out = call(session, &format!("{server}__pid"), json!({}))
        .await
        .unwrap();
    text(&out).parse().unwrap()
}

/// Returns scripted replies in order.
struct Scripted(Mutex<VecDeque<ModelResponse>>);

impl Model for Scripted {
    async fn complete(
        &self,
        _request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::Protocol("script exhausted".into()))
    }
}

fn calls_tool(name: &str, args: Value) -> ModelResponse {
    ModelResponse {
        content: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }],
        provider_payload: None,
        stop_reason: StopReason::ToolUse,
    }
}

fn answers(text: &str) -> ModelResponse {
    ModelResponse {
        content: vec![ContentPart::text(text)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
    }
}

// ---------- end to end ----------

#[tokio::test]
async fn tool_call_round_trips_through_core_on_both_protocol_versions() {
    for (config, expected_methods) in [
        (
            fixture("fx"),
            vec!["server/discover", "tools/list", "tools/call"],
        ),
        (
            legacy("fx"),
            vec![
                "server/discover",
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call",
            ],
        ),
    ] {
        let log = log_file("e2e");
        let config = config.env("FIXTURE_LOG", &log);
        let model = Scripted(Mutex::new(VecDeque::from([
            calls_tool("fx__echo", json!({"hello": "mcp"})),
            answers("done"),
        ])));
        let agent = Arc::new(Agent::new(model, McpToolProvider::new(vec![config])));
        let mut chat = agent.open_session().await.unwrap();

        let outcome = chat
            .run_turn("go", TurnOptions::default(), &NoopSink)
            .await
            .unwrap();

        assert_eq!(outcome.status, RunStatus::Completed);
        let result = chat
            .transcript()
            .iter()
            .find_map(|m| match m {
                Message::Tool { result } => Some(result.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(result.call_id, "c1");
        assert!(!result.is_error);
        assert_eq!(result.text(), r#"{"hello":"mcp"}"#);
        chat.close().await.unwrap();

        let methods: Vec<String> = logged(&log)
            .into_iter()
            .filter(|l| !l.starts_with("pid:"))
            .collect();
        assert_eq!(methods, expected_methods);
    }
}

// ---------- tools and aliases ----------

#[tokio::test]
async fn aliases_are_readable_valid_and_stable() {
    let first = open(vec![fixture("fx")]).await;
    let second = open(vec![legacy("fx")]).await;
    let names = |s: &McpSession| {
        s.tool_specs()
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>()
    };

    assert_eq!(names(&first), names(&second));
    assert!(names(&first).contains(&"fx__echo".to_owned()));
    let odd = tool_alias("fx", "Bad Name!");
    assert!(names(&first).contains(&odd));
    assert!(
        odd.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
    // The alias routes back to the original MCP name.
    let out = call(&first, &odd, json!({"x": 1})).await.unwrap();
    assert_eq!(text(&out), r#"{"x":1}"#);
}

#[tokio::test]
async fn alias_collision_fails_open_and_reaps_servers() {
    let log = log_file("collision");
    let configs = vec![
        fixture("fx").env("FIXTURE_LOG", &log),
        legacy("fx").env("FIXTURE_LOG", &log),
    ];

    let err = McpToolProvider::new(configs)
        .open_session()
        .await
        .err()
        .unwrap();

    assert!(err.0.contains("both map to"), "{err}");
    let pids = logged_pids(&log);
    assert_eq!(pids.len(), 2);
    for pid in pids {
        assert!(
            gone_within(pid, Duration::from_secs(3)).await,
            "server {pid} still running"
        );
    }
}

#[tokio::test]
async fn concurrency_comes_from_config_not_server_claims() {
    let concurrency = |s: &McpSession, name: &str| {
        s.tool_specs()
            .iter()
            .find(|t| t.name == name)
            .unwrap()
            .concurrency
    };

    let untrusted = open(vec![fixture("fx")]).await;
    assert_eq!(
        concurrency(&untrusted, "fx__read_only"),
        ToolConcurrency::Exclusive
    );

    let mut config = fixture("fx");
    config.trust_read_only_hint = true;
    config.shared_tools = vec!["echo".into()];
    let trusted = open(vec![config]).await;
    assert_eq!(
        concurrency(&trusted, "fx__read_only"),
        ToolConcurrency::Shared
    );
    assert_eq!(concurrency(&trusted, "fx__echo"), ToolConcurrency::Shared);
    assert_eq!(
        concurrency(&trusted, "fx__fail"),
        ToolConcurrency::Exclusive
    );
}

#[tokio::test]
async fn tool_errors_and_rich_content() {
    let session = open(vec![fixture("fx")]).await;

    let failure = call(&session, "fx__fail", json!({})).await.unwrap_err();
    assert_eq!(failure.code, "tool_error");
    assert_eq!(failure.message, "fixture failure");

    let rich = call(&session, "fx__rich", json!({})).await.unwrap();
    assert_eq!(rich.content.len(), 3);
    assert!(
        matches!(&rich.content[0], ContentPart::Text { meta: Some(m), .. } if m == &json!({"trace": "t-1"}))
    );
    assert!(
        matches!(&rich.content[1], ContentPart::Image { meta: Some(m), .. } if m == &json!({"origin": "fixture"}))
    );
    assert!(matches!(
        &rich.content[2],
        ContentPart::ResourceLink { meta: None, .. }
    ));
    assert!(
        matches!(&rich.content[1], ContentPart::Image { mime_type, data, .. } if mime_type == "image/png" && data == "AAAA")
    );
    assert!(
        matches!(&rich.content[2], ContentPart::ResourceLink { uri, name: Some(n), .. } if uri == "file:///tmp/a.txt" && n == "a.txt")
    );
    assert_eq!(rich.structured, Some(json!({"answer": 42})));
}

#[tokio::test]
async fn environment_is_allowlisted() {
    // SAFETY: these variable names are used by no other test.
    unsafe {
        std::env::set_var("FIXTURE_SECRET_TOKEN", "leaked");
        std::env::set_var("FIXTURE_INHERITED", "passed through");
    }
    let mut config = fixture("fx").env("FIXTURE_EXPLICIT", "set by config");
    config.inherit_env = vec!["FIXTURE_INHERITED".into()];
    let session = open(vec![config]).await;
    let read = |name: &'static str| {
        let session = &session;
        async move {
            text(
                &call(session, "fx__env", json!({"name": name}))
                    .await
                    .unwrap(),
            )
        }
    };

    assert_eq!(read("FIXTURE_SECRET_TOKEN").await, "<unset>");
    assert_eq!(read("FIXTURE_INHERITED").await, "passed through");
    assert_eq!(read("FIXTURE_EXPLICIT").await, "set by config");
}

#[tokio::test]
async fn server_requests_for_roots_are_refused_without_hanging() {
    let session = open(vec![legacy("fx")]).await;
    let start = Instant::now();

    let out = call(&session, "fx__ask_roots", json!({})).await.unwrap();

    let reply: Value = serde_json::from_str(&text(&out)).unwrap();
    assert_eq!(reply["error"]["code"], -32601);
    assert!(start.elapsed() < Duration::from_secs(2));
}

// ---------- state, concurrency, cancellation ----------

#[tokio::test]
async fn server_state_survives_turns_but_not_sessions() {
    let provider = McpToolProvider::new(vec![fixture("fx")]);
    let model = Scripted(Mutex::new(VecDeque::from([
        calls_tool("fx__counter", json!({})),
        answers("one"),
        calls_tool("fx__counter", json!({})),
        answers("two"),
    ])));
    let agent = Arc::new(Agent::new(model, provider.clone()));
    let mut chat = agent.open_session().await.unwrap();
    chat.run_turn("count", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    chat.run_turn("count again", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let counts: Vec<String> = chat
        .transcript()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { result } => Some(result.text()),
            _ => None,
        })
        .collect();
    assert_eq!(counts, ["1", "2"]);
    chat.close().await.unwrap();

    let fresh = provider.open_session().await.unwrap();
    assert_eq!(
        text(&call(&fresh, "fx__counter", json!({})).await.unwrap()),
        "1"
    );
}

#[tokio::test]
async fn shared_calls_overlap_on_one_server() {
    let session = open(vec![fixture("fx")]).await;
    let start = Instant::now();

    let (a, b) = tokio::join!(
        call(&session, "fx__sleep", json!({"ms": 400})),
        call(&session, "fx__sleep", json!({"ms": 400})),
    );

    assert!(a.is_ok() && b.is_ok());
    assert!(
        start.elapsed() < Duration::from_millis(750),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn cancelled_call_notifies_then_stops_the_server() {
    let log = log_file("cancel");
    let session = open(vec![fixture("fx").env("FIXTURE_LOG", &log)]).await;
    let pid = pid_of(&session, "fx").await;
    let token = CancellationToken::new();
    let trigger = token.clone();
    let start = Instant::now();

    let (result, ()) = tokio::join!(
        call_with(&session, "fx__sleep", json!({"ms": 5000}), token),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        }
    );

    let failure = result.unwrap_err();
    assert_eq!(failure.code, "cancelled");
    assert!(failure.message.contains("stopped"));
    assert!(start.elapsed() < Duration::from_secs(2));
    // The server read the cancellation before it was stopped.
    assert!(logged(&log).iter().any(|l| l.starts_with("cancelled:")));
    // It was still answering pings, yet it is not reused: its abandoned work may still run.
    assert!(gone_within(pid, Duration::from_secs(3)).await);
    assert_eq!(
        call(&session, "fx__echo", json!({}))
            .await
            .unwrap_err()
            .code,
        "extension_unavailable"
    );
}

#[tokio::test]
async fn timeout_on_a_frozen_server_kills_it_and_marks_it_unavailable() {
    let mut config = fixture("fx");
    config.call_timeout = Duration::from_millis(200);
    config.cancel_grace = Duration::from_millis(300);
    let session = open(vec![config]).await;
    let pid = pid_of(&session, "fx").await;

    let failure = call(&session, "fx__freeze", json!({})).await.unwrap_err();

    assert_eq!(failure.code, "extension_timeout");
    assert!(gone_within(pid, Duration::from_secs(3)).await);
    let next = call(&session, "fx__echo", json!({})).await.unwrap_err();
    assert_eq!(next.code, "extension_unavailable");
    assert!(next.message.contains("Do not call its tools again"));
}

#[tokio::test]
async fn timeout_stops_even_a_server_that_still_responds() {
    let mut config = fixture("fx");
    config.call_timeout = Duration::from_millis(200);
    let session = open(vec![config]).await;
    let pid = pid_of(&session, "fx").await;

    let failure = call(&session, "fx__sleep", json!({"ms": 5000}))
        .await
        .unwrap_err();

    // `sleep` keeps running in the background while the server answers pings; a later call
    // must not be able to run alongside it.
    assert_eq!(failure.code, "extension_timeout");
    assert!(gone_within(pid, Duration::from_secs(3)).await);
    assert_eq!(
        call(&session, "fx__echo", json!({}))
            .await
            .unwrap_err()
            .code,
        "extension_unavailable"
    );
}

#[tokio::test]
async fn dropping_an_in_flight_call_kills_the_server() {
    let session = open(vec![fixture("fx")]).await;
    let pid = pid_of(&session, "fx").await;

    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        call(&session, "fx__sleep", json!({"ms": 5000})),
    )
    .await;

    assert!(dropped.is_err());
    assert!(gone_within(pid, Duration::from_secs(3)).await);
    assert_eq!(
        call(&session, "fx__echo", json!({}))
            .await
            .unwrap_err()
            .code,
        "extension_unavailable"
    );
}

// ---------- process lifecycle ----------

#[tokio::test]
async fn close_reaps_the_server_and_its_children() {
    let mut session = open(vec![fixture("fx")]).await;
    let pid = pid_of(&session, "fx").await;
    let grandchild: u32 = text(
        &call(&session, "fx__spawn_grandchild", json!({}))
            .await
            .unwrap(),
    )
    .parse()
    .unwrap();
    assert!(alive(grandchild));

    session.close().await.unwrap();

    assert!(!alive(pid));
    assert!(gone_within(grandchild, Duration::from_secs(3)).await);
    assert_eq!(
        call(&session, "fx__echo", json!({}))
            .await
            .unwrap_err()
            .code,
        "extension_unavailable"
    );
}

#[tokio::test]
async fn dropping_the_session_kills_the_process_tree() {
    let session = open(vec![fixture("fx")]).await;
    let pid = pid_of(&session, "fx").await;
    let grandchild: u32 = text(
        &call(&session, "fx__spawn_grandchild", json!({}))
            .await
            .unwrap(),
    )
    .parse()
    .unwrap();

    drop(session);

    assert!(gone_within(pid, Duration::from_secs(3)).await);
    assert!(gone_within(grandchild, Duration::from_secs(3)).await);
}

#[tokio::test]
async fn failed_startup_reaps_servers_already_started() {
    let log = log_file("partial");
    let configs = vec![
        fixture("ok").env("FIXTURE_LOG", &log),
        McpServerConfig::new("missing", "/nonexistent/mcp-server"),
    ];

    let err = McpToolProvider::new(configs)
        .open_session()
        .await
        .err()
        .unwrap();

    assert!(err.0.contains("missing"), "{err}");
    let pids = logged_pids(&log);
    assert_eq!(pids.len(), 1);
    assert!(gone_within(pids[0], Duration::from_secs(3)).await);
}

#[tokio::test]
async fn server_crash_kills_its_children_at_once() {
    let log = log_file("crash");
    let session = open(vec![fixture("fx").env("FIXTURE_LOG", &log)]).await;
    let server = pid_of(&session, "fx").await;

    call(&session, "fx__crash_later", json!({})).await.unwrap();

    // No further call and no close(): the supervisor alone must clean up.
    let grandchild: u32 = logged(&log)
        .iter()
        .find_map(|l| l.strip_prefix("grandchild:")?.parse().ok())
        .expect("fixture logged its child");
    assert!(
        gone_within(server, Duration::from_secs(3)).await,
        "server still running"
    );
    assert!(
        gone_within(grandchild, Duration::from_secs(3)).await,
        "child left behind"
    );
    drop(session);
}

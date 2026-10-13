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
    call_at(session, name, args, 0, &NoopSink, cancellation).await
}

/// Like `call_with`, but with the batch position and event sink under the test's control.
async fn call_at(
    session: &McpSession,
    name: &str,
    args: Value,
    index: usize,
    events: &dyn EventSink,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolFailure> {
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments: args,
    };
    let cx = ToolContext {
        cancellation,
        events,
        index,
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
        reasoning: Vec::new(),
        content: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }],
        provider_payload: None,
        stop_reason: StopReason::ToolUse,
        usage: None,
    }
}

fn answers(text: &str) -> ModelResponse {
    ModelResponse {
        reasoning: Vec::new(),
        content: vec![ContentPart::text(text)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
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
                Message::Tool { result, .. } => Some(result.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(result.call_id, "c1");
        assert!(!result.is_error);
        assert_eq!(result.text(), r#"{"hello":"mcp"}"#);
        chat.close().await.unwrap();

        // After the methods, `close()` shut the server down gracefully: its stdin closed and
        // it finished on its own, rather than being killed.
        let mut expected: Vec<&str> = expected_methods.clone();
        expected.extend(["stdin-closed", "shutdown-complete"]);
        let lines: Vec<String> = logged(&log)
            .into_iter()
            .filter(|l| !l.starts_with("pid:"))
            .collect();
        assert_eq!(lines, expected);
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
async fn the_provider_reports_what_the_opened_session_connected() {
    let provider = McpToolProvider::new(vec![fixture("fx")]);
    assert_eq!(
        provider.configured(),
        1,
        "configured is known before opening"
    );
    assert!(provider.status().is_none(), "nothing has opened yet");

    let mut session = provider.open_session().await.unwrap();
    let status = provider.status().expect("the session is open");
    assert_eq!(status.servers, 1);
    assert_eq!(
        status.tools,
        session.tool_specs().len(),
        "the status must report the tools that actually connected"
    );
    session.close().await.unwrap();
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
            Message::Tool { result, .. } => Some(result.text()),
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
async fn replacing_a_transcript_keeps_the_mcp_process_and_its_state() {
    let provider = McpToolProvider::new(vec![fixture("fx")]);
    let model = Scripted(Mutex::new(VecDeque::from([
        ModelResponse {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![
                ToolCall {
                    id: "pid-before".into(),
                    name: "fx__pid".into(),
                    arguments: json!({}),
                },
                ToolCall {
                    id: "counter-before".into(),
                    name: "fx__counter".into(),
                    arguments: json!({}),
                },
            ],
            provider_payload: None,
            stop_reason: StopReason::ToolUse,
            usage: None,
        },
        answers("before"),
        ModelResponse {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![
                ToolCall {
                    id: "pid-after".into(),
                    name: "fx__pid".into(),
                    arguments: json!({}),
                },
                ToolCall {
                    id: "counter-after".into(),
                    name: "fx__counter".into(),
                    arguments: json!({}),
                },
            ],
            provider_payload: None,
            stop_reason: StopReason::ToolUse,
            usage: None,
        },
        answers("after"),
    ])));
    let agent = Arc::new(Agent::new(model, provider));
    let mut chat = agent.open_session().await.unwrap();

    chat.run_turn("before", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let before_pid = chat
        .transcript()
        .iter()
        .find_map(|message| match message {
            Message::Tool { result, .. } if result.call_id == "pid-before" => Some(result.text()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        chat.transcript()
            .iter()
            .find_map(|message| match message {
                Message::Tool { result, .. } if result.call_id == "counter-before" => {
                    Some(result.text())
                }
                _ => None,
            })
            .as_deref(),
        Some("1")
    );

    chat.replace_transcript(vec![Message::user("replacement")], 23, &NoopSink)
        .await
        .unwrap();
    chat.run_turn("after", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(
        chat.transcript()
            .iter()
            .find_map(|message| match message {
                Message::Tool { result, .. } if result.call_id == "pid-after" =>
                    Some(result.text()),
                _ => None,
            })
            .as_deref(),
        Some(before_pid.as_str())
    );
    assert_eq!(
        chat.transcript()
            .iter()
            .find_map(|message| match message {
                Message::Tool { result, .. } if result.call_id == "counter-after" => {
                    Some(result.text())
                }
                _ => None,
            })
            .as_deref(),
        Some("2")
    );
    chat.close().await.unwrap();
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

// ---------- progress ----------

/// Collects the events of the calls it is handed.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<AgentEvent>>>);

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn progress_events(events: &Recorder) -> Vec<(usize, Value)> {
    events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolProgress { index, progress } => Some((*index, progress.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn progress_maps_to_tool_progress() {
    let session = open(vec![fixture("fx")]).await;
    let events = Recorder::default();

    let out = call_at(
        &session,
        "fx__progress",
        json!({"count": 3, "intervalMs": 10, "total": 3, "label": "step"}),
        2,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(text(&out), "progressed");
    // In order, tagged with the call's position in its batch, carrying what the server sent.
    assert_eq!(
        progress_events(&events),
        vec![
            (
                2,
                json!({"progress": 1.0, "total": 3.0, "message": "step 1"})
            ),
            (
                2,
                json!({"progress": 2.0, "total": 3.0, "message": "step 2"})
            ),
            (
                2,
                json!({"progress": 3.0, "total": 3.0, "message": "step 3"})
            ),
        ]
    );
}

// `settleMs` holds the reply back until every notification has been routed, so on a
// multi-thread runtime none may be lost; this excludes rmcp's best-effort delivery of tail
// progress. The 10 ms spacing keeps notifications from overlapping in flight: rmcp routes each
// on its own task, and a burst can arrive out of order, which the monotone filter drops by
// design.
//
// This does not reproduce the registration race (a notification that beats the call's route).
// That window is a few microseconds: with the replay removed, 2880 calls under 16 CPU-hogging
// threads lost nothing. The `ProgressRouter` unit tests cover the replay deterministically.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settled_progress_is_not_lost() {
    let session = std::sync::Arc::new(open(vec![fixture("fx")]).await);
    let mut lost = 0;
    let runs = 20;
    for _ in 0..runs {
        let events = Recorder::default();
        let out = call_at(
            &session,
            "fx__progress",
            json!({"count": 5, "intervalMs": 10, "settleMs": 50}),
            0,
            &events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(text(&out), "progressed");
        lost += 5 - progress_events(&events).len().min(5);
    }
    assert_eq!(
        lost, 0,
        "{lost} progress events were dropped over {runs} calls"
    );
}

// A server may send progress far faster than a call drains it. Each call keeps a bounded queue
// and drops the oldest when it is full (the bound itself is a `ProgressQueue` unit test), so the
// newest value must still arrive. `settleMs` gives the last notifications time to be routed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_progress_flood_still_delivers_the_latest_value() {
    let session = open(vec![fixture("fx")]).await;
    let events = Recorder::default();

    call_at(
        &session,
        "fx__progress",
        json!({"count": 3000, "intervalMs": 0, "settleMs": 200}),
        0,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let seen: Vec<f64> = progress_events(&events)
        .iter()
        .map(|(_, progress)| progress["progress"].as_f64().unwrap())
        .collect();
    assert_eq!(seen.last().copied(), Some(3000.0));
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "emission stays monotone"
    );
}

// A server that breaks the spec's monotone rule reaches the filter on the real call path in a
// deterministic order, which rmcp's own reordering never does. Removing the filter from the
// call must fail this test; the filter's unit tests alone cannot show that it is wired in.
#[tokio::test]
async fn non_monotone_progress_is_dropped_on_the_call_path() {
    let session = open(vec![fixture("fx")]).await;
    let events = Recorder::default();

    call_at(
        &session,
        "fx__progress_values",
        json!({"values": [1, 3, 2, 3, 4]}),
        0,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let seen: Vec<f64> = progress_events(&events)
        .iter()
        .map(|(_, progress)| progress["progress"].as_f64().unwrap())
        .collect();
    assert_eq!(seen, [1.0, 3.0, 4.0]);
}

#[tokio::test]
async fn concurrent_progress_calls_stay_separate() {
    let mut config = fixture("fx");
    config.shared_tools = vec!["progress".into()];
    let session = open(vec![config]).await;
    let events = Recorder::default();

    let (a, b) = tokio::join!(
        call_at(
            &session,
            "fx__progress",
            json!({"count": 4, "intervalMs": 40, "label": "a"}),
            0,
            &events,
            CancellationToken::new(),
        ),
        call_at(
            &session,
            "fx__progress",
            json!({"count": 4, "intervalMs": 40, "label": "b"}),
            1,
            &events,
            CancellationToken::new(),
        )
    );

    assert!(a.is_ok() && b.is_ok());
    let recorded = progress_events(&events);
    assert_eq!(recorded.len(), 8);
    for (index, label) in [(0, "a"), (1, "b")] {
        let messages: Vec<String> = recorded
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, p)| p["message"].as_str().unwrap().to_owned())
            .collect();
        let expected: Vec<String> = (1..=4).map(|step| format!("{label} {step}")).collect();
        assert_eq!(messages, expected);
    }
}

#[tokio::test]
async fn late_and_unknown_progress_tokens_are_ignored() {
    let session = open(vec![fixture("fx")]).await;
    let events = Recorder::default();

    // One notification under a token no call ever carried, before the real ones.
    let out = call_at(
        &session,
        "fx__progress",
        json!({"count": 2, "intervalMs": 10, "label": "real", "alsoUnknownToken": true}),
        0,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(text(&out), "progressed");
    assert_eq!(
        progress_events(&events),
        vec![
            (0, json!({"progress": 1.0, "message": "real 1"})),
            (0, json!({"progress": 2.0, "message": "real 2"})),
        ]
    );

    // The notification the server still sends after answering is dropped, not delivered.
    let late = Recorder::default();
    let out = call_at(
        &session,
        "fx__progress_late",
        json!({}),
        0,
        &late,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(text(&out), "answered first");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        progress_events(&late).is_empty(),
        "{:?}",
        progress_events(&late)
    );

    // Neither hurt the session.
    assert_eq!(
        text(&call(&session, "fx__echo", json!({"x": 1})).await.unwrap()),
        r#"{"x":1}"#
    );
}

#[tokio::test]
async fn progress_does_not_reset_the_call_deadline() {
    let mut config = fixture("fx");
    config.call_timeout = Duration::from_millis(300);
    let session = open(vec![config]).await;
    let events = Recorder::default();
    let start = Instant::now();

    // Twenty notifications at 100 ms apiece would run 2 s if each one reset the deadline.
    let failure = call_at(
        &session,
        "fx__progress",
        json!({"count": 20, "intervalMs": 100, "label": "x"}),
        0,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.code, "extension_timeout");
    assert!(!progress_events(&events).is_empty());
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "deadline was pushed back by progress: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn settled_calls_leave_no_progress_registration_behind() {
    // Success: one route while in flight, none after.
    let session = std::sync::Arc::new(open(vec![fixture("fx")]).await);
    let events = Recorder::default();
    let inflight = {
        let session = std::sync::Arc::clone(&session);
        let events = events.clone();
        tokio::spawn(async move {
            call_at(
                &session,
                "fx__progress",
                json!({"count": 3, "intervalMs": 60, "label": "ok"}),
                0,
                &events,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(session.progress_route_count(), 1);
    inflight.await.unwrap();
    assert_eq!(session.progress_route_count(), 0);

    // Cancelled mid-progress.
    let session = open(vec![fixture("fx")]).await;
    let events = Recorder::default();
    let token = CancellationToken::new();
    let trigger = token.clone();
    let (result, ()) = tokio::join!(
        call_at(
            &session,
            "fx__progress",
            json!({"count": 100, "intervalMs": 50, "label": "x"}),
            0,
            &events,
            token,
        ),
        async {
            tokio::time::sleep(Duration::from_millis(120)).await;
            trigger.cancel();
        }
    );
    assert_eq!(result.unwrap_err().code, "cancelled");
    assert!(!progress_events(&events).is_empty());
    assert_eq!(session.progress_route_count(), 0);

    // Timed out mid-progress.
    let mut config = fixture("fx");
    config.call_timeout = Duration::from_millis(150);
    let session = open(vec![config]).await;
    let events = Recorder::default();
    let failure = call_at(
        &session,
        "fx__progress",
        json!({"count": 100, "intervalMs": 50, "label": "x"}),
        0,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "extension_timeout");
    assert!(!progress_events(&events).is_empty());
    assert_eq!(session.progress_route_count(), 0);

    // Dropped mid-progress: the drop itself must unregister, synchronously.
    let session = open(vec![fixture("fx")]).await;
    let dropped = tokio::time::timeout(
        Duration::from_millis(120),
        call_at(
            &session,
            "fx__progress",
            json!({"count": 100, "intervalMs": 50, "label": "x"}),
            0,
            &Recorder::default(),
            CancellationToken::new(),
        ),
    )
    .await;
    assert!(dropped.is_err());
    assert_eq!(session.progress_route_count(), 0);

    // None of it shows up later: a fresh call maps progress exactly as before.
    let session = open(vec![fixture("fx")]).await;
    let events = Recorder::default();
    call_at(
        &session,
        "fx__progress",
        json!({"count": 2, "intervalMs": 10, "label": "after"}),
        0,
        &events,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        progress_events(&events),
        vec![
            (0, json!({"progress": 1.0, "message": "after 1"})),
            (0, json!({"progress": 2.0, "message": "after 2"})),
        ]
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

//! ADR 0030 slice 6 part a: the lead's task tools.
//!
//! Group A drives [`LeadTools`]/[`run_lead_tool`] over a real [`TaskRegistry`] (a mock model
//! server only where a task turn must run). Group B opens real sessions: the `/mode` gate, and
//! the source rule (a lead turn stays `User`; a task's turn is `Subagent`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_core::{
    CancellationToken, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput, ToolProvider,
    TurnOptions,
};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    ApprovalRouting, HostBindings, LeadSession, LeadTools, LocalSession, SessionError,
    SessionFacade, SessionId, SessionProjection, SessionQueue, SessionSetup, SpawnRequest,
    TaskBuilder, TaskContext, TaskRegistry, TaskSession, TurnState, build_model, open_session,
    run_lead_tool,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-lead-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A private `RADIAN_HOME` for this test binary (the task worktree root resolves through it).
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-lead-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any registry is built.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn workspace_sandbox(ws: &Path) -> Option<Sandbox> {
    Sandbox::from_policy(SandboxPolicy::workspace_write(ws).ok()?).ok()
}

fn model(
    session_id: &str,
    base_url: &str,
    slot: Arc<radian_runtime::ConfigSlot>,
) -> radian_host::SwitchableModel {
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: base_url.to_owned(),
        model: "mock".into(),
        api_key: Some("sk-test".into()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
    })
    .unwrap();
    radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        slot,
        radian_host::RouteContext {
            providers: std::collections::BTreeMap::new(),
            session_id: session_id.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    )
}

/// The task builder a test gives the registry: a mock model, and `shared` tasks in the lead's
/// workspace forced to `safe`. `mount_lead_in_tasks` deliberately passes a task builder back so
/// the depth gate has something to refuse.
fn builder(session_dir: PathBuf, base_url: String, mount_lead_in_tasks: bool) -> TaskBuilder {
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(Value::Null);
            let sandbox = workspace_sandbox(&workspace)
                .ok_or_else(|| fail("no sandbox backend here".into()))?;
            let task_builder: Option<TaskBuilder> = if mount_lead_in_tasks {
                Some(Arc::new(|_ctx: &TaskContext<'_>| {
                    Err(SessionError::Message("nested task".into()))
                }))
            } else {
                None
            };
            Ok(TaskSession {
                setup: SessionSetup {
                    session_id: SessionId::new(ctx.session_id),
                    session_dir: session_dir.clone(),
                    workspace: workspace.clone(),
                    coding_root: Some(workspace.clone()),
                    model: model(ctx.session_id, &base_url, Arc::clone(&slot)),
                    config_slot: slot,
                    config_queue: radian_runtime::ConfigQueue::new(),
                    budget_for: None,
                    price_for: None,
                    protected_branches: Vec::new(),
                    allowed_filters: None,
                    servers: Vec::new(),
                    role: None,
                    system_prompt: None,
                    context_budget: None,
                    auto_compact: false,
                    compaction_settings: Value::Null,
                    compaction_settings_for: None,
                    retries: 0,
                    sandbox: Some(sandbox),
                    tier: Preset::WorkspaceWrite,
                    profile: Profile::workspace_write(&workspace)
                        .map_err(|e| fail(e.to_string()))?
                        .with_approval(ApprovalPolicy::Auto),
                    access_builder: None,
                    task_agent: false,
                    parent: None,
                    allowed_branch: None,
                    memory_root: None,
                    skills_extra_paths: Vec::new(),
                    skills_builtin: true,
                    skills_disabled: Vec::new(),
                    memory_injection: radian_host::memory::InjectionConfig::default(),
                    trust: radian_host::trust::Setup::untrusted(&workspace, false),
                    resume: false,
                    config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
                    turn_options: TurnOptions::default(),
                    turn_timeout: None,
                    wording: radian_host::CLI_WORDING,
                    capabilities: radian_host::SessionCapability::ALL.to_vec(),
                },
                bindings: HostBindings {
                    approvals: ApprovalRouting::Queued,
                    shared_queue: None,
                    ask_user: true,
                    audit: Arc::new(|_journal| {
                        Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
                    }),
                    events: Arc::new(NoopSink),
                    projection: Arc::new(SessionProjection::new()),
                    runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
                    driver_stopped: Arc::new(|_| {}),
                    before_driver: None,
                    memory_warning: Arc::new(|_message: &str| {}),
                    memory_notice_line: Arc::new(|_line: &str| {}),
                    task_builder,
                },
            })
        },
    )
}

/// A registry over a fresh git lead workspace.
struct Fixture {
    scratch: PathBuf,
    lead: PathBuf,
    session_dir: PathBuf,
    registry: Arc<TaskRegistry>,
}

impl Fixture {
    fn new(label: &str, base_url: &str) -> Self {
        Self::with_task_builder(label, base_url, false)
    }

    fn with_task_builder(label: &str, base_url: &str, mount_lead_in_tasks: bool) -> Self {
        let _ = isolated_home();
        let scratch = scratch(label);
        let lead = scratch.join("lead");
        std::fs::create_dir_all(&lead).unwrap();
        assert!(git(&lead, &["init", "-q", "-b", "main"]));
        assert!(git(
            &lead,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ]
        ));
        let session_dir = scratch.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let lead_journal = Arc::new(
            SessionJournal::create(
                session_dir.join("lead.jsonl"),
                "lead",
                SessionCreatedData::new(lead.to_string_lossy(), json!({})),
            )
            .expect("the lead journal"),
        );
        let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
        let registry = Arc::new(TaskRegistry::new(
            lead.clone(),
            "lead",
            lead_journal,
            Arc::clone(&queue),
            builder(
                session_dir.clone(),
                base_url.to_owned(),
                mount_lead_in_tasks,
            ),
        ));
        Self {
            scratch,
            lead,
            session_dir,
            registry,
        }
    }

    async fn lead_session(&self) -> LeadSession {
        LeadTools::new(Arc::clone(&self.registry))
            .open_session()
            .await
            .expect("a lead tool session")
    }

    fn lead_journal_text(&self) -> String {
        std::fs::read_to_string(self.session_dir.join("lead.jsonl")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Calls one lead tool with a throwaway context.
async fn run_tool(
    session: &LeadSession,
    name: &str,
    arguments: Value,
) -> Result<ToolOutput, ToolFailure> {
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    let call = ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments,
    };
    run_lead_tool(session, &call, cx).await
}

fn structured(output: &ToolOutput) -> Value {
    output.structured.clone().expect("structured output")
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

/// An assistant turn with several tool calls.
fn tools_reply(calls: &[(&str, &str, &str)]) -> Reply {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({"index": index, "id": id, "type": "function",
                   "function": {"name": name, "arguments": arguments}})
        })
        .collect();
    Reply::sse(&[
        chunk(json!({"tool_calls": calls}), None),
        chunk(json!({}), Some("tool_calls")),
    ])
}

// ---------------------------------------------------------------------------
// Group A: the tools over a registry
// ---------------------------------------------------------------------------

/// `spawn_task` returns the id and where it runs.
#[tokio::test]
async fn spawn_task_returns_the_id_and_where_it_runs() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("spawn", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return; // no sandbox backend here
    }
    let session = fixture.lead_session().await;
    let output = run_tool(
        &session,
        "spawn_task",
        json!({"id": "t1", "brief": "do it"}),
    )
    .await
    .expect("spawn");
    let value = structured(&output);
    assert_eq!(value["task"], "t1");
    assert_eq!(value["isolation"], "worktree");
    assert_eq!(value["branch"], "radian/t1");
    assert!(
        value["worktree"].as_str().unwrap().ends_with("t1"),
        "{value}"
    );
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// The argument limits: a bad `isolation`/`approval` is refused, and above all `spawn_task`
/// takes **no** `policy` (ADR 0030 item 3).
#[tokio::test]
async fn spawn_task_refuses_bad_arguments_and_has_no_policy_field() {
    let fixture = Fixture::new("args", "http://127.0.0.1:1/v1");
    let session = fixture.lead_session().await;
    for (label, arguments) in [
        ("isolation", json!({"brief": "x", "isolation": "yolo"})),
        ("approval", json!({"brief": "x", "approval": "always"})),
        ("policy", json!({"brief": "x", "policy": "/bin/sh"})),
        ("no-brief", json!({"id": "t1"})),
    ] {
        let error = run_tool(&session, "spawn_task", arguments)
            .await
            .expect_err(label);
        assert_eq!(error.code, "invalid_arguments", "{label}: {error:?}");
    }
}

/// The wait result: a task that asks returns the question; `answer_task` resolves it and the
/// task finishes (the next `wait_task` reports `finished`).
#[tokio::test]
async fn wait_task_returns_a_question_then_the_finish() {
    let server = MockServer::start(vec![
        tool_reply(
            "call_q",
            "ask_user",
            r#"{"questions":[{"id":"q1","prompt":"Which branch?","kind":"text"}]}"#,
        ),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("ask", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");

    let output = run_tool(
        &session,
        "wait_task",
        json!({"id": "t1", "timeout_secs": 20}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "question", "{value}");
    assert_eq!(value["task"], "t1");
    assert_eq!(value["questions"][0]["prompt"], "Which branch?");
    let request_id = value["request_id"].as_str().unwrap().to_owned();

    run_tool(
        &session,
        "answer_task",
        json!({"id": "t1", "request_id": request_id, "answers": [{"id": "q1", "text": "main"}]}),
    )
    .await
    .expect("answer");

    let output = run_tool(
        &session,
        "wait_task",
        json!({"id": "t1", "timeout_secs": 20}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "finished", "{value}");
    assert_eq!(value["summary"], "done");
    // Nothing a task raises goes to the lead's inbox (ADR 0030).
    assert!(
        !fixture.lead_journal_text().contains("InboxEnqueued"),
        "the lead's inbox must stay empty"
    );
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A timeout returns `still_running` and never cancels the task.
#[tokio::test]
async fn a_timeout_returns_still_running_and_keeps_the_task() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("timeout", &server.base_url);
    let session = fixture.lead_session().await;
    // No brief: the task never runs a turn, so nothing can finish or ask.
    fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let output = run_tool(
        &session,
        "wait_task",
        json!({"id": "t1", "timeout_secs": 0.02}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "still_running", "{value}");
    assert_eq!(value["timeout_secs"], 0.02);
    let record = fixture.registry.get("t1").expect("the task is untouched");
    assert!(!record.running(), "the timeout must not leave it running");
    assert!(record.last_outcome().is_none(), "no turn ended");

    // The task was not cancelled: it still runs a turn when it is given one.
    record
        .session
        .inbox()
        .followup(
            agent_core::Message::user("late"),
            agent_core::InputSource::Subagent,
        )
        .await
        .expect("the task's inbox still works");
    wait_requests(&server, 1).await;
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A negative `timeout_secs` is a bad-arguments failure, on both waits.
#[tokio::test]
async fn a_bad_timeout_is_refused() {
    let fixture = Fixture::new("bad-timeout", "http://127.0.0.1:1/v1");
    let session = fixture.lead_session().await;
    fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let error = run_tool(
        &session,
        "wait_task",
        json!({"id": "t1", "timeout_secs": -1.0}),
    )
    .await
    .expect_err("a negative timeout must be refused");
    assert_eq!(error.code, "invalid_arguments", "{error:?}");
    let error = run_tool(
        &session,
        "wait_any",
        json!({"ids": ["t1"], "timeout_secs": -1.0}),
    )
    .await
    .expect_err("a negative timeout must be refused");
    assert_eq!(error.code, "invalid_arguments", "{error:?}");
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A huge `timeout_secs` must not panic the host: the deadline is clamped to 24 hours, so the
/// wait still ends when the task finishes (and still respects cancellation while waiting).
#[tokio::test]
async fn a_huge_timeout_is_clamped_and_does_not_panic() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let fixture = Fixture::new("huge-timeout", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;

    // `wait_task` with a value that would overflow `Instant` on the old code.
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");
    let output = run_tool(
        &session,
        "wait_task",
        json!({"id": "t1", "timeout_secs": 1e19}),
    )
    .await
    .expect("wait");
    assert_eq!(structured(&output)["event"], "finished");

    // `wait_any` too.
    let mut request = SpawnRequest::new("t2");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");
    let output = run_tool(
        &session,
        "wait_any",
        json!({"ids": ["t1", "t2"], "timeout_secs": 1e19}),
    )
    .await
    .expect("wait");
    assert_eq!(structured(&output)["event"], "finished");

    // A task that never finishes: the huge-timeout wait keeps waiting and still respects the
    // turn's cancellation.
    fixture
        .registry
        .spawn(SpawnRequest::new("t3"))
        .await
        .expect("spawn");
    let session = Arc::new(session);
    let token = CancellationToken::new();
    let cancel = token.clone();
    let handle = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            let sink = NoopSink;
            let cx = ToolContext {
                cancellation: token,
                events: &sink,
                index: 0,
            };
            let call = ToolCall {
                id: "call-huge".into(),
                name: "wait_task".into(),
                arguments: json!({"id": "t3", "timeout_secs": 1e19}),
            };
            run_lead_tool(&session, &call, cx).await
        }
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the wait must end on cancellation")
        .unwrap()
        .expect_err("cancelled");
    assert_eq!(error.code, "cancelled", "{error:?}");

    fixture.registry.remove("t1", true).await.ok();
    fixture.registry.remove("t2", true).await.ok();
    fixture.registry.remove("t3", true).await.ok();
}

/// `wait_any` requires a non-empty `ids` and every id must exist before it waits.
#[tokio::test]
async fn wait_any_requires_a_known_non_empty_id_set() {
    let fixture = Fixture::new("any-ids", "http://127.0.0.1:1/v1");
    let session = fixture.lead_session().await;
    fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let error = run_tool(&session, "wait_any", json!({"ids": []}))
        .await
        .expect_err("an empty id set must be refused");
    assert_eq!(error.code, "invalid_arguments", "{error:?}");
    let error = run_tool(&session, "wait_any", json!({"ids": ["t1", "ghost"]}))
        .await
        .expect_err("an unknown id must be refused before waiting");
    assert_eq!(error.code, "no_such_task", "{error:?}");
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// `wait_any` watches only its `ids`: a question from a task outside the set stays pending.
#[tokio::test]
async fn wait_any_ignores_a_task_not_in_ids() {
    let server = MockServer::start(vec![
        tool_reply(
            "call_q",
            "ask_user",
            r#"{"questions":[{"id":"q1","prompt":"Go?","kind":"text"}]}"#,
        ),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("any-filter", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    // t1 never runs (no brief); t2 asks.
    fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let mut request = SpawnRequest::new("t2");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");
    // Wait until t2's question is pending (peek does not take it).
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while fixture.registry.pending().peek().is_none() {
        assert!(std::time::Instant::now() < deadline, "t2 never asked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // t1 only: t2's question is not an event for this wait, so it times out and stays pending.
    let output = run_tool(
        &session,
        "wait_any",
        json!({"ids": ["t1"], "timeout_secs": 0.2}),
    )
    .await
    .expect("wait");
    assert_eq!(structured(&output)["event"], "still_running");
    let pending = fixture
        .registry
        .pending()
        .peek()
        .expect("t2's question is still pending");
    assert_eq!(pending.task_id, "t2");

    // The later wait that names t2 takes it.
    let output = run_tool(
        &session,
        "wait_any",
        json!({"ids": ["t2"], "timeout_secs": 20}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "question", "{value}");
    assert_eq!(value["task"], "t2");
    fixture.registry.remove("t1", true).await.ok();
    fixture.registry.remove("t2", true).await.ok();
}

/// `wait_any` returns a task that asks and leaves the other untouched.
#[tokio::test]
async fn wait_any_returns_a_question_and_leaves_the_others() {
    let server = MockServer::start(vec![
        tool_reply(
            "call_q",
            "ask_user",
            r#"{"questions":[{"id":"q1","prompt":"Go?","kind":"text"}]}"#,
        ),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("any-ask", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");
    fixture
        .registry
        .spawn(SpawnRequest::new("t2"))
        .await
        .expect("spawn");

    let output = run_tool(
        &session,
        "wait_any",
        json!({"ids": ["t1", "t2"], "timeout_secs": 20}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "question", "{value}");
    assert_eq!(value["task"], "t1");
    let request_id = value["request_id"].as_str().unwrap().to_owned();
    assert!(fixture.registry.get("t2").is_some(), "t2 is untouched");
    run_tool(
        &session,
        "answer_task",
        json!({"id": "t1", "request_id": request_id, "answers": [{"id": "q1", "text": "go"}]}),
    )
    .await
    .expect("answer");
    fixture.registry.remove("t1", true).await.ok();
    fixture.registry.remove("t2", true).await.ok();
}

/// `wait_any` returns the first task to finish its turn.
#[tokio::test]
async fn wait_any_returns_the_first_finisher() {
    let server = MockServer::start(vec![text_reply("one")]).await;
    let fixture = Fixture::new("any-finish", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");

    let output = run_tool(
        &session,
        "wait_any",
        json!({"ids": ["t1"], "timeout_secs": 20}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "finished", "{value}");
    assert_eq!(value["task"], "t1");
    assert_eq!(value["outcome"], "completed");
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A summary over `TASK_SUMMARY_LIMIT_CHARS` is cut and says so (ADR 0030 R2).
#[tokio::test]
async fn a_long_summary_is_cut_and_says_so() {
    let long = "z".repeat(radian_host::TASK_SUMMARY_LIMIT_CHARS + 500);
    let server = MockServer::start(vec![text_reply(&long)]).await;
    let fixture = Fixture::new("bounded", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");

    let output = run_tool(
        &session,
        "wait_task",
        json!({"id": "t1", "timeout_secs": 20}),
    )
    .await
    .expect("wait");
    let value = structured(&output);
    assert_eq!(value["event"], "finished", "{value}");
    assert_eq!(value["summary_truncated"], true);
    let summary = value["summary"].as_str().unwrap();
    assert!(
        summary.chars().count() <= radian_host::TASK_SUMMARY_LIMIT_CHARS + 16,
        "the summary must be bounded"
    );
    assert!(summary.contains("[truncated]"), "{summary}");
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// `task_status` and `list_tasks` read the observer's committed facts and carry a bounded
/// change summary; `list_tasks` lists every task.
#[tokio::test]
async fn status_and_list_report_every_task() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("status", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");

    let status = structured(
        &run_tool(&session, "task_status", json!({"id": "t1"}))
            .await
            .unwrap(),
    );
    assert_eq!(status["task"], "t1");
    assert!(status["state"].is_string(), "{status}");
    let list = structured(&run_tool(&session, "list_tasks", json!({})).await.unwrap());
    let tasks = list["tasks"].as_array().expect("a tasks array");
    assert_eq!(tasks.len(), 1, "{list}");
    assert_eq!(tasks[0]["task"], "t1");
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// `cancel_task` stops the current turn; `remove_task` closes the task. A second `cancel_task`
/// is idempotent; `remove_task` on an unknown id is a tool error.
#[tokio::test]
async fn cancel_and_remove_behave() {
    let fixture = Fixture::new("cancel", "http://127.0.0.1:1/v1");
    let session = fixture.lead_session().await;
    fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    run_tool(&session, "cancel_task", json!({"id": "t1"}))
        .await
        .expect("cancel");
    run_tool(&session, "cancel_task", json!({"id": "t1"}))
        .await
        .expect("cancel is idempotent");
    run_tool(&session, "remove_task", json!({"id": "t1"}))
        .await
        .expect("remove");
    let error = run_tool(&session, "remove_task", json!({"id": "t1"}))
        .await
        .expect_err("unknown");
    assert_eq!(error.code, "remove_failed");
}

// ---------------------------------------------------------------------------
// Group B: real sessions — the mode gate and the source rule
// ---------------------------------------------------------------------------

/// The lead tools are mounted from the start and gated by `/mode`: `general` denies a call with
/// a "switch with /mode lead" hint, `lead` allows it, and the tool list is the same in both
/// modes (ADR 0030 item 5).
#[tokio::test]
async fn the_mode_gate_denies_in_general_and_allows_in_lead_with_one_tool_list() {
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "spawn_task",
            r#"{"brief":"x","isolation":"shared"}"#,
        ),
        text_reply("denied"),
        tool_reply("call_2", "list_tasks", "{}"),
        text_reply("allowed"),
    ])
    .await;
    let scratch = scratch("mode-gate");
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session = open_lead(&scratch, "mode", &server.base_url, Some(dummy_builder())).await;

    send(&session, "first").await;
    wait_turn_end(&session, &server, 2).await;

    // The tool is in the list from the start; the call was refused with the /mode hint.
    let names = tool_names(&server, 0);
    assert!(names.iter().any(|name| name == "spawn_task"), "{names:?}");
    let result = tool_result(&server, 1, "call_1");
    assert!(result.contains("/mode lead"), "{result}");

    session
        .set_mode(radian_host::Mode::Lead)
        .expect("general -> lead");
    send(&session, "second").await;
    wait_turn_end(&session, &server, 4).await;

    // The same tool list, and now the call is allowed.
    assert_eq!(tool_names(&server, 2), names, "the tool list must not move");
    let result = tool_result(&server, 3, "call_2");
    assert!(result.contains("task(s)"), "{result}");

    session.stop();
    let _ = session.join().await;
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A task session never mounts the lead tools, even when its builder passes a task builder back
/// (depth limit 2, ADR 0030 O13).
#[tokio::test]
async fn a_task_session_gets_no_lead_tools() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::with_task_builder("depth", &server.base_url, true);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    fixture.registry.spawn(request).await.expect("spawn");

    // The task's own model request must not carry a lead tool.
    wait_requests(&server, 1).await;
    let names = tool_names(&server, 0);
    for lead in radian_host::LEAD_TOOL_NAMES {
        assert!(
            !names.iter().any(|name| name == lead),
            "{lead} in {names:?}"
        );
    }
    fixture.registry.remove("t1", true).await.expect("remove");
}

/// The source rule: a **user** lead turn that calls `spawn_task` and `wait_any` keeps `User`
/// authority, so a later `ask` still pops the human approval; the task's own turn is
/// `Subagent` (ADR 0030 O4, revision 6).
#[tokio::test]
async fn a_lead_turn_stays_user_and_a_task_turn_is_subagent() {
    let server = MockServer::start(vec![
        // The lead's first turn asks for a task and waits for it. `wait_any` blocks the lead
        // until the task finishes, so the task's next request is served before the lead's.
        tools_reply(&[
            (
                "call_1",
                "spawn_task",
                r#"{"id":"t1","brief":"do it","isolation":"shared"}"#,
            ),
            ("call_2", "wait_any", r#"{"ids":["t1"],"timeout_secs":20}"#),
        ]),
        text_reply("task done"),
        tool_reply("call_3", "shell", r#"{"command":"echo hi"}"#),
        text_reply("all done"),
    ])
    .await;
    let scratch = scratch("source");
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    assert!(git(&workspace, &["init", "-q", "-b", "main"]));
    assert!(git(
        &workspace,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]
    ));
    if workspace_sandbox(&workspace).is_none() {
        let _ = std::fs::remove_dir_all(&scratch);
        return;
    }
    let session =
        open_lead_workspace(&scratch, "source", &server.base_url, workspace.clone()).await;
    send(&session, "start").await;

    // The lead's shell call needs an approval: it can only reach the human queue if the turn is
    // still a `User` turn. A lowered (non-human) turn would be denied without a prompt.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let pending = loop {
        if let Some(head) = session.pending().head() {
            break head;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the lead's ask never reached the human queue"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    match &pending.request {
        radian_host::PendingRequest::Approval(request) => assert_eq!(request.tool, "shell"),
        other => panic!("expected an approval, got {other:?}"),
    }
    assert_eq!(
        session.hooks().turn_source(),
        Some(agent_core::InputSource::User),
        "task events must not lower the lead's turn source"
    );
    session
        .answer(
            pending.request_id,
            radian_host::PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
        )
        .expect("answer the approval");
    wait_turn_end(&session, &server, 4).await;

    // The task's own turn ran as `Subagent`.
    let record = session.lead_tasks().unwrap().get("t1").expect("the task");
    assert_eq!(
        record.session.hooks().turn_source(),
        Some(agent_core::InputSource::Subagent),
        "a task agent's turn is Subagent"
    );
    // Nothing task-related entered the lead's inbox: only the user's own message is there, and
    // no `subagent` input at all (ADR 0030).
    let journal = std::fs::read_to_string(scratch.join("source.jsonl")).unwrap();
    assert!(
        !journal.contains("\"subagent\""),
        "a task must never enqueue into the lead's inbox:\n{journal}"
    );

    session.stop();
    let _ = session.join().await;
    let _ = std::fs::remove_dir_all(&scratch);
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

/// A task builder that is never called (the mode test denies `spawn_task` before it runs; the
/// source test supplies its own).
fn dummy_builder() -> TaskBuilder {
    Arc::new(|_ctx: &TaskContext<'_>| Err(SessionError::Message("unused".into())))
}

async fn open_lead(
    scratch: &Path,
    label: &str,
    base_url: &str,
    task_builder: Option<TaskBuilder>,
) -> Arc<LocalSession> {
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    open_lead_inner(
        scratch,
        label,
        base_url,
        &workspace,
        Preset::Safe,
        None,
        task_builder,
        false,
    )
    .await
}

async fn open_lead_workspace(
    scratch: &Path,
    label: &str,
    base_url: &str,
    workspace: PathBuf,
) -> Arc<LocalSession> {
    let sandbox = workspace_sandbox(&workspace).expect("checked");
    open_lead_inner(
        scratch,
        label,
        base_url,
        &workspace,
        Preset::WorkspaceWrite,
        Some(sandbox),
        Some(builder(scratch.to_path_buf(), base_url.to_owned(), false)),
        // The source-rule test runs its lead turn as `lead` from the first request, so the
        // lead tools are usable without a mid-test `/mode` (the gate reads the slot).
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // a fixture helper; the caller assembles the setup
async fn open_lead_inner(
    scratch: &Path,
    label: &str,
    base_url: &str,
    workspace: &Path,
    tier: Preset,
    sandbox: Option<Sandbox>,
    task_builder: Option<TaskBuilder>,
    mode_lead: bool,
) -> Arc<LocalSession> {
    let initial = if mode_lead {
        json!({"mode": "lead"})
    } else {
        serde_json::Value::Null
    };
    let slot = radian_runtime::ConfigSlot::new(initial);
    let profile = match tier {
        Preset::Safe => Profile::safe(workspace),
        _ => Profile::workspace_write(workspace),
    }
    .unwrap()
    .with_approval(ApprovalPolicy::OnAsk);
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: scratch.to_path_buf(),
            workspace: workspace.to_path_buf(),
            coding_root: Some(workspace.to_path_buf()),
            model: model(label, base_url, Arc::clone(&slot)),
            config_slot: slot,
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            protected_branches: Vec::new(),
            allowed_filters: None,
            servers: Vec::new(),
            role: None,
            system_prompt: None,
            context_budget: None,
            auto_compact: false,
            compaction_settings: serde_json::Value::Null,
            compaction_settings_for: None,
            retries: 0,
            sandbox,
            tier,
            profile,
            access_builder: None,
            task_agent: false,
            parent: None,
            allowed_branch: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            memory_injection: radian_host::memory::InjectionConfig::default(),
            trust: radian_host::trust::Setup::untrusted(workspace, false),
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: radian_host::SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: ApprovalRouting::Queued,
            shared_queue: None,
            ask_user: false,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            memory_notice_line: Arc::new(|_line: &str| {}),
            task_builder,
        },
    )
    .await
    .expect("open");
    Arc::new(session)
}

async fn send(session: &Arc<LocalSession>, text: &str) {
    let session = Arc::clone(session);
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        session.command(radian_host::SessionCommand::SendFollowup(
            agent_core::Message::user(text),
        ))
    })
    .await
    .unwrap()
    .expect("the session declares `send`");
}

/// Waits until the mock saw at least `want` requests.
async fn wait_requests(server: &MockServer, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while server.requests().len() < want {
        assert!(
            std::time::Instant::now() < deadline,
            "no request reached the mock"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_turn_end(session: &Arc<LocalSession>, server: &MockServer, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let idle = session.projection().read().turn() == TurnState::Idle;
        if idle && server.requests().len() >= want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish: {} requests",
            server.requests().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The tool names in the `n`-th recorded request.
fn tool_names(server: &MockServer, n: usize) -> Vec<String> {
    server.requests()[n].json()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The text of the tool result for `call_id` in the `n`-th recorded request.
fn tool_result(server: &MockServer, n: usize, call_id: &str) -> String {
    server.requests()[n].json()["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

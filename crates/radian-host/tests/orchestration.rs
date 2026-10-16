//! ADR 0030 slice 6 part b1: `max_tasks` queueing and the `stalled` watch, over a real
//! registry. The heavy gate's own behavior is tested in `radian-coding`; here it is only
//! installed and shared. Everything is deterministic: temp repos, a mock model only where a
//! turn must run, and a fake clock for the stall check (no 600 s sleep).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent_core::{NoopSink, ToolProvider, TurnOptions};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use chrono::{DateTime, Utc};
use radian_host::{
    HostBindings, LeadTools, OrchestrationLimits, SessionError, SessionId, SessionProjection,
    SessionQueue, SessionSetup, SpawnRequest, TaskBuilder, TaskContext, TaskHandle, TaskRegistry,
    TaskSession, build_model, run_lead_tool,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::SessionCreatedData;
use serde_json::{Value, json};

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-orch-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-orch-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: set once, before any registry is built in this binary.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn workspace_sandbox(ws: &Path) -> Option<Sandbox> {
    let policy = SandboxPolicy::workspace_write(ws).ok()?;
    Sandbox::from_policy(policy).ok()
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

/// The builder a test gives the registry. It asserts the `max_tasks` cap in the seam (the
/// `live` counter) and decrements it when a task's driver ends (a removed task frees a slot).
fn builder(
    session_dir: PathBuf,
    base_url: String,
    live: Arc<AtomicUsize>,
    max_tasks: usize,
) -> TaskBuilder {
    // A session decrements `live` once, at whichever comes first: its first turn boundary or its
    // driver stopping (a removal). That is exactly a `max_tasks` slot being freed.
    let ended: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let previous = live.fetch_add(1, Ordering::SeqCst);
            assert!(
                previous < max_tasks,
                "the task cap was exceeded: {previous} live (max {max_tasks})"
            );
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(Value::Null);
            let sandbox = workspace_sandbox(&workspace)
                .ok_or_else(|| fail("no sandbox backend here".into()))?;
            let session_id = ctx.session_id.to_owned();
            let free = {
                let live = Arc::clone(&live);
                let ended = Arc::clone(&ended);
                let session_id = session_id.clone();
                move || {
                    if ended
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(session_id.clone())
                    {
                        live.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            };
            let on_stop = {
                let free = free.clone();
                Arc::new(move |_| free())
            };
            let free_events = free.clone();
            let runtime_events: Arc<dyn radian_runtime::RuntimeEvents> =
                Arc::new(move |event: &radian_session::RuntimeEvent| {
                    if matches!(
                        event,
                        radian_session::RuntimeEvent::TurnFinished { .. }
                            | radian_session::RuntimeEvent::TurnFailed { .. }
                    ) {
                        free_events();
                    }
                });
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
                    shell_auto_background: None,
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
                    git_commit_requires_branch: false,
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
                    tools_allow: None,
                    skills_allow: None,
                },
                bindings: HostBindings {
                    approvals: radian_host::ApprovalRouting::Queued,
                    shared_queue: None,
                    ask_user: true,
                    audit: Arc::new(|_journal| {
                        Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
                    }),
                    events: Arc::new(NoopSink),
                    projection: Arc::new(SessionProjection::new()),
                    runtime_events,
                    driver_stopped: on_stop,
                    before_driver: None,
                    memory_warning: Arc::new(|_message: &str| {}),
                    memory_notice_line: Arc::new(|_line: &str| {}),
                    task_builder: None,
                    named_builder: None,
                },
            })
        },
    )
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// A recorded hook event: `(name, payload)`.
type HookLog = Arc<Mutex<Vec<(String, Value)>>>;

struct Fixture {
    scratch: PathBuf,
    registry: Arc<TaskRegistry>,
    live: Arc<AtomicUsize>,
}

impl Fixture {
    fn new(label: &str, limits: OrchestrationLimits) -> Self {
        Self::build(
            label,
            limits,
            "http://127.0.0.1:9/v1".to_owned(),
            None,
            None,
        )
    }

    fn with_server(label: &str, limits: OrchestrationLimits, base_url: String) -> Self {
        Self::build(label, limits, base_url, None, None)
    }

    fn with_clock(
        label: &str,
        limits: OrchestrationLimits,
        base_url: String,
        clock: Clock,
    ) -> Self {
        Self::build(label, limits, base_url, Some(clock), None)
    }

    fn with_clock_and_hooks(
        label: &str,
        limits: OrchestrationLimits,
        base_url: String,
        clock: Clock,
        hooks: HookLog,
    ) -> Self {
        Self::build(label, limits, base_url, Some(clock), Some(hooks))
    }

    fn build(
        label: &str,
        limits: OrchestrationLimits,
        base_url: String,
        clock: Option<Clock>,
        hooks: Option<HookLog>,
    ) -> Self {
        let _ = isolated_home();
        let scratch = scratch(label);
        let lead = scratch.join("lead");
        std::fs::create_dir_all(&lead).unwrap();
        assert!(git(&lead, &["init", "-q", "-b", "main"]));
        assert!(git(
            &lead,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init"
            ]
        ));
        let session_dir = scratch.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let lead_journal = Arc::new(
            radian_session::SessionJournal::create(
                session_dir.join("lead.jsonl"),
                "lead",
                SessionCreatedData::new(lead.to_string_lossy(), json!({})),
            )
            .expect("the lead journal"),
        );
        let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
        let live = Arc::new(AtomicUsize::new(0));
        let max_tasks = limits.max_tasks;
        let mut registry = TaskRegistry::new(
            lead.clone(),
            "lead",
            lead_journal,
            Arc::clone(&queue),
            builder(session_dir.clone(), base_url, Arc::clone(&live), max_tasks),
        )
        .with_limits(limits);
        if let Some(clock) = clock {
            registry = registry.with_clock(move || clock());
        }
        if let Some(hooks) = hooks {
            let hook: radian_host::task::HookNotifier = Arc::new(move |name, data| {
                hooks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((name.to_owned(), data.clone()));
            });
            registry = registry.with_hook_notifier(hook);
        }
        Self {
            scratch,
            registry: Arc::new(registry),
            live,
        }
    }

    /// Whether this machine has a sandbox backend (else tests skip).
    fn sandbox_available(&self) -> bool {
        workspace_sandbox(self.registry.workspace()).is_some()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Waits, bounded, until `predicate` yields; a channel/wake or a short poll, never a long sleep.
async fn wait_until<T>(mut predicate: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the condition did not become true in time"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn limits(max_tasks: usize, stall_after_secs: f64) -> OrchestrationLimits {
    OrchestrationLimits {
        max_tasks,
        stall_after_secs,
        ..OrchestrationLimits::default()
    }
}

#[tokio::test]
async fn spawn_past_max_tasks_queues_fifo_and_starts_when_a_slot_frees() {
    let fixture = Fixture::new("queue-fifo", limits(2, 0.0));
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);

    for id in ["t1", "t2"] {
        let spawned = registry.submit(SpawnRequest::new(id)).await.expect("spawn");
        assert!(spawned.record.is_some(), "{id} started");
    }
    let t3 = registry
        .submit(SpawnRequest::new("t3"))
        .await
        .expect("queue");
    assert!(
        t3.record.is_none() && t3.position == Some(1),
        "t3 is queued first"
    );
    let t4 = registry
        .submit(SpawnRequest::new("t4"))
        .await
        .expect("queue");
    assert!(t4.record.is_none() && t4.position == Some(2));

    assert_eq!(registry.live_count(), 2);
    assert_eq!(
        registry.queued(),
        vec![("t3".to_owned(), 1), ("t4".to_owned(), 2)]
    );
    assert!(registry.get("t3").is_none(), "a queued task has no record");
    assert!(matches!(registry.lookup("t3"), Some(TaskHandle::Queued(_))));

    // Removing t1 frees a slot; the pump starts t3 (not t4), FIFO.
    registry.remove("t1", true).await.expect("remove");
    wait_until(|| registry.get("t3").map(|_| ())).await;
    assert_eq!(registry.queued(), vec![("t4".to_owned(), 1)]);
    assert!(registry.get("t4").is_none(), "t4 waits behind t3");
    assert_eq!(registry.live_count(), 2);
    assert!(
        fixture.live.load(Ordering::SeqCst) <= 2,
        "the builder seam saw at most max_tasks live"
    );
}

#[tokio::test]
async fn removing_a_queued_task_drops_it_from_the_queue() {
    let fixture = Fixture::new("queue-remove", limits(1, 0.0));
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);

    registry
        .submit(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let queued = registry
        .submit(SpawnRequest::new("t2"))
        .await
        .expect("queue");
    assert_eq!(queued.position, Some(1));

    registry
        .remove("t2", false)
        .await
        .expect("drop the queued task");
    assert!(registry.lookup("t2").is_none(), "t2 is gone");
    assert!(registry.queued().is_empty(), "the queue is empty");
    assert!(registry.get("t1").is_some(), "t1 is untouched");
}

#[tokio::test]
async fn a_wait_on_a_queued_task_times_out_still_running_and_never_cancels() {
    let fixture = Fixture::new("queue-wait", limits(1, 0.0));
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);
    registry
        .submit(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    registry
        .submit(SpawnRequest::new("t2"))
        .await
        .expect("queue t2");

    let tools = LeadTools::new(Arc::clone(&registry));
    let session = tools.open_session().await.expect("lead session");
    let sink = NoopSink;
    let waiting = tokio::spawn(async move {
        let cx = agent_core::ToolContext {
            cancellation: agent_core::CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        run_lead_tool(
            &session,
            &agent_core::ToolCall {
                id: "c".into(),
                name: "wait_task".into(),
                arguments: json!({"id": "t2", "timeout_secs": 0.05}),
            },
            cx,
        )
        .await
    });
    // The wait returns `still_running`, not an error, and the task is untouched.
    let output = waiting
        .await
        .expect("the wait task")
        .expect("still_running");
    assert_eq!(output.structured.unwrap()["event"], "still_running");
    assert!(matches!(registry.lookup("t2"), Some(TaskHandle::Queued(_))));
}

#[tokio::test]
async fn a_queued_task_starts_when_a_running_one_finishes() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::with_server("queue-finish", limits(1, 0.0), server.base_url.clone());
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);

    let mut first = SpawnRequest::new("t1");
    first.brief = Some("first".into());
    let record_t1 = registry
        .submit(first)
        .await
        .expect("spawn t1")
        .record
        .expect("t1 started");

    let t2 = registry
        .submit(SpawnRequest::new("t2"))
        .await
        .expect("queue t2");
    assert_eq!(t2.position, Some(1));

    // t1's turn ends, freeing the slot; the pump starts t2.
    wait_until(|| record_t1.last_outcome().map(|_| ())).await;
    wait_until(|| registry.get("t2").map(|_| ())).await;
    assert!(registry.queued().is_empty());
}

// ---------------------------------------------------------------------------
// Stall (ADR 0030 O8/R5)
// ---------------------------------------------------------------------------

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn ask_user_reply(id: &str) -> Reply {
    let arguments =
        json!({"questions": [{"id": "q1", "prompt": "which?", "kind": "text"}]}).to_string();
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": "ask_user", "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

#[tokio::test]
async fn the_stall_fires_once_clears_on_activity_and_fires_again() {
    // ask_user, ask_user, then a hanging reply: the turn stays running while it waits on the
    // lead, and the task never writes.
    let server = MockServer::start(vec![
        ask_user_reply("call_q1"),
        ask_user_reply("call_q2"),
        Reply::SseThenHang { pieces: Vec::new() },
    ])
    .await;
    let base = Utc::now();
    let cell = Arc::new(Mutex::new(base));
    let clock: Clock = {
        let cell = Arc::clone(&cell);
        Arc::new(move || *cell.lock().unwrap_or_else(|e| e.into_inner()))
    };
    let fixture = Fixture::with_clock(
        "stall",
        limits(4, 30.0),
        server.base_url.clone(),
        Arc::clone(&clock),
    );
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);

    let mut briefed = SpawnRequest::new("t1");
    briefed.brief = Some("go".into());
    let record = registry
        .submit(briefed)
        .await
        .expect("spawn")
        .record
        .expect("started");

    // Wait for the first ask_user (the turn is running, blocked on the lead).
    let pending = Arc::clone(registry.pending());
    let view = wait_until(|| pending.next_for("t1")).await;
    assert!(
        record.last_outcome().is_none(),
        "the turn has not finished while it waits"
    );

    // The task's activity timestamps come from the real clock, so measure the quiet stretch
    // from a moment taken after that activity: under a loaded machine the first events can
    // land seconds after `base` (gate 2026-10-16: 0 notices at base + 31 s).
    let quiet = Utc::now();
    *cell.lock().unwrap() = quiet + chrono::Duration::seconds(31);
    assert_eq!(registry.check_stalls(), 1, "one notice at the threshold");
    let notice = registry.next_stall().expect("a notice");
    assert_eq!(notice.task_id, "t1");
    assert_eq!(registry.check_stalls(), 0, "one notice per episode");

    // Answer the question: the task advances (a fresh committed event), so the episode clears.
    registry
        .answer("t1", &view.request_id, Vec::new())
        .expect("answer");
    let _view2 = wait_until(|| pending.next_for("t1")).await;
    let quiet = Utc::now();
    *cell.lock().unwrap() = quiet + chrono::Duration::seconds(29);
    assert_eq!(registry.check_stalls(), 0, "recent activity is not a stall");
    assert!(
        record.last_outcome().is_none(),
        "the runtime never intervenes (the task keeps waiting)"
    );
    assert!(matches!(
        registry.lookup("t1"),
        Some(TaskHandle::Started(_))
    ));

    // A second quiet stretch fires again.
    *cell.lock().unwrap() = quiet + chrono::Duration::seconds(31);
    assert_eq!(registry.check_stalls(), 1, "a second episode fires again");
}

/// ADR 0030 O7/O8 / slice 9: the `stalled` hook point is emitted when a task's two timestamps are
/// both past the threshold (the same check the `stalled` watch uses), with the stable name and
/// payload for the control port.
#[tokio::test]
async fn the_stalled_hook_point_is_emitted() {
    let server = MockServer::start(vec![
        ask_user_reply("call_q1"),
        Reply::SseThenHang { pieces: Vec::new() },
    ])
    .await;
    let base = Utc::now();
    let cell = Arc::new(Mutex::new(base));
    let clock: Clock = {
        let cell = Arc::clone(&cell);
        Arc::new(move || *cell.lock().unwrap_or_else(|e| e.into_inner()))
    };
    let hooks: HookLog = Arc::new(Mutex::new(Vec::new()));
    let fixture = Fixture::with_clock_and_hooks(
        "stall-hook",
        limits(4, 30.0),
        server.base_url.clone(),
        Arc::clone(&clock),
        Arc::clone(&hooks),
    );
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);
    let mut briefed = SpawnRequest::new("t1");
    briefed.brief = Some("go".into());
    registry
        .submit(briefed)
        .await
        .expect("spawn")
        .record
        .expect("started");

    let pending = Arc::clone(registry.pending());
    let _view = wait_until(|| pending.next_for("t1")).await;
    let quiet = Utc::now();
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = quiet + chrono::Duration::seconds(31);
    assert_eq!(registry.check_stalls(), 1, "one notice at the threshold");

    let seen = hooks.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        seen.iter().any(|(name, data)| name == "stalled"
            && data["task"] == "t1"
            && data["idle_secs"].as_u64().unwrap_or(0) >= 30),
        "the stalled hook point: {seen:?}"
    );
}

/// ADR 0030 item 4: the agent definition is snapshotted at the `spawn_task` call, so a
/// task that waits past `max_tasks` starts from that snapshot even if the file is gone by the
/// time a slot frees. Without the submit-time snapshot the queued start would re-resolve and fail.
#[tokio::test]
async fn a_queued_task_starts_from_its_submit_time_snapshot() {
    let fixture = Fixture::new("queue-snapshot", limits(1, 0.0));
    if !fixture.sandbox_available() {
        return;
    }
    let registry = Arc::clone(&fixture.registry);
    // A definition the queued task names.
    let home = isolated_home();
    let dir = home.join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let def = dir.join("qagent.md");
    std::fs::write(&def, "---\nname: qagent\n---\nQA\n").unwrap();

    // t1 occupies the only slot (no brief, so it runs no turn and stays live).
    registry
        .submit(SpawnRequest::new("t1"))
        .await
        .expect("t1 starts");
    let mut request = SpawnRequest::new("t2");
    request.agent = Some("qagent".into());
    let queued = registry.submit(request).await.expect("t2 queues");
    assert_eq!(queued.position, Some(1), "t2 waits");
    assert!(registry.get("t2").is_none(), "a queued task has no record");

    // The definition file is gone before the queued task starts.
    std::fs::remove_file(&def).unwrap();

    // Free the slot: the pump starts t2 from the submit-time snapshot.
    registry.remove("t1", true).await.expect("remove t1");
    let record = wait_until(|| registry.get("t2")).await;
    assert_eq!(
        record.agent, "qagent",
        "the queued task started from its submit-time definition"
    );
    assert!(registry.queued().is_empty(), "the queue drained");
    registry.remove("t2", true).await.ok();
}

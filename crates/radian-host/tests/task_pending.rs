//! ADR 0030 slice 5: the task-side pending queue, the three-layer edge policy, and the promise
//! that nothing a task raises reaches the lead's inbox.
//!
//! Group A/B/D drive the queue and the `TaskApprover` directly (deterministic, no sandbox).
//! Group C runs a real task session against a mock model where a sandbox backend exists.

use std::sync::Arc;
use std::time::Duration;

use agent_core::{InputSource, Message};
use radian_coding::{Prompter, Question, QuestionKind, QuestionReply, QuestionRequest};
use radian_host::{
    ApprovalRouting, HostBindings, PolicyCommand, SessionError, SessionId, SessionProjection,
    SessionQueue, SessionSetup, SpawnRequest, TaskApprover, TaskBuilder, TaskContext,
    TaskPendingContent, TaskPendingKind, TaskPendingQueue, TaskPrompter, TaskRegistry, TaskSession,
    build_model,
};
use radian_permissions::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Approver, NoopAudit, Preset, Profile,
};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::SessionCreatedData;
use serde_json::{Value, json};

fn request(id: &str) -> ApprovalRequest {
    ApprovalRequest::new(
        id,
        format!("call-{id}"),
        "shell",
        "echo hi",
        "echo hi",
        json!({"command": "echo hi"}),
        Some("shell asks in this tier".into()),
    )
}

fn policy_script(name: &str, script: &str) -> PolicyCommand {
    PolicyCommand::new(name, "/bin/sh").with_args(vec!["-c".into(), script.into()])
}

/// Polls a queue until the next event appears, waiting on its `watch` instead of sleeping.
async fn wait_next(
    queue: &Arc<TaskPendingQueue>,
    rx: &mut tokio::sync::watch::Receiver<u64>,
) -> radian_host::TaskPendingView {
    loop {
        if let Some(view) = queue.next() {
            return view;
        }
        rx.changed().await.expect("the queue is alive");
    }
}

// ---------------------------------------------------------------------------
// Group A: the edge policy command (ADR 0030 R1 layer 2)
// ---------------------------------------------------------------------------

/// A policy command that says `allow`/`deny` decides, and the outcome names the policy.
#[tokio::test]
async fn a_policy_command_allow_and_deny_decide_and_are_named() {
    let queue = TaskPendingQueue::new(None);
    queue.set_timeout(Duration::from_secs(600));

    let approver = TaskApprover::new(
        Arc::clone(&queue),
        "t1",
        ApprovalPolicy::Auto,
        Some(policy_script("safe-commands", "read line; echo allow")),
        None,
    );
    let outcome = approver.decide(request("r1")).await.unwrap();
    assert_eq!(
        outcome,
        ApprovalOutcome::Policy {
            allow: true,
            name: "safe-commands".into(),
            reason: Some("allowed by the edge policy".into()),
        }
    );
    assert!(queue.peek().is_none(), "an allow raises no task event");

    let approver = TaskApprover::new(
        Arc::clone(&queue),
        "t1",
        ApprovalPolicy::Auto,
        Some(policy_script("safe-commands", "read line; echo deny")),
        None,
    );
    let outcome = approver.decide(request("r2")).await.unwrap();
    assert!(
        matches!(
            outcome,
            ApprovalOutcome::Policy {
                allow: false,
                ref name,
                ..
            } if name == "safe-commands"
        ),
        "a deny names the policy: {outcome:?}"
    );
}

/// A policy that says `ask` is **not** auto-allowed even under `auto`: it becomes a pending task
/// event. This is the mutation "policy ask auto-allowed".
#[tokio::test]
async fn a_policy_ask_lands_in_the_pending_queue_not_auto() {
    let queue = TaskPendingQueue::new(None);
    queue.set_timeout(Duration::from_secs(600));
    let mut rx = queue.subscribe();
    let approver = Arc::new(TaskApprover::new(
        Arc::clone(&queue),
        "t1",
        ApprovalPolicy::Auto,
        Some(policy_script("picky", "read line; echo ask")),
        None,
    ));
    let task_id = "t1";
    let handle = tokio::spawn({
        let approver = Arc::clone(&approver);
        async move { approver.decide(request("r-ask")).await }
    });

    // No sleep: the queue's watch wakes us when the task raises the event.
    let view = wait_next(&queue, &mut rx).await;
    assert_eq!(view.task_id, task_id);
    assert_eq!(view.kind(), TaskPendingKind::Approval);
    assert!(!handle.is_finished(), "the task is blocked, not allowed");

    queue
        .deny_approval(task_id, &view.request_id, Some("no".into()))
        .unwrap();
    let outcome = handle.await.unwrap().unwrap();
    assert!(
        matches!(outcome, ApprovalOutcome::Deny { .. }),
        "{outcome:?}"
    );
}

/// A policy that times out, exits non-zero, or prints something unparseable is `ask`.
#[tokio::test]
async fn policy_timeout_nonzero_and_garbage_all_still_ask() {
    // The command blocks on a second line of stdin, so it never exits before the tiny timeout.
    // No `sleep` in the test: the timeout is the policy command's own, configured small.
    let blocking = policy_script("hangs", "read a; read b; echo allow")
        .with_timeout(Duration::from_millis(150));
    let exit = policy_script("exit", "read line; exit 3");
    let garbage = policy_script("garbage", "read line; echo maybe");

    for policy in [blocking, exit, garbage] {
        let queue = TaskPendingQueue::new(None);
        queue.set_timeout(Duration::from_secs(600));
        let mut rx = queue.subscribe();
        let approver = Arc::new(TaskApprover::new(
            Arc::clone(&queue),
            "t1",
            ApprovalPolicy::Auto,
            Some(policy),
            None,
        ));
        let handle = tokio::spawn(async move { approver.decide(request("r-soft")).await });
        let view = wait_next(&queue, &mut rx).await;
        assert_eq!(view.kind(), TaskPendingKind::Approval);
        assert!(!handle.is_finished(), "a soft failure must not allow");
        queue
            .deny_approval("t1", &view.request_id, Some("stop".into()))
            .unwrap();
        assert!(matches!(
            handle.await.unwrap().unwrap(),
            ApprovalOutcome::Deny { .. }
        ));
    }
}

/// A plain `auto` task with no edge policy is allowed by the built-in layer, with no event.
#[tokio::test]
async fn auto_without_a_policy_allows_with_no_event() {
    let queue = TaskPendingQueue::new(None);
    let approver = TaskApprover::new(Arc::clone(&queue), "t1", ApprovalPolicy::Auto, None, None);
    let outcome = approver.decide(request("r-auto")).await.unwrap();
    assert_eq!(outcome, ApprovalOutcome::AllowOnce);
    assert!(queue.peek().is_none());
}

/// `on-ask` with no edge policy sends every ask to the pending queue.
#[tokio::test]
async fn on_ask_sends_every_ask_to_the_queue() {
    let queue = TaskPendingQueue::new(None);
    queue.set_timeout(Duration::from_secs(600));
    let mut rx = queue.subscribe();
    let approver = Arc::new(TaskApprover::new(
        Arc::clone(&queue),
        "t1",
        ApprovalPolicy::OnAsk,
        None,
        None,
    ));
    let handle = tokio::spawn(async move { approver.decide(request("r-on-ask")).await });
    let view = wait_next(&queue, &mut rx).await;
    assert_eq!(view.kind(), TaskPendingKind::Approval);
    assert!(!handle.is_finished());
    queue
        .deny_approval("t1", &view.request_id, Some("no".into()))
        .unwrap();
    assert!(matches!(
        handle.await.unwrap().unwrap(),
        ApprovalOutcome::Deny { .. }
    ));
}

// ---------------------------------------------------------------------------
// Group B: resolution operations
// ---------------------------------------------------------------------------

/// A lead's `deny_approval` narrows: the blocked task gets `Deny`. There is no approve op.
#[tokio::test]
async fn deny_approval_narrows_and_there_is_no_approve() {
    let queue = TaskPendingQueue::new(None);
    queue.set_timeout(Duration::from_secs(600));
    let mut rx = queue.subscribe();
    let handle = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.approval("t1".into(), request("r-deny")).await }
    });
    let view = wait_next(&queue, &mut rx).await;
    queue
        .deny_approval("t1", &view.request_id, Some("too risky".into()))
        .unwrap();
    match handle.await.unwrap().unwrap() {
        ApprovalOutcome::Deny { reason } => {
            assert_eq!(reason.as_deref(), Some("too risky"));
        }
        other => panic!("deny must narrow, got {other:?}"),
    }
    // A second resolution is refused (the event is gone), so nothing can flip it to allow.
    assert!(queue.deny_approval("t1", &view.request_id, None).is_err());
}

/// `escalate_approval` puts the request in the human queue with the lead's summary, and only the
/// human's answer resolves the task (the lead cannot approve). The mutation "an approve
/// operation added for the lead" makes the task resolve before the human answers.
#[tokio::test]
async fn escalate_reaches_the_human_queue_with_the_summary() {
    let human: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let queue = TaskPendingQueue::new(Some(Arc::clone(&human)));
    queue.set_timeout(Duration::from_secs(600));
    let mut rx = queue.subscribe();
    let handle = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.approval("t1".into(), request("r-esc")).await }
    });
    let view = wait_next(&queue, &mut rx).await;
    queue
        .escalate_approval(
            "t1",
            &view.request_id,
            "the task wants to rewrite the changelog",
            "allow it, it is in scope",
        )
        .unwrap();

    // The human queue now holds the task's request, tagged with the task as the origin.
    let head = human.head().expect("the human sees the escalation");
    assert_eq!(head.origin, "t1");
    match head.request {
        radian_host::PendingRequest::Approval(request) => {
            let reason = request.reason.expect("the lead's summary");
            assert!(reason.contains("rewrite the changelog"), "{reason}");
            assert!(reason.contains("lead recommendation: allow it"), "{reason}");
        }
        other => panic!("expected an approval, got {other:?}"),
    }
    // The task is still blocked: the lead has not decided.
    assert!(
        !handle.is_finished(),
        "the lead cannot approve, only the human can"
    );

    // The human answers; now the task resolves.
    human
        .answer(
            head.request_id,
            radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .unwrap();
    assert_eq!(handle.await.unwrap().unwrap(), ApprovalOutcome::AllowOnce);
}

/// A task question becomes a pending event and `answer` completes it.
#[tokio::test]
async fn a_task_question_is_answered_through_the_queue() {
    let queue = TaskPendingQueue::new(None);
    queue.set_timeout(Duration::from_secs(600));
    let mut rx = queue.subscribe();
    let prompter = Arc::new(TaskPrompter::new(Arc::clone(&queue), "t1"));
    let question = QuestionRequest::new(vec![Question {
        id: "q1".into(),
        header: None,
        prompt: "which branch?".into(),
        kind: QuestionKind::Text,
        options: Vec::new(),
        allow_other: false,
        required: true,
    }]);
    let handle = tokio::spawn(async move { prompter.ask(question).await });
    let view = wait_next(&queue, &mut rx).await;
    assert_eq!(view.kind(), TaskPendingKind::Question);
    assert!(matches!(view.content, TaskPendingContent::Question(_)));
    queue
        .answer(
            "t1",
            &view.request_id,
            vec![QuestionReply {
                id: "q1".into(),
                text: Some("main".into()),
                ..Default::default()
            }],
        )
        .unwrap();
    let answer = handle.await.unwrap().expect("an answer");
    assert_eq!(answer.answers.len(), 1);
    assert_eq!(answer.answers[0].text.as_deref(), Some("main"));
}

/// The R9 timeout starts when the lead **takes** the event, not when the task raises it
/// (ADR 0030), and it resolves as deny. Paused time, no sleep.
#[tokio::test(start_paused = true)]
async fn the_timeout_starts_at_take_and_resolves_as_deny() {
    let queue = TaskPendingQueue::new(None);
    queue.set_timeout(Duration::from_secs(60));
    let mut rx = queue.subscribe();
    let handle = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.approval("t1".into(), request("r-timeout")).await }
    });
    // The task raised the event.
    rx.changed().await.unwrap();

    // Advance well past the timeout *before* taking: the timeout has not started, so the task is
    // still blocked. A timeout starting at creation turns this red.
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "the timeout must not start at creation"
    );

    // Taking starts it; advancing fires it as a deny.
    let view = queue.next().expect("the event");
    assert!(!handle.is_finished());
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;
    match handle.await.unwrap().unwrap() {
        ApprovalOutcome::Deny { reason } => {
            assert!(reason.unwrap().contains("timed out"));
        }
        other => panic!("the timeout must deny, got {other:?}"),
    }
    let _ = view;
}

// ---------------------------------------------------------------------------
// Group C: a real task session (needs a sandbox backend)
// ---------------------------------------------------------------------------

fn scratch(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "radian-task-pending-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn git(dir: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn workspace_sandbox(ws: &std::path::Path) -> Option<Sandbox> {
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

fn builder(session_dir: std::path::PathBuf, base_url: String) -> TaskBuilder {
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(Value::Null);
            let sandbox = workspace_sandbox(&workspace)
                .ok_or_else(|| fail("no sandbox backend here".into()))?;
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
                        .map_err(|e| fail(e.to_string()))?,
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
                    turn_options: Default::default(),
                    turn_timeout: None,
                    wording: radian_host::CLI_WORDING,
                    capabilities: radian_host::SessionCapability::ALL.to_vec(),
                },
                bindings: HostBindings {
                    approvals: ApprovalRouting::Queued,
                    shared_queue: None,
                    ask_user: false,
                    audit: Arc::new(|_journal| {
                        Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
                    }),
                    events: Arc::new(agent_core::NoopSink),
                    projection: Arc::new(SessionProjection::new()),
                    runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
                    driver_stopped: Arc::new(|_| {}),
                    before_driver: None,
                    memory_warning: Arc::new(|_message: &str| {}),
                    memory_notice_line: Arc::new(|_line: &str| {}),
                },
            })
        },
    )
}

/// A task's still-ask approval becomes a pending task event; the lead denies it; the task never
/// appears in the human queue and nothing is written to the lead's inbox.
#[tokio::test]
async fn a_task_ask_blocks_on_the_pending_queue_and_never_touches_the_lead_inbox() {
    use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "shell", "arguments": "{\"command\":\"echo hi\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        Reply::sse(&[
            chunk(json!({"role": "assistant", "content": "done"}), None),
            chunk(json!({}), Some("stop")),
        ]),
    ])
    .await;
    let dir = scratch("e2e");
    let lead = dir.join("lead");
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
    if workspace_sandbox(&lead).is_none() {
        return; // no sandbox backend here
    }
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let lead_journal = Arc::new(
        radian_session::SessionJournal::create(
            session_dir.join("lead.jsonl"),
            "lead",
            SessionCreatedData::new(lead.to_string_lossy(), json!({})),
        )
        .unwrap(),
    );
    let human: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let registry = TaskRegistry::new(
        lead.clone(),
        "lead",
        lead_journal,
        Arc::clone(&human),
        builder(session_dir.clone(), server.base_url.clone()),
    );

    let mut request = SpawnRequest::new("t1");
    // `on-ask`: every ask becomes a pending task event (the ticket's item 3).
    request.approval = Some(ApprovalPolicy::OnAsk);
    let _record = registry.spawn(request).await.expect("spawn");

    // Subscribe before the brief, so the event cannot be missed.
    let queue = Arc::clone(registry.pending());
    let mut rx = queue.subscribe();
    let session = registry.get("t1").unwrap().session.clone();
    session
        .inbox()
        .followup(Message::user("run echo"), InputSource::Subagent)
        .await
        .expect("the brief");

    let view = wait_next(&queue, &mut rx).await;
    assert_eq!(view.kind(), TaskPendingKind::Approval);
    match &view.content {
        TaskPendingContent::Approval(request) => {
            assert_eq!(request.tool, "shell", "{request:?}");
        }
        other => panic!("expected an approval, got {other:?}"),
    }
    // The human queue is untouched: a task never appears to the human directly.
    assert_eq!(human.len(), 0, "the human must not see the task directly");

    registry
        .deny_approval("t1", &view.request_id, Some("no shell".into()))
        .unwrap();

    // The turn finishes after the denial.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while server.requests().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Nothing the task raised reached the lead's inbox: only TaskSpawned / TaskFinished and the
    // task's own turn events are in the lead journal, and no InboxEnqueued names the task.
    let lines = std::fs::read_to_string(session_dir.join("lead.jsonl")).unwrap();
    assert!(
        !lines.contains("InboxEnqueued"),
        "the task must never enqueue into the lead's inbox:\n{lines}"
    );
    assert!(
        !lines.contains("\"subagent\""),
        "no subagent input is written to the lead's journal:\n{lines}"
    );

    registry.remove("t1", true).await.expect("remove");
    let _ = std::fs::remove_dir_all(&dir);
}

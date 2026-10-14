//! The Session Facade (ADR 0023 U5): the capability list is the host's, and the facade
//! re-validates a command before it runs — the host does not trust the UI's copy.
//!
//! `open_session` is the real in-process implementation, so this test opens a real session
//! (with a mock model that is never called) and checks the refusal.

use std::sync::Arc;

use agent_core::{NoopSink, TurnOptions};
use radian_host::{
    HostBindings, SessionCapability, SessionCommand, SessionFacade, SessionId, SessionProjection,
    SessionSetup, build_model, open_session,
};
use radian_permissions::{
    ApprovalError, ApprovalOutcome, ApprovalRequest, Approver, NoopAudit, Preset, Profile,
};
use serde_json::json;

struct AllowAll;

impl Approver for AllowAll {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async { Ok(ApprovalOutcome::AllowOnce) })
    }
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "radian-host-session-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens a session that declares only `Observe`.
async fn observe_only(label: &str) -> (Scratch, Arc<radian_host::LocalSession>) {
    open_with(label, vec![SessionCapability::Observe]).await
}

/// Opens a session with `capabilities`.
async fn open_with(
    label: &str,
    capabilities: Vec<SessionCapability>,
) -> (Scratch, Arc<radian_host::LocalSession>) {
    let scratch = Scratch::new(label);
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "mock".into(),
        api_key: None,
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
    })
    .unwrap();
    let model = radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        config_slot.clone(),
        radian_host::RouteContext {
            providers: std::collections::BTreeMap::new(),
            session_id: label.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let profile = Profile::safe(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model,
            config_slot: config_slot.clone(),
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            servers: Vec::new(),
            system_prompt: None,
            context_budget: None,
            auto_compact: false,
            sandbox: None,
            tier: Preset::Safe,
            profile,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities,
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(AllowAll)),
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|| {}),
            ask_user: false,
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .unwrap();
    (scratch, Arc::new(session))
}

/// The facade declares `Observe` only, so a send is refused **by the facade**, not only by the
/// UI's key handling.
#[tokio::test]
async fn a_local_session_refuses_a_command_it_does_not_declare() {
    let (scratch, session) = observe_only("refuse").await;
    assert_eq!(session.capabilities(), [SessionCapability::Observe]);

    let refused = session
        .command(SessionCommand::SendFollowup("hi".into()))
        .expect_err("a session that cannot send must refuse the command");
    assert_eq!(refused.command, "send");
    assert_eq!(refused.capability.as_deref(), Some("send"));
    assert!(
        refused.reason.contains("does not declare"),
        "{}",
        refused.reason
    );

    // Every command that needs an undeclared capability is refused, and the projection is
    // still readable (the facade serves `Observe`).
    assert!(session.command(SessionCommand::Cancel).is_err());
    assert_eq!(session.id().as_str(), "refuse");
    assert_eq!(
        session.projection().read().turn(),
        radian_host::TurnState::Idle
    );

    session.stop();
    assert!(session.join().await.is_ok());
    drop(scratch);
}

/// A session that declares the capability accepts the command (the check is not "refuse
/// everything").
#[tokio::test]
async fn a_local_session_accepts_a_declared_command() {
    let scratch = Scratch::new("accept");
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "mock".into(),
        api_key: None,
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
    })
    .unwrap();
    let model = radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        config_slot.clone(),
        radian_host::RouteContext {
            providers: std::collections::BTreeMap::new(),
            session_id: "can-send".to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let profile = Profile::safe(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new("can-send"),
            session_dir: scratch.0.clone(),
            workspace,
            coding_root: None,
            model,
            config_slot: config_slot.clone(),
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            servers: Vec::new(),
            system_prompt: None,
            context_budget: None,
            auto_compact: false,
            sandbox: None,
            tier: Preset::Safe,
            profile,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            resume: false,
            config_snapshot: json!({}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(AllowAll)),
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|| {}),
            ask_user: false,
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .unwrap();
    let session = Arc::new(session);
    // `command` blocks on the inbox, so it must not run on a runtime thread: a real host calls
    // it from its synchronous UI thread, and this test uses a blocking thread.
    let command = Arc::clone(&session);
    tokio::task::spawn_blocking(move || command.command(SessionCommand::SendFollowup("go".into())))
        .await
        .unwrap()
        .expect("a declared capability accepts the command");
    // The command returned `Ok`, which is the whole assertion: a declared capability accepts
    // it. (The driver may already have claimed the turn, so the inbox need not still hold it.)
    session.stop();
    let _ = session.join().await;
}

/// The compact path checks the capability too (PR ④'s review found this unchecked). A session
/// that declares only `Observe` must refuse it **immediately**, without touching the inbox:
/// a real request would be async (it is a control request the driver answers).
#[tokio::test]
async fn a_local_session_without_the_capability_refuses_compact() {
    let (scratch, session) = observe_only("compact").await;
    assert_eq!(session.capabilities(), [SessionCapability::Observe]);

    let (tx, mut rx) = tokio::sync::oneshot::channel();
    session.compact(
        None,
        Box::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    let result = rx
        .try_recv()
        .expect("a refused compact must report immediately, without an inbox request");
    assert!(
        result.unwrap_err().contains("does not declare"),
        "the refusal names the missing capability"
    );

    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// The answer path checks the capability too (ADR 0023 U5's `AnswerRequest`): a facade that
/// declares only `Observe` cannot answer an approval, so a UI that ignored the capability
/// list still cannot resolve one.
#[tokio::test]
async fn a_local_session_without_the_capability_refuses_an_answer() {
    let (scratch, session) = observe_only("answer").await;
    assert_eq!(session.capabilities(), [SessionCapability::Observe]);

    let refused = session
        .answer(
            1,
            radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .expect_err("a session that cannot answer must refuse");
    assert_eq!(refused.command, "answer");
    assert_eq!(refused.capability.as_deref(), Some("answer"));

    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// A late answer (the request is already gone) is silently ignored, not an error: ADR 0024 Q4
/// says a request that was already completed or abandoned must not be completed twice. The
/// facade hides `Answered::Late`/`Unknown` from the UI, which has nothing useful to do with it.
#[tokio::test]
async fn a_late_answer_is_silently_ignored() {
    let (scratch, session) = open_with("late", SessionCapability::ALL.to_vec()).await;
    assert!(session.queue().is_empty());
    session
        .answer(
            7,
            radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .expect("a late answer is ignored, not refused");
    assert!(session.queue().is_empty());
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// ADR 0023 U7 / ADR 0024 Q4: closing a session completes its pending requests — an approval
/// is denied fail-closed and a question gets `PromptError::Gone` — so nothing is left waiting.
#[tokio::test]
async fn stopping_a_session_aborts_its_pending_requests() {
    use std::time::Duration;

    let (scratch, session) = open_with("stop", SessionCapability::ALL.to_vec()).await;
    let origin = session.id().as_str().to_string();
    let (_approval_id, approval) = session.queue().push(
        origin.clone(),
        radian_host::PendingRequest::Approval(radian_permissions::ApprovalRequest::new(
            "perm-1",
            "call-1",
            "write",
            "/ws/out.txt",
            "/ws/out.txt",
            json!({}),
            None,
        )),
    );
    let (_question_id, question) = session.queue().push(
        origin,
        radian_host::PendingRequest::Question(radian_coding::QuestionRequest::new(Vec::new())),
    );
    assert_eq!(session.queue().len(), 2);

    session.stop();

    let approval = tokio::time::timeout(Duration::from_secs(2), approval.wait())
        .await
        .expect("the approval must not hang")
        .expect("the queue must complete it");
    assert_eq!(
        approval,
        radian_host::PendingResponse::Approval(ApprovalOutcome::Deny {
            reason: Some("session closed".into())
        }),
        "an approval is denied fail-closed when the session closes"
    );
    let question = tokio::time::timeout(Duration::from_secs(2), question.wait())
        .await
        .expect("the question must not hang")
        .expect("the queue must complete it");
    assert_eq!(
        question,
        radian_host::PendingResponse::Question(Err(radian_coding::PromptError::Gone)),
        "a question is aborted when the session closes"
    );
    assert!(session.queue().is_empty());
    let _ = session.join().await;
    drop(scratch);
}

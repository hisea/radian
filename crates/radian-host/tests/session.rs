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
    let scratch = Scratch::new(label);
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "mock".into(),
        api_key: None,
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
    })
    .unwrap();
    let profile = Profile::safe(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new("observe-only"),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model,
            servers: Vec::new(),
            system_prompt: None,
            context_budget: None,
            auto_compact: false,
            sandbox: None,
            tier: Preset::Safe,
            profile,
            memory_root: None,
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: vec![SessionCapability::Observe],
        },
        HostBindings {
            approver: Arc::new(AllowAll),
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|| {}),
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
    assert_eq!(session.id().as_str(), "observe-only");
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
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "mock".into(),
        api_key: None,
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
    })
    .unwrap();
    let profile = Profile::safe(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new("can-send"),
            session_dir: scratch.0.clone(),
            workspace,
            coding_root: None,
            model,
            servers: Vec::new(),
            system_prompt: None,
            context_budget: None,
            auto_compact: false,
            sandbox: None,
            tier: Preset::Safe,
            profile,
            memory_root: None,
            resume: false,
            config_snapshot: json!({}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approver: Arc::new(AllowAll),
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|| {}),
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

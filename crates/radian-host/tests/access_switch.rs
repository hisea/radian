//! ADR 0038 slice 2: the host-side tier switch (`SessionFacade::set_access`) and the resume
//! read (`resumed_access`).
//!
//! `set_access` rebuilds the profile and the sandbox through the host's `AccessBuilder`. These
//! tests use a builder that returns a transparent `full` sandbox, `None` for `safe`, and an
//! error for `workspace` (a simulated probe failure), so all three paths run without a real
//! backend. The switch must apply to the live hooks and the projection, queue one
//! `ConfigChanged`, and a failed probe must refuse and change nothing.

use std::sync::Arc;

use agent_core::{NoopSink, TurnOptions};
use radian_host::{
    HostBindings, SessionCapability, SessionFacade, SessionId, SessionProjection, SessionSetup,
    build_model, open_session,
};
use radian_permissions::{NoopAudit, Preset, Profile};
use serde_json::json;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("radian-host-access-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn model(session_id: &str) -> radian_host::SwitchableModel {
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
        reasoning_replay: None,
    })
    .unwrap();
    radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        config_slot,
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

/// A builder that simulates the three tiers: `safe` has no sandbox, `full` is transparent, and
/// `workspace` fails its probe.
fn switchable_builder(workspace: std::path::PathBuf) -> radian_host::AccessBuilder {
    Arc::new(move |preset: Preset| match preset {
        Preset::Safe => Ok(radian_host::AccessParts {
            profile: Profile::safe(&workspace).map_err(|e| e.to_string())?,
            sandbox: None,
        }),
        Preset::FullAccess => Ok(radian_host::AccessParts {
            profile: Profile::full_access(&workspace).map_err(|e| e.to_string())?,
            sandbox: Some(
                radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::FullAccess, &workspace)
                    .map_err(|e| e.to_string())?,
            ),
        }),
        Preset::WorkspaceWrite => {
            Err("the `seatbelt` sandbox backend is unavailable or failed its probe".to_string())
        }
    })
}

/// Opens a session that starts on `safe` and can switch tiers.
async fn open_switchable(label: &str) -> (Scratch, Arc<radian_host::LocalSession>) {
    let scratch = Scratch::new(label);
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model: model(label),
            config_slot,
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
            sandbox: None,
            tier: Preset::Safe,
            profile: Profile::safe(&workspace).unwrap(),
            access_builder: Some(switchable_builder(workspace.clone())),
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(NoopApprover)),
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
            memory_notice_line: Arc::new(|_line: &str| {}),
        },
    )
    .await
    .unwrap();
    (scratch, Arc::new(session))
}

struct NoopApprover;

impl radian_permissions::Approver for NoopApprover {
    fn decide(
        &self,
        _request: radian_permissions::ApprovalRequest,
    ) -> futures_util::future::BoxFuture<
        '_,
        Result<radian_permissions::ApprovalOutcome, radian_permissions::ApprovalError>,
    > {
        Box::pin(async { Ok(radian_permissions::ApprovalOutcome::Deny { reason: None }) })
    }
}

/// `set_access` swaps the live profile, moves the projection's tier, and queues exactly one
/// `ConfigChanged` for the driver to journal.
#[tokio::test]
async fn set_access_swaps_the_profile_and_queues_the_change() {
    let (_scratch, session) = open_switchable("set-access").await;
    assert_eq!(session.hooks().profile().preset, Preset::Safe);

    session
        .set_access(Preset::FullAccess)
        .expect("full is allowed");
    assert_eq!(session.hooks().profile().preset, Preset::FullAccess);
    assert_eq!(session.projection().read().tier(), Some("full"));
    assert_eq!(
        session.sandbox_mode(),
        Some(radian_sandbox::SandboxMode::FullAccess),
        "the switch must reach the sandbox the tools read"
    );
    assert_eq!(
        session.config_queue().len(),
        1,
        "one ConfigChanged per switch"
    );

    // Switching back narrows again, from the very next call.
    session.set_access(Preset::Safe).expect("safe is allowed");
    assert_eq!(session.hooks().profile().preset, Preset::Safe);
    assert_eq!(session.projection().read().tier(), Some("safe"));
    assert_eq!(session.sandbox_mode(), None, "safe has no sandbox");
    assert_eq!(session.config_queue().len(), 2);

    session.stop();
    let _ = session.join().await;
}

/// A failed sandbox probe is a refusal that changes nothing: the profile, the projection and
/// the config queue are untouched (ADR 0012: a failed probe never degrades).
#[tokio::test]
async fn a_failed_probe_refuses_and_changes_nothing() {
    let (_scratch, session) = open_switchable("refuse").await;
    let refused = session
        .set_access(Preset::WorkspaceWrite)
        .expect_err("a failed probe must refuse");
    assert!(refused.reason.contains("unavailable"), "{}", refused.reason);
    assert_eq!(session.hooks().profile().preset, Preset::Safe);
    assert_eq!(session.projection().read().tier(), Some("safe"));
    assert!(session.config_queue().is_empty());

    session.stop();
    let _ = session.join().await;
}

/// A session with no builder refuses `set_access` (the CLI, which has no `/access`).
#[tokio::test]
async fn a_session_without_a_builder_refuses_set_access() {
    let scratch = Scratch::new("no-builder");
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new("no-builder"),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model: model("no-builder"),
            config_slot: radian_runtime::ConfigSlot::new(serde_json::Value::Null),
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
            sandbox: None,
            tier: Preset::Safe,
            profile: Profile::safe(&workspace).unwrap(),
            access_builder: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume: false,
            config_snapshot: json!({}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(NoopApprover)),
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
            memory_notice_line: Arc::new(|_line: &str| {}),
        },
    )
    .await
    .unwrap();
    let session = Arc::new(session);

    let refused = session
        .set_access(Preset::FullAccess)
        .expect_err("no builder, no switch");
    assert!(
        refused.reason.contains("cannot switch"),
        "{}",
        refused.reason
    );

    session.stop();
    let _ = session.join().await;
}

/// `resumed_access` reads the journal's **last** access change; a model change in between does
/// not disturb it.
#[tokio::test]
async fn resumed_access_reads_the_last_change() {
    let scratch = Scratch::new("resume-access");
    let path = scratch.0.join("resumed.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "resumed",
        radian_session::SessionCreatedData::new("/ws", json!({})),
    )
    .unwrap();
    for change in [
        json!({"access": "workspace"}),
        json!({"model": {"provider": "openai-compatible", "model": "mock"}}),
        json!({"access": "full"}),
    ] {
        journal
            .append_runtime_event(radian_session::RuntimeEvent::ConfigChanged {
                revision: 1,
                change,
            })
            .await
            .unwrap();
    }
    drop(journal);

    assert_eq!(
        radian_host::resumed_access(&scratch.0, "resumed"),
        Some(Preset::FullAccess)
    );
    assert_eq!(
        radian_host::resumed_access(&scratch.0, "missing"),
        None,
        "an unknown session has no access change"
    );
}

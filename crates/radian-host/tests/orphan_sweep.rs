//! The resume sweep of the session's jobs directory (ADR 0037 §6).
//!
//! A resumed session clears `<session-id>.jobs/` right after the tools are built: every
//! leftover file is gone after open. A fresh (non-resume) open never sweeps: a pre-created
//! jobs directory keeps its files, which is exactly what the fresh path sees (the directory
//! is created, never cleared, by `build_coding_tools`).

use std::sync::Arc;

use radian_core::{NoopSink, TurnOptions};
use radian_host::{
    HostBindings, SessionCapability, SessionId, SessionProjection, SessionSetup, build_model,
    open_session,
};
use radian_permissions::{ApprovalError, ApprovalRequest};
use radian_permissions::{ApprovalOutcome, Approver, NoopAudit, Preset, Profile};
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
            "radian-host-orphan-sweep-{label}-{}",
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

/// Opens a session with coding tools (so a jobs registry exists), `resume` as given, and an
/// optional pre-created leftover file in `<label>.jobs/`. Returns the jobs directory for the
/// filesystem assertions.
async fn open_with_leftover(
    label: &str,
    resume: bool,
    leftover: Option<(&str, &str)>,
) -> (Scratch, Arc<radian_host::LocalSession>, std::path::PathBuf) {
    let scratch = Scratch::new(label);
    let workspace = scratch.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    if resume {
        // The resume path requires the session file to exist; no records are needed.
        let path = scratch.0.join(format!("{label}.jsonl"));
        let journal = radian_session::SessionJournal::create(
            &path,
            label,
            radian_session::SessionCreatedData::new(workspace.to_string_lossy(), json!({})),
        )
        .unwrap();
        drop(journal);
    }
    let jobs_dir = scratch.0.join(format!("{label}.jobs"));
    if let Some((name, content)) = leftover {
        std::fs::create_dir_all(&jobs_dir).unwrap();
        std::fs::write(jobs_dir.join(name), content).unwrap();
    }
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = build_model(radian_host::ModelArgs {
        extra_body: Default::default(),
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
        max_output_tokens: None,
        chatgpt_oauth: false,
    })
    .unwrap();
    let model = radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        config_slot.clone(),
        radian_host::RouteContext {
            extra_body: Default::default(),
            providers: std::collections::BTreeMap::new(),
            session_id: label.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let profile = Profile::full_access(&workspace).unwrap();
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: Some(workspace.clone()),
            model,
            config_slot: config_slot.clone(),
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            servers: Vec::new(),
            mcp_config_path: None,
            system_prompt: None,
            context_budget: None,
            window_context: None,
            auto_compact: false,
            compaction_settings: serde_json::Value::Null,
            compaction_settings_for: None,
            retries: 0,
            sandbox: None,
            tier: Preset::FullAccess,
            profile,
            access_builder: None,
            parent: None,
            subagent: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            tools_allow: None,
            extra_deny_read: Vec::new(),
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            hooks: Default::default(),
            project_hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: vec![SessionCapability::Observe],
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(AllowAll)),
            shared_queue: None,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            ask_user: false,
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            hooks_warning: Arc::new(|_message: &str| {}),
            wake_warning: Arc::new(|_message: &str| {}),
            notice_lost_warning: Arc::new(|_message: &str| {}),
            intent_stop_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .unwrap();
    (scratch, Arc::new(session), jobs_dir)
}

/// A resume clears the jobs directory: a leftover file present before open is gone after.
/// Without the resume-path sweep call this file survives.
#[tokio::test]
async fn a_resume_sweeps_a_leftover_jobs_file() {
    let (scratch, session, jobs_dir) =
        open_with_leftover("sweep-resume", true, Some(("job-9-9.out", "stale\n"))).await;
    assert!(
        !jobs_dir.join("job-9-9.out").exists(),
        "a resumed session must clear its jobs directory"
    );
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

/// A fresh open never sweeps: a pre-created jobs directory keeps its file. The fresh open
/// path allows the pre-created directory (`ensure_private_dir` keeps an existing one), so
/// this proves the sweep call is resume-only; without the `if resume` guard the file would
/// be gone.
#[tokio::test]
async fn a_fresh_open_never_sweeps() {
    let (scratch, session, jobs_dir) =
        open_with_leftover("sweep-fresh", false, Some(("job-9-9.out", "stale\n"))).await;
    assert_eq!(
        std::fs::read_to_string(jobs_dir.join("job-9-9.out")).unwrap(),
        "stale\n",
        "a fresh open must not touch the jobs directory"
    );
    session.stop();
    let _ = session.join().await;
    drop(scratch);
}

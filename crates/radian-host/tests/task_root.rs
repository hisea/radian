//! ADR 0039 decision 4: the task worktree root lives under `~/.radian/worktrees/<repo-key>`
//! (`RADIAN_HOME` decides the root), and a symlink at **any** level of it is refused before
//! git runs.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test: it owns
//! its whole home, including the `worktrees/` directory itself — the level `tests/task.rs`
//! shares between its fixtures and therefore cannot swap.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agent_core::{NoopSink, TurnOptions};
use radian_host::{
    HostBindings, SessionError, SessionProjection, SessionQueue, SessionSetup, SpawnRequest,
    TaskBuilder, TaskContext, TaskRegistry, TaskSession, build_model,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::{Value, json};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radian-task-root-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
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

fn builder(session_dir: PathBuf) -> TaskBuilder {
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(Value::Null);
            let model = build_model(radian_host::ModelArgs {
                provider: radian_host::Provider::OpenAiCompatible,
                base_url: "http://127.0.0.1:1/v1".to_owned(),
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
            let model = radian_host::SwitchableModel::new(
                model,
                radian_host::ModelRoute::new("openai-compatible", "mock"),
                Arc::clone(&slot),
                radian_host::RouteContext {
                    providers: std::collections::BTreeMap::new(),
                    session_id: ctx.session_id.to_owned(),
                    user_headers: Vec::new(),
                    include_usage: None,
                    max_tokens: 1024,
                    thinking: "medium".to_owned(),
                    thinking_budget: None,
                },
            );
            let sandbox = workspace_sandbox(&workspace)
                .ok_or_else(|| fail("no sandbox backend here".into()))?;
            Ok(TaskSession {
                setup: SessionSetup {
                    session_id: radian_host::SessionId::new(ctx.session_id),
                    session_dir: session_dir.clone(),
                    workspace: workspace.clone(),
                    coding_root: Some(workspace.clone()),
                    model,
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
                    memory_root: None,
                    skills_extra_paths: Vec::new(),
                    skills_builtin: true,
                    skills_disabled: Vec::new(),
                    tools_allow: None,
                    skills_allow: None,
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
                    approvals: radian_host::ApprovalRouting::Queued,
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
                    task_builder: None,
                    named_builder: None,
                },
            })
        },
    )
}

/// The derivation under `RADIAN_HOME` and the refusal of a symlink at each level of the root
/// (ADR 0039 decision 4). One test per binary: `RADIAN_HOME` is process-wide.
#[cfg(unix)]
#[tokio::test]
async fn the_root_is_under_radian_home_and_every_level_is_checked_for_symlinks() {
    let scratch = scratch("home");
    let home = scratch.join("radian-home");
    let outside = scratch.join("outside");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // SAFETY: this test binary holds exactly one test.
    unsafe { std::env::set_var("RADIAN_HOME", &home) };

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
    let registry = TaskRegistry::new(
        lead.clone(),
        "lead",
        lead_journal,
        queue,
        builder(session_dir.clone()),
    );

    // The root is `<RADIAN_HOME>/worktrees/<repo-key>`, and its key names the repository.
    let root = registry
        .worktree_root()
        .expect("the lead is a git repository")
        .to_path_buf();
    assert_eq!(
        root.parent().unwrap(),
        home.join("worktrees"),
        "{}",
        root.display()
    );
    let key = root.file_name().unwrap().to_str().unwrap();
    assert!(key.starts_with("lead-"), "{key}");

    if workspace_sandbox(&lead).is_none() {
        return; // no sandbox backend here
    }
    let refused = |result: Result<Arc<radian_host::TaskRecord>, radian_host::TaskError>| {
        let error = match result {
            Ok(_) => panic!("a symlink at a root level must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("symlink"),
            "the error must name the symlink: {error}"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "the symlink target stays empty"
        );
    };

    // (a) the `worktrees/` level is a symlink.
    std::os::unix::fs::symlink(&outside, home.join("worktrees")).unwrap();
    refused(registry.spawn(SpawnRequest::new("t1")).await);
    std::fs::remove_file(home.join("worktrees")).unwrap();

    // (b) the `<repo-key>` level is a symlink.
    std::fs::create_dir_all(home.join("worktrees")).unwrap();
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    refused(registry.spawn(SpawnRequest::new("t2")).await);
    std::fs::remove_file(&root).unwrap();

    // (c) the leaf `<id>` is already a symlink.
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("t3")).unwrap();
    refused(registry.spawn(SpawnRequest::new("t3")).await);
    std::fs::remove_file(root.join("t3")).unwrap();

    // (d) with every level clean the worktree spawns under the root.
    let record = registry
        .spawn(SpawnRequest::new("t4"))
        .await
        .expect("spawn into a clean root");
    assert_eq!(record.worktree.clone().unwrap(), root.join("t4"));
    assert!(record.worktree.clone().unwrap().exists());
    registry.remove("t4", true).await.expect("remove");
}

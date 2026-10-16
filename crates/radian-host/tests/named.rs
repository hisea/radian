//! ADR 0030 slice 7 part b: the named-agent registry and the `send_agent`/`wait_agent` tools.
//!
//! Group A drives [`radian_host::NamedRegistry`] directly (a mock model for the turns). Group B
//! drives the lead tools. Every test uses a private `RADIAN_HOME`, so worktrees land under the
//! test's own root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_core::{CancellationToken, InputSource, NoopSink, ToolCall, ToolContext, ToolProvider};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    ApprovalRouting, HostBindings, NamedBuilder, NamedContext, NamedRegistry, SessionError,
    SessionId, SessionProjection, SessionQueue, SessionSetup, TaskBuilder, TaskContext,
    TaskRegistry, TaskSession, build_model,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::{RuntimeEvent, SessionCreatedData, SessionJournal};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-named-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A private `RADIAN_HOME` for this test binary (the named worktree root resolves through it).
/// It also holds the agent definitions the tests use, written once so parallel tests cannot race
/// on them.
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-named-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any registry is built.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        for (name, text) in [
            ("writer", "---\nname: writer\n---\nYou write.\n"),
            ("chief", "---\nname: chief\nspawn: true\n---\nCHIEF\n"),
            ("plain", "---\nname: plain\nspawn: false\n---\nPLAIN\n"),
            (
                "shared_writer",
                "---\nname: shared_writer\nisolation: shared\n---\nSHARED\n",
            ),
            (
                "shared_git",
                "---\nname: shared_git\nisolation: shared\ntools: [read, git_commit]\n---\nSHARED GIT\n",
            ),
        ] {
            std::fs::write(agents.join(format!("{name}.md")), text).unwrap();
        }
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

/// The named builder a test gives the registry: a mock model, and a `workspace` tier with
/// `on-ask` approval (so an `ask` really reaches the approver). A `spawn: true` definition is
/// handed a task builder (the dummy one) so `open_session` mounts the lead tools; a plain one is
/// not (the registry also strips it).
fn builder(session_dir: PathBuf, base_url: String) -> NamedBuilder {
    Arc::new(
        move |ctx: &NamedContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
            let sandbox = workspace_sandbox(&workspace)
                .ok_or_else(|| fail("no sandbox backend here".into()))?;
            let task_builder: Option<TaskBuilder> = if ctx.agent.spawn {
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
                        .with_approval(ApprovalPolicy::OnAsk),
                    access_builder: None,
                    task_agent: false,
                    parent: None,
                    allowed_branch: None,
                    git_commit_requires_branch: false,
                    // Deliberately non-`None`: `enforce_named_setup` must clear it (ADR 0030 O8),
                    // so this line is what the "no memory" test pins.
                    memory_root: Some(session_dir.join("memory")),
                    skills_extra_paths: Vec::new(),
                    skills_builtin: true,
                    skills_disabled: Vec::new(),
                    tools_allow: None,
                    skills_allow: None,
                    memory_injection: radian_host::memory::InjectionConfig::default(),
                    trust: radian_host::trust::Setup::untrusted(&workspace, false),
                    resume: false,
                    config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
                    turn_options: agent_core::TurnOptions::default(),
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
                    runtime_events: Arc::new(|_event: &RuntimeEvent| {}),
                    driver_stopped: Arc::new(|_| {}),
                    before_driver: None,
                    memory_warning: Arc::new(|_message: &str| {}),
                    memory_notice_line: Arc::new(|_line: &str| {}),
                    task_builder,
                    named_builder: None,
                },
            })
        },
    )
}

/// A named registry over a fresh git lead workspace.
struct Fixture {
    scratch: PathBuf,
    lead: PathBuf,
    session_dir: PathBuf,
    base_url: String,
    queue: Arc<SessionQueue>,
    lead_journal: Arc<SessionJournal>,
    registry: Arc<NamedRegistry>,
    task_registry: Arc<TaskRegistry>,
}

impl Fixture {
    fn new(label: &str, base_url: &str) -> Self {
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
        let registry = Arc::new(NamedRegistry::new(
            lead.clone(),
            Arc::clone(&lead_journal),
            Arc::clone(&queue),
            builder(session_dir.clone(), base_url.to_owned()),
        ));
        // A task registry the lead tools need; the named tests never spawn a task through it.
        let task_registry = Arc::new(TaskRegistry::new(
            lead.clone(),
            "lead",
            Arc::clone(&lead_journal),
            Arc::clone(&queue),
            Arc::new(|_ctx: &TaskContext<'_>| Err(SessionError::Message("unused".into()))),
        ));
        Self {
            scratch,
            lead,
            session_dir,
            base_url: base_url.to_owned(),
            queue,
            lead_journal,
            registry,
            task_registry,
        }
    }

    /// Builds a second named registry over the same lead, journal and queue, with an explicit
    /// orchestration snapshot (ADR 0030 O7 rev 2 item 4): the test seam for a named agent that
    /// must read the lead's trusted project agent dir.
    fn registry_with(
        &self,
        orchestration: radian_host::orchestration::Orchestration,
    ) -> Arc<NamedRegistry> {
        Arc::new(
            NamedRegistry::new(
                self.lead.clone(),
                Arc::clone(&self.lead_journal),
                Arc::clone(&self.queue),
                builder(self.session_dir.clone(), self.base_url.clone()),
            )
            .with_orchestration(orchestration),
        )
    }

    /// The lead's tool session, with both registries mounted.
    async fn lead_session(&self) -> radian_host::LeadSession {
        radian_host::LeadTools::new(Arc::clone(&self.task_registry))
            .with_named(Some(Arc::clone(&self.registry)))
            .open_session()
            .await
            .expect("a lead tool session")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

// ---------------------------------------------------------------------------
// Group A: the registry
// ---------------------------------------------------------------------------

/// The first `send` creates the session from its definition; the second reuses it: same session
/// id, same worktree (ADR 0030 O12).
#[tokio::test]
async fn first_send_creates_and_the_second_reuses() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let fixture = Fixture::new("reuse", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let first = fixture
        .registry
        .send("writer", "first", InputSource::User)
        .await
        .expect("first send");
    assert!(first.created, "the first send creates");
    assert!(!first.queued, "the fresh agent is idle");
    assert_eq!(first.name, "writer");
    assert_eq!(first.session_id, "writer");
    let worktree = first.worktree.clone().expect("a worktree");

    let second = fixture
        .registry
        .send("writer", "second", InputSource::User)
        .await
        .expect("second send");
    assert!(!second.created, "the second send reuses");
    assert_eq!(second.session_id, first.session_id, "same session id");
    assert_eq!(second.worktree.as_deref(), Some(worktree.as_path()));
    assert!(worktree.exists(), "the worktree is reused");

    // The lead's journal records the creation once.
    let lead = std::fs::read_to_string(fixture.session_dir.join("lead.jsonl")).unwrap();
    assert_eq!(
        lead.matches("NamedAgentStarted").count(),
        1,
        "one creation event:\n{lead}"
    );
    assert!(lead.contains("\"name\":\"writer\""), "{lead}");

    fixture.registry.close_all();
    let record = fixture.registry.get("writer").expect("the record");
    let _ = record.session.join().await;
}

/// ADR 0030 O7 rev 2 item 4 / ADR 0031 T1: a named agent's definition is read from the LEAD's
/// workspace (its trusted project `.radian/agents`), never from the named agent's own worktree; a
/// project file of the same name overrides the user-level one, and the snapshot is taken once, so
/// a later edit does not move the live session.
#[tokio::test]
async fn a_named_agent_reads_the_leads_project_definition_snapshot() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let fixture = Fixture::new("project-def", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    // The lead's trusted project dir, as the orchestration snapshot carries it (ADR 0030 O7).
    let project = fixture.lead.join(".radian").join("agents");
    std::fs::create_dir_all(&project).unwrap();
    let def = project.join("writer.md");
    std::fs::write(
        &def,
        "---\nname: writer\nisolation: shared\n---\nPROJECT WRITER\n",
    )
    .unwrap();
    let registry = fixture.registry_with(radian_host::orchestration::Orchestration {
        project_agents: Some(project.clone()),
        ..Default::default()
    });

    // The user-level `writer` names no isolation (a worktree); the project file sets `shared`, so
    // a `None` worktree proves the lead's project definition was read from the snapshot.
    let first = registry
        .send("writer", "first", InputSource::User)
        .await
        .expect("first send");
    assert!(first.created, "the first send creates");
    assert_eq!(
        first.worktree, None,
        "the project definition (isolation: shared) came from the lead's snapshot"
    );

    // Snapshot once: an edit to the file after creation does not move the live session.
    std::fs::write(
        &def,
        "---\nname: writer\nisolation: worktree\n---\nEDITED\n",
    )
    .unwrap();
    let second = registry
        .send("writer", "second", InputSource::User)
        .await
        .expect("second send");
    assert!(
        !second.created,
        "the live session is reused, not re-resolved"
    );
    assert_eq!(second.session_id, first.session_id, "same session id");
    assert_eq!(
        second.worktree, None,
        "the snapshot is not re-read after the session exists"
    );

    registry.close_all();
    let record = registry.get("writer").expect("the record");
    let _ = record.session.join().await;
}

/// A send while the agent is running is queued in the `next_turn` lane (ADR 0030 O4/O11).
#[tokio::test]
async fn a_busy_send_goes_to_the_next_turn_lane() {
    let server = MockServer::start(vec![
        // The first turn blocks on an approval, so it is still running for the second send.
        tool_reply("call_1", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("busy", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let first = fixture
        .registry
        .send("writer", "first", InputSource::User)
        .await
        .expect("first send");
    assert!(!first.queued);

    // Wait until the first turn is running (its shell call is pending).
    wait_pending(&fixture.queue).await;
    let record = fixture.registry.get("writer").expect("the record");
    wait_running(&record).await;

    let second = fixture
        .registry
        .send("writer", "second", InputSource::User)
        .await
        .expect("second send");
    assert!(second.queued, "the busy agent queues the input");

    // Both inputs are in the `next_turn` lane.
    let journal = std::fs::read_to_string(record.session.journal_path()).unwrap();
    let lanes = journal.matches("\"lane\":\"next_turn\"").count();
    assert!(lanes >= 2, "both sends are next_turn:\n{journal}");

    // Let the approval through so the turn can end.
    let pending = fixture.queue.head().expect("an approval");
    fixture
        .queue
        .answer(
            pending.request_id,
            radian_host::PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
        )
        .expect("answer");
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// A named agent never opens memory (ADR 0030 O8), sets the definition body as its role layer,
/// and records no parent (it is not a task).
#[tokio::test]
async fn a_named_agent_has_no_memory_and_is_not_a_task() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("nomem", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    fixture
        .registry
        .send("writer", "hi", InputSource::User)
        .await
        .expect("send");
    let record = fixture.registry.get("writer").expect("the record");
    assert!(record.session.memory().is_none(), "O8: no memory");
    assert!(
        record.session.lead_tasks().is_none(),
        "a plain named agent mounts no lead tools"
    );
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// A `spawn: true` definition is a lead and mounts the lead tools; a plain one does not
/// (ADR 0030 O13).
#[tokio::test]
async fn a_named_agent_mounts_the_lead_tools_only_when_spawn_is_true() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("spawn", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let _ = isolated_home();

    fixture
        .registry
        .send("chief", "hi", InputSource::User)
        .await
        .expect("chief");
    fixture
        .registry
        .send("plain", "hi", InputSource::User)
        .await
        .expect("plain");

    let chief = fixture.registry.get("chief").expect("chief");
    let plain = fixture.registry.get("plain").expect("plain");
    assert!(
        chief.session.lead_tasks().is_some(),
        "spawn: true is a lead"
    );
    assert!(
        plain.session.lead_tasks().is_none(),
        "spawn: false mounts no lead tools"
    );
    // ADR 0030: a named lead's integration branch is per-lead, so two leads
    // never share `radian/integration`.
    assert_eq!(
        chief
            .session
            .lead_tasks()
            .unwrap()
            .integration_branch()
            .as_str(),
        "radian/integration-chief"
    );

    fixture.registry.close_all();
    let _ = chief.session.join().await;
    let _ = plain.session.join().await;
}

/// ADR 0030 O13/O8: `max_tasks` caps **task** agents. A named agent is an O12 long session, not a
/// task, so it never occupies a task slot (the named registry does not touch the task queue).
#[tokio::test]
async fn a_named_agent_does_not_consume_a_task_slot() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("slot", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    fixture
        .registry
        .send("writer", "hi", InputSource::User)
        .await
        .expect("send");
    assert_eq!(
        fixture.task_registry.live_count(),
        0,
        "a named agent is not a task: it takes no max_tasks slot"
    );
    assert!(
        fixture.task_registry.queued().is_empty(),
        "and it queues nothing"
    );
    fixture.registry.close_all();
    let record = fixture.registry.get("writer").expect("record");
    let _ = record.session.join().await;
}

/// On lead exit the named sessions close and their worktrees stay (ADR 0030 O12).
#[tokio::test]
async fn closing_the_lead_closes_the_sessions_and_keeps_the_worktrees() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("close", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let outcome = fixture
        .registry
        .send("writer", "hi", InputSource::User)
        .await
        .expect("send");
    let worktree = outcome.worktree.clone().expect("a worktree");
    let record = fixture.registry.get("writer").expect("the record");
    // Wait for the turn to end so the driver is idle before the close.
    wait_turn_end(&record, &server, 1).await;

    fixture.registry.close_all();
    let end = record.session.join().await;
    assert!(
        matches!(end, radian_host::DriverEnd::Ok),
        "the driver stops cleanly: {end:?}"
    );
    assert!(
        worktree.exists(),
        "the worktree is kept: {}",
        worktree.display()
    );
    let _ = std::fs::remove_dir_all(&worktree);
}

/// ADR 0030 O3 (review fix 2): a `shared` named agent runs in the human's workspace and has no
/// branch of its own, so `git_commit` is not mounted; a call to it is an unknown tool.
#[tokio::test]
async fn a_shared_named_agent_does_not_mount_git_commit() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "git_commit", r#"{"message":"x"}"#),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("shared-commit", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    fixture
        .registry
        .send("shared_writer", "hi", InputSource::User)
        .await
        .expect("send");
    let record = fixture.registry.get("shared_writer").expect("record");
    // Wait only for the first model request first: its tool list is what the model was offered.
    wait_requests(&server, 1).await;
    let requests = server.requests();
    // The model was never offered `git_commit` (the request is the whole mounted tool list).
    let tools = requests[0].json()["tools"].clone();
    let names = tools.to_string();
    assert!(
        !names.contains("git_commit"),
        "a shared named agent must not be offered git_commit: {tools}"
    );
    // The tool list is real: the other built-ins are there, so the assertion above is not vacuous.
    assert!(
        names.contains("read") && names.contains("shell"),
        "the mounted built-ins are offered: {tools}"
    );
    // Two model requests: the scripted `git_commit` call, then the model's reply to its result.
    wait_requests(&server, 2).await;
    let second = server.requests()[1].json().to_string();
    // Calling it anyway is an unknown tool (the model's next request carries that result).
    assert!(
        second.contains("There is no tool named `git_commit`"),
        "a git_commit call is an unknown tool: {second}"
    );

    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// ADR 0030 O3/O11 (review fix 2): a `shared` definition that lists `git_commit` in `tools` is
/// warned about in the send result — the tool is not mounted, so the list silently loses it.
#[tokio::test]
async fn a_shared_send_warns_about_a_git_commit_the_definition_listed() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("shared-git-warn", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let out = run_tool(
        &session,
        "send_agent",
        json!({"name": "shared_git", "brief": "do it"}),
    )
    .await
    .expect("send_agent");
    let value = structured(&out);
    let warnings = value["warnings"].to_string();
    assert!(
        warnings.contains("git_commit") && warnings.contains("shared"),
        "the send result must warn that git_commit is not mounted: {value}"
    );
    let record = fixture.registry.get("shared_git").expect("record");
    assert!(
        record
            .warnings
            .iter()
            .any(|w| w.contains("git_commit") && w.contains("shared")),
        "{:?}",
        record.warnings
    );
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

// ---------------------------------------------------------------------------
// Group B: the source rule
// ---------------------------------------------------------------------------

/// A `User` send runs the named agent's turn as `User`, so its `ask` pops the human approval
/// (ADR 0030 O12 / ADR 0012 decision 5).
#[tokio::test]
async fn a_user_send_pops_the_human_approval() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("user-ask", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    fixture
        .registry
        .send("writer", "hi", InputSource::User)
        .await
        .expect("send");
    let record = fixture.registry.get("writer").expect("the record");
    assert_eq!(
        record.session.hooks().turn_source(),
        Some(InputSource::User),
        "a /send turn is User"
    );

    let pending = wait_pending(&fixture.queue).await;
    match &pending.request {
        radian_host::PendingRequest::Approval(request) => assert_eq!(request.tool, "shell"),
        other => panic!("expected an approval, got {other:?}"),
    }
    // The named agent's journal records the `user` source (ADR 0011 L2).
    let journal = std::fs::read_to_string(record.session.journal_path()).unwrap();
    assert!(journal.contains("\"source\":\"user\""), "{journal}");

    fixture
        .queue
        .answer(
            pending.request_id,
            radian_host::PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
        )
        .expect("answer");
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// A `Subagent` send lowers the named agent's turn, so its `ask` is refused and no approval
/// reaches the human queue (ADR 0030 O12 / ADR 0012 decision 5).
#[tokio::test]
async fn a_subagent_send_lowers_the_turn_and_the_ask_is_refused() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("sub-ask", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    fixture
        .registry
        .send("writer", "hi", InputSource::Subagent)
        .await
        .expect("send");
    let record = fixture.registry.get("writer").expect("the record");
    assert_eq!(
        record.session.hooks().turn_source(),
        Some(InputSource::Subagent),
        "a send_agent turn is Subagent"
    );
    // The turn continues past the refused ask (the model gets a denied tool result), so it ends;
    // no approval ever appears in the human queue.
    wait_turn_end(&record, &server, 2).await;
    assert!(
        fixture.queue.head().is_none(),
        "a Subagent ask must not reach the human queue"
    );
    let journal = std::fs::read_to_string(record.session.journal_path()).unwrap();
    assert!(journal.contains("\"source\":\"subagent\""), "{journal}");

    fixture.registry.close_all();
    let _ = record.session.join().await;
}

// ---------------------------------------------------------------------------
// Group C: the lead tools
// ---------------------------------------------------------------------------

/// `send_agent` creates/queues and returns at once; `wait_agent` returns the bounded reply.
#[tokio::test]
async fn send_agent_and_wait_agent_round_trip() {
    let server = MockServer::start(vec![text_reply("the reply")]).await;
    let fixture = Fixture::new("tools", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let out = run_tool(
        &session,
        "send_agent",
        json!({"name": "writer", "brief": "do it"}),
    )
    .await
    .expect("send_agent");
    let value = structured(&out);
    assert_eq!(value["agent"], "writer");
    assert_eq!(value["created"], true);
    // `send_agent` enqueues as `Subagent` (ADR 0030 O12): the named turn is lowered.
    let record = fixture.registry.get("writer").expect("record");
    assert_eq!(
        record.session.hooks().turn_source(),
        Some(agent_core::InputSource::Subagent),
        "send_agent is Subagent"
    );
    let out = run_tool(
        &session,
        "wait_agent",
        json!({"name": "writer", "timeout_secs": 20}),
    )
    .await
    .expect("wait_agent");
    let value = structured(&out);
    assert_eq!(value["event"], "finished", "{value}");
    assert_eq!(value["summary"], "the reply");

    // A second send reuses the session.
    let out = run_tool(
        &session,
        "send_agent",
        json!({"name": "writer", "brief": "again"}),
    )
    .await
    .expect("send_agent");
    assert_eq!(structured(&out)["created"], false);
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// `wait_agent` on an agent whose turn never ends times out with `still_running`.
#[tokio::test]
async fn wait_agent_times_out() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("tools-timeout", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    // Create the agent with a `User` send so its shell ask reaches the human queue and the turn
    // blocks (send_agent would be Subagent, whose ask is refused).
    fixture
        .registry
        .send("writer", "do it", InputSource::User)
        .await
        .expect("send");
    let _pending = wait_pending(&fixture.queue).await;
    let out = run_tool(
        &session,
        "wait_agent",
        json!({"name": "writer", "timeout_secs": 0.05}),
    )
    .await
    .expect("wait_agent");
    assert_eq!(structured(&out)["event"], "still_running", "{out:?}");
    // A timeout never cancels the agent: it is still running.
    let record = fixture.registry.get("writer").expect("record");
    assert!(record.running(), "the timeout must not cancel the turn");
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// ADR 0030 O12 (review fix 1): a turn that ends **before** `wait_agent` is called is not
/// missed. `send_agent` returns a `since` token and `wait_agent` with it returns at once.
#[tokio::test]
async fn wait_agent_with_the_since_token_sees_a_turn_that_already_ended() {
    let server = MockServer::start(vec![text_reply("the reply")]).await;
    let fixture = Fixture::new("tools-since", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    let out = run_tool(
        &session,
        "send_agent",
        json!({"name": "writer", "brief": "do it"}),
    )
    .await
    .expect("send_agent");
    let since = structured(&out)["since"].as_u64().expect("a since token");
    let record = fixture.registry.get("writer").expect("record");
    // Let the (mock) turn finish before the wait: the old wait then looked for a turn that never
    // came and timed out with `still_running`.
    wait_turn_end(&record, &server, 1).await;
    let out = run_tool(
        &session,
        "wait_agent",
        json!({"name": "writer", "since": since, "timeout_secs": 5}),
    )
    .await
    .expect("wait_agent");
    let value = structured(&out);
    assert_eq!(value["event"], "finished", "{value}");
    assert_eq!(value["summary"], "the reply");
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

/// ADR 0030 O12 (review fix 2): a send queued behind a running turn — the common busy case — must
/// wait until **its own** turn ends, not the turn it was queued behind. The token is the
/// generation one past the running turn (`G + 1`): with the running turn's generation (`G`) the
/// wait would settle on that turn's reply in the idle moment before the queued turn starts. Both
/// turns are held by a human approval (a gate the test releases, no sleep), so the wait is polled
/// while the queued turn is still running and must not return the first turn's text.
#[tokio::test]
async fn a_queued_send_waits_past_the_running_turn() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", r#"{"command":"echo one"}"#),
        text_reply("one"),
        tool_reply("call_2", "shell", r#"{"command":"echo two"}"#),
        text_reply("two"),
    ])
    .await;
    let fixture = Fixture::new("tools-busy-since", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let session = fixture.lead_session().await;
    // Turn 1: a `User` send whose shell ask blocks on the human queue, so the agent is busy.
    fixture
        .registry
        .send("writer", "one", InputSource::User)
        .await
        .expect("send one");
    let record = fixture.registry.get("writer").expect("record");
    let first = wait_pending(&fixture.queue).await;
    wait_running(&record).await;

    // Turn 2 is sent while turn 1 runs, so it goes to the `next_turn` lane behind it. Its token
    // is the generation at the enqueue **plus one**: the running turn ends first.
    let generation = record.turn_generation();
    let second = fixture
        .registry
        .send("writer", "two", InputSource::User)
        .await
        .expect("send two");
    assert!(second.queued, "the busy agent queues the input");
    assert_eq!(
        second.turn,
        generation + 1,
        "the token waits past the running turn (G + 1), not at the enqueue (G)"
    );

    // Release turn 1. Turn 2 starts and is itself held on its shell ask, so the wait below is
    // polled while turn 2 runs: it must not settle for turn 1's "one".
    fixture
        .queue
        .answer(
            first.request_id,
            radian_host::PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
        )
        .expect("answer turn 1");
    let pending = wait_pending(&fixture.queue).await;
    assert!(
        record.running(),
        "turn 2 is still running when the wait is polled"
    );
    let out = run_tool(
        &session,
        "wait_agent",
        json!({"name": "writer", "since": second.turn, "timeout_secs": 0.1}),
    )
    .await
    .expect("wait_agent");
    assert_eq!(
        structured(&out)["event"],
        "still_running",
        "the wait must not settle for the running turn's reply"
    );

    // Release turn 2: the wait with the busy token now returns turn 2's reply, never turn 1's.
    fixture
        .queue
        .answer(
            pending.request_id,
            radian_host::PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
        )
        .expect("answer turn 2");
    let out = run_tool(
        &session,
        "wait_agent",
        json!({"name": "writer", "since": second.turn, "timeout_secs": 20}),
    )
    .await
    .expect("wait_agent");
    let value = structured(&out);
    assert_eq!(value["event"], "finished", "{value}");
    assert_eq!(
        value["summary"], "two",
        "the wait returns the queued send's reply, not the turn it was queued behind"
    );
    fixture.registry.close_all();
    let _ = record.session.join().await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

async fn run_tool(
    session: &radian_host::LeadSession,
    name: &str,
    arguments: Value,
) -> Result<agent_core::ToolOutput, agent_core::ToolFailure> {
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
    radian_host::run_lead_tool(session, &call, cx).await
}

fn structured(output: &agent_core::ToolOutput) -> Value {
    output.structured.clone().expect("structured output")
}

/// Waits until the mock server has seen at least `want` model requests.
async fn wait_requests(server: &MockServer, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while server.requests().len() < want {
        assert!(
            std::time::Instant::now() < deadline,
            "the model did not make {want} request(s)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the lead queue has a request, then returns it.
async fn wait_pending(
    queue: &Arc<SessionQueue>,
) -> radian_host::PendingView<radian_host::PendingRequest> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(head) = queue.head() {
            return head;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no approval reached the human queue"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the named agent reports its turn is running.
async fn wait_running(record: &Arc<radian_host::NamedRecord>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !record.running() {
        assert!(
            std::time::Instant::now() < deadline,
            "the named turn never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits until the named agent's turn is idle after at least `want` model requests.
async fn wait_turn_end(record: &Arc<radian_host::NamedRecord>, server: &MockServer, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let idle = !record.running() && record.last_outcome().is_some();
        if idle && server.requests().len() >= want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the named turn did not finish: {} requests",
            server.requests().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

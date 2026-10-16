//! ADR 0030 slice 6b2: the **production** task builder ([`radian_host::task_host`]).
//!
//! The registry enforces the task invariants on whatever a builder returns; these tests pin
//! that the production builder's result passes those invariants and that a task's own stream
//! never reaches the lead's live sink (the notifier is the only lead-visible channel).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{Provider, SpawnRequest, TaskHost, TaskRegistry, build_model, task_builder};
use radian_permissions::NoopAudit;
use radian_runtime::ConfigSlot;
use radian_sandbox::GitAccess;
use radian_session::{RuntimeEvent, SessionCreatedData, SessionJournal, TurnOutcome};
use serde_json::{Value, json};

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "radian-taskhost-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-taskhost-home-{}", std::process::id()));
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

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

/// A recorder for the runtime events one sink sees.
type Seen = Arc<Mutex<Vec<RuntimeEvent>>>;

fn recorder() -> Seen {
    Arc::new(Mutex::new(Vec::new()))
}

/// The production builder, wired to a mock provider and the two recording sinks.
fn host(base_url: &str, workspace: &Path, task_seen: Seen) -> TaskHost {
    let base_url = base_url.to_owned();
    let model: radian_host::TaskModelFactory = Arc::new(
        move |session_id: &str,
              slot: Arc<ConfigSlot>,
              _route: Option<radian_host::ModelRoute>,
              _thinking: Option<String>| {
            let initial = build_model(radian_host::ModelArgs {
                provider: Provider::OpenAiCompatible,
                base_url: base_url.clone(),
                model: "mock".into(),
                api_key: Some("sk-test".into()),
                headers: Vec::new(),
                include_usage: None,
                max_tokens: 1024,
                thinking: radian_host::Thinking::Off,
                reasoning_effort: None,
                reasoning_replay: None,
            })
            .map_err(|error| error.to_string())?;
            Ok(radian_host::SwitchableModel::new(
                initial,
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
            ))
        },
    );
    TaskHost {
        session_dir: workspace.join("sessions"),
        model,
        route: ("openai-compatible".into(), "mock".into()),
        servers: Vec::new(),
        sandbox: None,
        sandbox_deny_read: Vec::new(),
        allow_unix_sockets: Vec::new(),
        retries: 0,
        context_budget: None,
        auto_compact: false,
        compaction_settings: Value::Null,
        compaction_settings_for: None,
        turn_options: agent_core::TurnOptions::default(),
        turn_timeout: None,
        wording: radian_host::CLI_WORDING,
        protected_branches: Vec::new(),
        allowed_filters: None,
        shell_auto_background: None,
        skills_extra_paths: Vec::new(),
        skills_builtin: true,
        skills_disabled: Vec::new(),
        memory_injection: radian_host::memory::InjectionConfig::default(),
        capabilities: radian_host::SessionCapability::ALL.to_vec(),
        audit: Arc::new(|_journal| Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>),
        task_events: Arc::new({
            let seen = Arc::clone(&task_seen);
            move |event: &RuntimeEvent| seen.lock().unwrap().push(event.clone())
        }),
    }
}

struct Fixture {
    scratch: PathBuf,
    lead: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
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
        Self { scratch, lead }
    }

    fn registry(&self, base_url: &str, task_seen: Seen, lead_seen: Seen) -> Arc<TaskRegistry> {
        self.registry_with_host(host(base_url, &self.scratch, task_seen), lead_seen)
    }

    /// [`Fixture::registry`] over a caller-supplied [`TaskHost`] (so a test can wrap the model
    /// factory).
    fn registry_with_host(&self, host: TaskHost, lead_seen: Seen) -> Arc<TaskRegistry> {
        let session_dir = self.scratch.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let journal = Arc::new(
            SessionJournal::create(
                session_dir.join("lead.jsonl"),
                "lead",
                SessionCreatedData::new(self.lead.to_string_lossy(), json!({})),
            )
            .expect("the lead journal"),
        );
        let queue = Arc::new(radian_host::SessionQueue::new());
        let notifier: radian_host::TaskNotifier = Arc::new(move |event: &RuntimeEvent| {
            lead_seen.lock().unwrap().push(event.clone());
        });
        Arc::new(
            TaskRegistry::new(
                self.lead.clone(),
                "lead",
                journal,
                queue,
                task_builder(host),
            )
            .with_notifier(notifier),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

async fn wait_outcome(registry: &Arc<TaskRegistry>, id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let record = registry.get(id).expect("the task");
        if !record.running() && record.last_outcome().is_some() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the task's turn never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The production builder builds a session the registry accepts, with the task invariants in
/// place: the worktree as workspace, `network off` and `.git` read-only, no MCP servers, no lead
/// tools. The task's own stream reaches its own sink, never the lead's.
#[tokio::test]
async fn the_production_builder_builds_an_accepted_session() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("build");
    let task_seen = recorder();
    let lead_seen = recorder();
    let registry = fixture.registry(
        &server.base_url,
        Arc::clone(&task_seen),
        Arc::clone(&lead_seen),
    );

    let mut request = SpawnRequest::new("t1");
    request.brief = Some("do it".into());
    let record = registry
        .spawn(request)
        .await
        .expect("the registry accepts it");

    // The worktree, its branch, and the task's own workspace root.
    let worktree = record.worktree.clone().expect("a worktree");
    assert!(worktree.exists(), "{}", worktree.display());
    assert_eq!(record.branch.as_deref(), Some("radian/t1"));
    assert_eq!(
        std::fs::canonicalize(record.session.workspace()).unwrap(),
        std::fs::canonicalize(&worktree).unwrap()
    );

    // The sandbox: a `workspace` policy with the network off and `.git` read-only.
    let policy = record.sandbox_policy.as_ref().expect("a sandbox policy");
    assert!(!policy.network, "a task sandbox has the network off");
    assert_eq!(policy.git_access, GitAccess::ReadOnly);
    assert_eq!(
        policy.workspace,
        std::fs::canonicalize(&worktree).unwrap(),
        "the sandbox is rooted at the worktree"
    );

    // No MCP servers (ADR 0030 O8), no memory, and no lead tools (depth limit 2).
    assert_eq!(record.session.facts().mcp_configured, 0);
    assert!(
        record.session.lead_tasks().is_none(),
        "a task session mounts no lead tools"
    );

    wait_outcome(&registry, "t1").await;
    assert_eq!(record.last_outcome(), Some(TurnOutcome::Completed));

    // The task's own stream reached its own sink...
    let task_events = task_seen.lock().unwrap().clone();
    assert!(
        task_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::TurnFinished { .. } | RuntimeEvent::TurnStarted { .. }
        )),
        "the task's own sink sees its turn: {task_events:?}"
    );
    // ...and not the lead's, which only ever sees the spawn (ADR 0030 O10).
    let lead_events = lead_seen.lock().unwrap().clone();
    assert!(
        lead_events.iter().all(|event| matches!(
            event,
            RuntimeEvent::TaskSpawned { .. } | RuntimeEvent::TaskFinished { .. }
        )),
        "the task's stream must not reach the lead's live sink: {lead_events:?}"
    );
    assert!(
        lead_events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TaskSpawned { .. })),
        "the lead's sink sees the spawn notice: {lead_events:?}"
    );

    registry.remove("t1", true).await.expect("remove");
}

/// A `shared` task has no worktree and is forced read-only, still with no MCP and no lead tools.
#[tokio::test]
async fn a_shared_task_has_no_worktree_and_no_network() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("shared");
    let registry = fixture.registry(&server.base_url, recorder(), recorder());

    let mut request = SpawnRequest::new("t1");
    request.isolation = Some(radian_host::Isolation::Shared);
    request.brief = Some("do it".into());
    let record = registry
        .spawn(request)
        .await
        .expect("the registry accepts it");
    assert!(record.worktree.is_none(), "a shared task has no worktree");
    assert!(record.sandbox_policy.is_none(), "shared: no sandbox");
    assert_eq!(record.session.facts().mcp_configured, 0);
    assert!(record.session.lead_tasks().is_none());

    wait_outcome(&registry, "t1").await;
    registry.remove("t1", true).await.expect("remove");
}

/// ADR 0030 O12: `/tasks kill` closes the session and keeps the worktree, unlike `remove`.
#[tokio::test]
async fn kill_closes_the_task_and_keeps_the_worktree() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("kill");
    let registry = fixture.registry(&server.base_url, recorder(), recorder());

    let record = registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("the registry accepts it");
    let worktree = record.worktree.clone().expect("a worktree");

    registry.kill("t1").await.expect("kill");
    assert!(registry.get("t1").is_none(), "the record is gone");
    assert!(
        worktree.exists(),
        "kill keeps the worktree: {}",
        worktree.display()
    );
    // The branch is still there too.
    assert!(git(&fixture.lead, &["rev-parse", "--verify", "radian/t1"]));
}

/// ADR 0030 O11: the production builder hands the resolved agent definition to the model
/// factory, so a `spawn_task { agent }` runs the definition's route and thinking level (the
/// factory resolves that route's credential the same way `/model` does).
#[tokio::test]
async fn the_builder_hands_the_agent_definition_to_the_model_factory() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("factory-agent");
    let home = isolated_home();
    let agents = home.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let def_path = agents.join("route-agent.md");
    std::fs::write(
        &def_path,
        "---\nname: route-agent\nmodel: opencode-go/one\nthinking: low\n---\nBODY\n",
    )
    .unwrap();

    // A recorder for the `provider / thinking` the factory was handed.
    type SeenAgent = Arc<Mutex<Option<(String, Option<String>)>>>;
    let seen: SeenAgent = Arc::new(Mutex::new(None));
    let mut host = host(&server.base_url, &fixture.scratch, recorder());
    let inner = Arc::clone(&host.model);
    let seen_factory = Arc::clone(&seen);
    host.model = Arc::new(
        move |session_id: &str,
              slot: Arc<ConfigSlot>,
              route: Option<radian_host::ModelRoute>,
              thinking: Option<String>| {
            *seen_factory.lock().unwrap() = Some((
                route
                    .as_ref()
                    .map(|route| route.provider.clone())
                    .unwrap_or_default(),
                thinking.clone(),
            ));
            inner(session_id, slot, route, thinking)
        },
    );

    let registry = fixture.registry_with_host(host, recorder());
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("route-agent".into());
    request.brief = Some("do it".into());
    let _record = registry.spawn(request).await.expect("spawn");
    wait_outcome(&registry, "t1").await;

    let seen = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the factory saw a definition");
    assert_eq!(seen.0, "opencode-go");
    assert_eq!(seen.1.as_deref(), Some("low"));

    registry.remove("t1", true).await.expect("remove");
    let _ = std::fs::remove_file(def_path);
}

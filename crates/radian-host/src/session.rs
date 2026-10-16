//! The Session Facade (ADR 0023 U5): the UI's one handle on a session, and the in-process
//! implementation `open_session` builds for both binaries.
//!
//! The facade is a **door, not an owner**: the inbox, the driver, the journal, the tool
//! session and the approval routing belong to the runtime and the host. The UI holds an
//! `Arc<dyn SessionFacade>`, reads the projection through it, and sends commands; a command a
//! facade does not declare is refused, and `LocalSession` checks again when it runs one — the
//! host does not trust the UI's copy of the capability list.
//!
//! `open_session` is the one place that establishes a session: the journal, the permission and
//! memory hooks, the combined tools, the driver, the inbox and the projection. `agent-cli` and
//! `radian-tui` differ only in what they inject (their approver, audit sink, event sink,
//! runtime-event handler, wording and startup lines), which is [`HostBindings`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::{
    Agent, AgentConfig, ContextBudget, EventSink, Hooks, Message, ModelUsage, TurnOptions,
};
use agent_extensions::{McpServerConfig, McpToolProvider};
use radian_coding::CodingTools;
use radian_permissions::{ApprovalPolicy, Approver, AuditSink, PermissionHooks, Preset, Profile};
use radian_runtime::{
    BudgetFor, CancelCause, CompactionSettingsFor, ConfigQueue, ConfigSlot, Driver, Inbox,
    ModelRoute, RuntimeEvents,
};
use radian_sandbox::{Sandbox, SandboxSlot};
use radian_session::{ForkParent, SessionCreatedData, SessionJournal};

use crate::cost::RoutePrice;
use crate::describe::{Wording, describe, sandbox_word};
use crate::home::{radian_home, skills_dir, user_home};
use crate::memory::{
    InjectionConfig, MemoryHooks, MemoryService, MemoryTools, ProjectState,
    injection::MemoryInjection,
};
use crate::model::SwitchableModel;
use crate::pending::{
    Answered, PendingQueue, PendingRequest, PendingResponse, QueuedApprover, QueuedPrompter,
};
use crate::projection::{ProjectionRead, SessionProjection};
use crate::skills::tools::SkillTools;
use crate::skills::{self, Skill, SkillsHooks};
use crate::tools::HostTools;
use crate::trust;

/// A session's stable identifier: the session file's name and `SessionCreated.session_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a facade can do (ADR 0023 U5). The UI must not infer capability from the concrete
/// type; it reads this list, and the facade re-checks it when a command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCapability {
    /// Read the projection.
    Observe,
    /// Start a new turn.
    SendMessage,
    /// Redirect the running turn.
    Steer,
    /// Cancel the running turn.
    Cancel,
    /// Answer a pending approval or question.
    AnswerRequest,
}

impl SessionCapability {
    /// Every capability the in-process session serves; what a normal host declares.
    pub const ALL: [SessionCapability; 5] = [
        SessionCapability::Observe,
        SessionCapability::SendMessage,
        SessionCapability::Steer,
        SessionCapability::Cancel,
        SessionCapability::AnswerRequest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::SendMessage => "send",
            Self::Steer => "steer",
            Self::Cancel => "cancel",
            Self::AnswerRequest => "answer",
        }
    }
}

/// A command the UI asks a session to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    /// Start a new turn: `Inbox::followup`. A [`Message`], not a string: a send can carry an
    /// attached file as its own part (ADR 0018).
    SendFollowup(Message),
    /// Redirect the running turn: `Inbox::steer`.
    Steer(Message),
    /// Cancel the running turn.
    Cancel,
    /// Enqueue a message that does **not** wake an idle driver (ADR 0033's `!` output, ADR
    /// 0011's `inject`): it joins the next turn's context without starting one.
    Inject(Message),
}

/// One `!`/`!!` command's audit record (ADR 0033): the durable `UserBash` journal entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserBashRecord {
    pub command: String,
    pub exclude_from_context: bool,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
}

impl SessionCommand {
    /// The capability a session must declare to accept this command.
    pub fn capability(&self) -> SessionCapability {
        match self {
            Self::SendFollowup(_) | Self::Inject(_) => SessionCapability::SendMessage,
            Self::Steer(_) => SessionCapability::Steer,
            Self::Cancel => SessionCapability::Cancel,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::SendFollowup(_) => "send",
            Self::Inject(_) => "inject",
            Self::Steer(_) => "steer",
            Self::Cancel => "cancel",
        }
    }
}

/// A command was refused. `undeclared` is the capability check; any other reason is the inbox
/// rejecting the input (closed, full, not a user message).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{command}` was refused: {reason}")]
pub struct CommandRefused {
    pub command: String,
    /// `Some` when the facade does not declare the capability; `None` when the inbox rejected
    /// the input (closed, full, not a user message).
    pub capability: Option<String>,
    pub reason: String,
}

impl CommandRefused {
    /// The facade does not declare the capability the command needs.
    pub fn undeclared(command: &SessionCommand) -> Self {
        let capability = command.capability();
        Self {
            command: command.name().to_string(),
            capability: Some(capability.as_str().to_string()),
            reason: format!(
                "this session does not declare the `{}` capability",
                capability.as_str()
            ),
        }
    }

    /// The capability check passed; the inbox refused the input.
    pub fn inbox(command: &str, error: &radian_runtime::InboxError) -> Self {
        Self {
            command: command.to_string(),
            capability: None,
            reason: error.to_string(),
        }
    }
}

/// The UI's one handle on a session (ADR 0023 U5).
pub trait SessionFacade: Send + Sync {
    fn id(&self) -> &SessionId;
    fn capabilities(&self) -> &[SessionCapability];
    fn projection(&self) -> ProjectionRead<'_>;
    fn command(&self, command: SessionCommand) -> Result<(), CommandRefused>;

    /// The pending requests (ADR 0023 U7): the UI shows the head and counts the queue.
    fn pending(&self) -> &SessionQueue;

    /// Answer a pending request by its queue id (ADR 0023 U5's `AnswerRequest`). A late answer
    /// is silently ignored (ADR 0024 Q4): the request is already gone, and re-completing it is
    /// worse than dropping the answer.
    fn answer(&self, request_id: u64, response: PendingResponse) -> Result<(), CommandRefused>;

    /// Ask the runtime to compact the context (the TUI's `/compact`). `command` cannot carry
    /// the outcome, and the UI must not hold the inbox, so the result comes back through
    /// `on_done`. Needs the same capability as sending.
    fn compact(
        &self,
        instructions: Option<String>,
        on_done: Box<dyn FnOnce(Result<agent_core::CompactionOutcome, String>) + Send>,
    );

    /// Like [`SessionFacade::compact`], with the budget the compaction targets (ADR 0026 D7):
    /// the `/model` confirm compaction aims at the **new** model's `B'`.
    fn compact_with_budget(
        &self,
        instructions: Option<String>,
        budget_tokens: Option<u64>,
        on_done: Box<dyn FnOnce(Result<agent_core::CompactionOutcome, String>) + Send>,
    );

    /// Queues a configuration change for the next turn boundary (ADR 0026 D5). A route never
    /// carries a key or a header value.
    fn queue_config(&self, change: serde_json::Value);

    /// The model route in force (ADR 0026 D8): the resumed `ConfigChanged` if the session has
    /// one, else the startup route from the snapshot.
    fn model_route(&self) -> Option<(String, String)>;

    /// `B` for a switched route, from the local catalog only (ADR 0026 D7). `None` when the
    /// window is unknown — the app then queues without a comparison.
    fn budget_for_route(&self, provider: &str, model: &str) -> Option<u64>;

    /// Switch the session's approval policy in place (ADR 0026 D9's in-session `auto`).
    /// The hooks audit the switch and the facade queues the durable `ConfigChanged` record
    /// (`{"approval": …}`) the driver journals at the next turn boundary. Needs the same
    /// capability as answering: it belongs to the interactive approver.
    fn set_approval(&self, policy: ApprovalPolicy) -> Result<(), CommandRefused>;

    /// Switch the session's permission tier in place (ADR 0038 slice 2's `/access`). The
    /// profile and the sandbox are rebuilt through the host's [`AccessBuilder`]; a failed
    /// sandbox probe is a refusal that changes nothing. The switch takes effect on the next
    /// tool call (a running command keeps its sandbox). The durable `ConfigChanged`
    /// (`{"access": …}`) is queued for the driver to journal at the turn boundary.
    fn set_access(&self, tier: Preset) -> Result<(), CommandRefused>;

    /// Switch the session's working mode (ADR 0038 decision 1's `/mode`). Layer 1 (the role
    /// prompt) changes at the **next** model request: the change is queued for the next turn
    /// boundary and journaled as a `ConfigChanged` (`{"mode": …}`), so a resume restores it.
    /// Session only — never written to a config file. A task agent's role comes from its brief
    /// and cannot be switched, and neither can a session whose layer 1 is fixed (`--system`,
    /// an explicit role).
    fn set_mode(&self, mode: crate::prompt::Mode) -> Result<(), CommandRefused>;

    /// The discovered skills, in discovery order (ADR 0025 S1). The UI lists them (`/skills`)
    /// and completes their names (`/skill:`); the host owns the list because it owns discovery.
    fn skills(&self) -> &[Skill];

    /// Expands `/skill:<name> [args]` into the message ADR 0025 S3 sends to the inbox. `Err` is
    /// a notice (an unknown skill or an unreadable file), never a turn failure.
    fn expand_skill(&self, name: &str, args: &str) -> Result<String, String>;

    /// The project-trust state (ADR 0031 T3): what the session starts with, and the source the
    /// startup line shows.
    fn trust(&self) -> trust::Setup;

    /// Applies a startup trust choice (ADR 0031 T3): writes `trust.json` for the persistent
    /// choices and journals `ProjectTrustChanged`, or changes only this session's state.
    fn record_trust(&self, choice: trust::Choice) -> Result<trust::Setup, String>;

    /// Appends one durable `UserBash` audit record (ADR 0033). Both `!` and `!!` call it; the
    /// record is audit-only and never replayed.
    fn record_user_bash(&self, record: &UserBashRecord);

    /// The session's background jobs (`/jobs`, ADR 0037 decision 4). Empty for a host with no
    /// coding tools.
    fn jobs(&self) -> Vec<radian_coding::JobSummary>;

    /// Stop one background job (`/jobs kill <id>`). `Err` is a notice (an unknown id, or a
    /// host with no jobs).
    fn kill_job(&self, id: &str) -> Result<(), String>;

    /// Stop every running background job (`/jobs kill all`).
    fn kill_all_jobs(&self);
}

/// How the driver task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEnd {
    /// The driver ended cleanly.
    Ok,
    /// The driver returned an error.
    Failed(String),
    /// The task panicked, was aborted, or its watcher is gone.
    Panicked(String),
}

impl DriverEnd {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// The driver's task handle: the host asks it to stop or aborts it, and waits once.
struct DriverTask {
    stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    abort: tokio::task::AbortHandle,
    done: Mutex<Option<tokio::sync::oneshot::Receiver<DriverEnd>>>,
}

/// Why a session could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{0}")]
    Message(String),
}

/// The parts a tier switch rebuilds (ADR 0038 slice 2): the permission profile and the
/// sandbox for the new tier. The sandbox is `None` for `safe`.
pub struct AccessParts {
    pub profile: Profile,
    pub sandbox: Option<Sandbox>,
}

/// Rebuilds [`AccessParts`] for a tier. The host/binary owns the inputs (the workspace, the
/// sandbox deny-read list, the Unix-socket whitelist, the `--allow-shell-command` rules), so
/// the session calls back into it instead of guessing. A failed sandbox probe returns `Err`,
/// which the facade turns into a refusal: the switch never degrades (ADR 0012).
pub type AccessBuilder = Arc<dyn Fn(Preset) -> Result<AccessParts, String> + Send + Sync>;

/// The values a host resolves before the session exists. Both binaries build this from their
/// own flags and config; `open_session` does the rest.
pub struct SessionSetup {
    pub session_id: SessionId,
    /// Where session files live (the host resolved `--session-dir`/`RADIAN_SESSION_DIR`/home).
    pub session_dir: PathBuf,
    /// The canonical workspace the tools and the permission rules are rooted at.
    pub workspace: PathBuf,
    /// The `--workspace` flag: the built-in coding tools exist only when it was given.
    pub coding_root: Option<PathBuf>,
    /// The switchable model (ADR 0026 D5): the startup model wrapped in the route cache.
    pub model: SwitchableModel,
    /// The slot the model reads and the driver writes (one instance, shared).
    pub config_slot: Arc<ConfigSlot>,
    /// The queue `/model` pushes into and the driver drains at the turn boundary (D5).
    pub config_queue: Arc<ConfigQueue>,
    /// Computes `B` for a switched model from the local catalog (ADR 0026 D7).
    pub budget_for: Option<BudgetFor>,
    /// Resolves a route's catalog prices from the warm local cache (ADR 0027). Used
    /// for the **resume** fold (F3): the journal's usage records are priced at the route each
    /// call ran under, which the host can only resolve offline. `None` means no price is
    /// resolved, so a resumed session's tokens are restored but its fee stays hidden.
    pub price_for: Option<RoutePrice>,
    /// Branch names the `git_commit` tool refuses besides `main`/`master` (ADR 0030 R10).
    pub protected_branches: Vec<String>,
    /// Filter drivers `git add` may run (`[git] allowed_filters`, ADR 0030). `None`
    /// means the default (`["lfs"]`); an empty list allows none.
    pub allowed_filters: Option<Vec<String>>,
    /// The resolved `[shell] auto_background_secs` (ADR 0037): a **foreground**
    /// `shell` call still running after this is adopted by the job registry in place. `None`
    /// disables adoption (the config's `0`, or a host that does not set it).
    pub shell_auto_background: Option<Duration>,
    pub servers: Vec<McpServerConfig>,
    /// The role prompt's name (ADR 0036): `default`, `task` or `lead`. `None` is
    /// `default`. `--system` replaces this layer only.
    pub role: Option<String>,
    pub system_prompt: Option<String>,
    pub context_budget: Option<ContextBudget>,
    pub auto_compact: bool,
    /// The request-level settings the **compaction** call carries (ADR 0018,
    /// decision 1), already converted to the route's wire value by the host
    /// ([`crate::compaction_settings_for`]); `Null` sends none. Copied into
    /// `AgentConfig.compaction_settings` and used until the first `/model` switch.
    pub compaction_settings: serde_json::Value,
    /// Recomputes [`SessionSetup::compaction_settings`] for a switched route (ADR 0018
    /// revision 8, ADR 0026 D7). `None` means a switch cannot move the settings (they stay at
    /// the startup value).
    pub compaction_settings_for: Option<CompactionSettingsFor>,
    /// `[model] retries` (ADR 0017), the host already resolved against the default:
    /// how many times a transient model failure is retried inside one model call; `0` disables.
    pub retries: u32,
    /// The sandbox `shell` runs under; `None` for the safe tier.
    pub sandbox: Option<Sandbox>,
    /// The tier, for the startup description.
    pub tier: Preset,
    /// The permission profile (the host adds its own `allow` rules and approval policy).
    pub profile: Profile,
    /// Rebuilds the profile and sandbox for a run-time tier switch (ADR 0038 slice 2). `None`
    /// for a host that has no `/access` command (the CLI): a switch is then refused.
    pub access_builder: Option<AccessBuilder>,
    /// `true` when this session is a **task agent** (ADR 0030 O2: opened by a lead's
    /// `spawn_task`, one brief per session). `radian-host`'s `TaskRegistry` sets it; it is the
    /// flag `/mode` is refused behind (a task agent's role comes from its brief), and it is the
    /// flag that makes `git_commit` refuse `main`/`master` (ADR 0038 decision 4).
    pub task_agent: bool,
    /// The `SessionCreated.parent` a derived session records (ADR 0022's fork, ADR 0030 O2's
    /// task agent). `None` for an ordinary session. A `relation: "task"` parent names the lead
    /// and its `TaskSpawned` seq; a fork's names its parent and fork point.
    pub parent: Option<ForkParent>,
    /// When set, `git_commit` may only commit to this branch (a task agent's `radian/<id>`,
    /// ADR 0030 O3). `None` for a session with no branch restriction.
    pub allowed_branch: Option<String>,
    /// The memory root, or `None` when neither `RADIAN_HOME` nor `HOME` is set.
    pub memory_root: Option<PathBuf>,
    /// Extra skill roots from `[skills] extra_paths` (ADR 0025 slice 4), already resolved by the
    /// host (`~` and paths relative to the config file). The user-level `~/.radian/skills` is
    /// discovered first, so it wins a name conflict.
    pub skills_extra_paths: Vec<PathBuf>,
    /// `[skills] builtin` (ADR 0025): include the compiled-in skills. Default true.
    pub skills_builtin: bool,
    /// `[skills] disabled`: built-in skill names to leave out.
    pub skills_disabled: Vec<String>,
    /// ADR 0030 O11: the agent definition's built-in tool allow-list. `None` mounts every
    /// built-in the host has; `Some` mounts only the named ones ("not in the list is not
    /// mounted"). MCP tools are not in this universe — [`SessionSetup::servers`] governs them.
    pub tools_allow: Option<Vec<String>>,
    /// ADR 0030 O11: the agent definition's `skills` allow-list. `None` loads every discovered
    /// skill (today's behaviour); `Some` keeps only the named ones in the catalog, and a name
    /// discovery did not find is reported at spawn, never a failure.
    pub skills_allow: Option<Vec<String>>,
    /// The memory injection configuration (ADR 0021). The host builds it
    /// from `[memory.injection]`; `InjectionConfig::default()` is the documented D6 table.
    pub memory_injection: InjectionConfig,
    /// `true` to resume
    /// The project-trust state this session starts with (ADR 0031 T3). The host resolves it
    /// before `open_session`; `needs_ask` means an interactive host must ask before the first
    /// turn, and the session is untrusted until it answers.
    pub trust: trust::Setup,
    /// `true` to resume an existing session file, `false` to create one.
    pub resume: bool,
    /// The `SessionCreated` snapshot (a route reference: never a key or a header value).
    pub config_snapshot: serde_json::Value,
    pub turn_options: TurnOptions,
    /// A per-turn timeout (the CLI's `--turn-timeout-secs`).
    pub turn_timeout: Option<Duration>,
    /// The host's startup wording (ADR 0012 decision 4).
    pub wording: Wording,
    /// What the facade declares it can do (ADR 0023 U5). A host grants all five; a test can
    /// grant fewer, and the facade then refuses the commands it cannot serve.
    pub capabilities: Vec<SessionCapability>,
}

/// The one queue an in-process session uses (ADR 0023 U7): approvals and questions share it,
/// so the UI shows one arrival order.
pub type SessionQueue = PendingQueue<PendingRequest, PendingResponse>;

/// How a host answers approvals.
pub enum ApprovalRouting {
    /// Through the session's pending queue: the UI reads the head and answers it (the TUI).
    Queued,
    /// Directly, by this approver (the CLI's line prompt: no queue, no modal).
    Direct(Arc<dyn Approver>),
    /// A task agent (ADR 0030 slice 5): its approval asks go to the task-side pending queue, and
    /// its `ask_user` questions likewise, never to the human's queue. Only an escalation the
    /// lead performs reaches the human queue (ADR 0030 R9 / revision 6).
    Task {
        approver: Arc<dyn Approver>,
        prompter: Arc<dyn radian_coding::Prompter>,
    },
}

/// What the host injects. Everything here is host-specific; nothing is a session fact.
pub struct HostBindings {
    /// How approvals are answered (ADR 0023 U7).
    pub approvals: ApprovalRouting,
    /// A queue to share with another session (ADR 0030 O2): a task agent routes its approvals
    /// and questions to the **lead's** queue, tagged with its own session id as the origin, so
    /// the human sees one arrival order. `None` means the session creates its own queue.
    pub shared_queue: Option<Arc<SessionQueue>>,
    /// `true` to advertise `ask_user` and route questions through the queue (a host with an
    /// interactive user, ADR 0024 Q5); `false` for a host with nobody to ask.
    pub ask_user: bool,
    /// Where permission decisions are written (audit). A factory: the audit sink needs the
    /// journal, and `open_session` creates it.
    pub audit: Arc<dyn Fn(Arc<SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync>,
    /// The sink for committed `AgentEvent`s (the TUI's channel sink, the CLI's line UI).
    pub events: Arc<dyn EventSink>,
    /// The session's facts; the same handle the host's event sink writes.
    pub projection: Arc<SessionProjection>,
    /// Called for a failed turn (ADR 0017).
    pub runtime_events: Arc<dyn RuntimeEvents>,
    /// Called once when the driver task ends (the TUI sends an event, the CLI notifies).
    pub driver_stopped: Arc<dyn Fn(DriverEnd) + Send + Sync>,
    /// Runs at the start of the driver task (a TUI test hook panics here).
    pub before_driver: Option<Arc<dyn Fn() + Send + Sync>>,
    /// A memory hook's warning line (the two hosts prefix it differently).
    pub memory_warning: Arc<dyn Fn(&str) + Send + Sync>,
    /// A memory notice line for the user (ADR 0021: the project a default-scope
    /// write registered, and how to join it to a previous one). The CLI prints it to stderr
    /// and the TUI shows it as a notice.
    pub memory_notice_line: Arc<dyn Fn(&str) + Send + Sync>,
    /// Builds the host-specific parts of a **task** session (ADR 0030 slice 6). `Some` mounts
    /// the lead's task tools from the start for an interactive (non-task) session, behind the
    /// `/mode` gate (ADR 0038 decision 1); `None` mounts none. A task session never mounts them
    /// (depth limit 2), whatever its builder returns.
    pub task_builder: Option<crate::task::TaskBuilder>,
}

/// The facts the host renders its own way (the CLI's `eprintln!` lines, the TUI's notices).
pub struct SessionFacts {
    /// The `[profile]` line.
    pub profile_line: String,
    /// The `[sandbox]` line.
    pub sandbox_facts: String,
    /// The startup summary's word for the sandbox (ADR 0027): the real backend in plain
    /// words for `workspace` (`macOS sandbox` / `Linux sandbox`), `no exec` for `safe`,
    /// `no sandbox` for `full`. The TUI prints it as-is.
    pub sandbox_word: String,
    /// The `[memory]` line.
    pub memory_notice: String,
    /// Whether this process has a memory root (`/memory` is refused without one).
    pub memory_enabled: bool,
    /// How many MCP servers were configured.
    pub mcp_configured: usize,
    /// The servers/tools that actually connected, once the session is open.
    pub mcp_open: Option<(usize, usize)>,
    /// Whether `ask_user` is advertised (ADR 0024 Q5), for the host's startup line.
    pub ask_user: bool,
    /// The startup line for the loaded context files (ADR 0036 C2): `AGENTS.md (12.3 KB), …`
    /// or `none`.
    pub context_line: String,
    /// The context files that were loaded, in prompt order.
    pub context_files: Vec<crate::context::LoadedFile>,
}

impl SessionFacts {
    /// The startup line for `ask_user` (ADR 0024 Q5): a host with no interactive user must say
    /// so, so the model's missing tool is explained.
    pub fn ask_user_line(&self) -> String {
        if self.ask_user {
            "ask_user: on".to_string()
        } else {
            "ask_user: off (no interactive user)".to_string()
        }
    }
}

/// The in-process facade: it owns the inbox, the driver handle, the projection and the
/// approval routing (the injected approver), and serves the UI.
pub struct LocalSession {
    id: SessionId,
    /// The config change queue shared with the driver (`/model`, ADR 0026 D5; the in-session
    /// approval switch, D9). The driver journals a queued change before the next turn.
    config_queue: Arc<ConfigQueue>,
    /// The slot the model reads and the driver writes (the resume replay lands here, D8).
    config_slot: Arc<ConfigSlot>,
    /// The budget resolver (`B` from the warm catalog), shared with the driver (D7).
    budget_for: Option<BudgetFor>,
    workspace: PathBuf,
    journal_path: PathBuf,
    journal: Arc<SessionJournal>,
    inbox: Arc<Inbox>,
    hooks: Arc<PermissionHooks>,
    /// The sandbox the coding tools and the host's `!`/`!!` path read at each call (ADR 0038
    /// slice 2). `set_access` swaps it; a call in flight keeps the `Arc` it cloned.
    sandbox_slot: SandboxSlot,
    /// Rebuilds the profile and sandbox for a tier switch. `None` refuses `set_access`.
    access_builder: Option<AccessBuilder>,
    /// Whether this session is a task agent (ADR 0030 O2): `set_mode` refuses then.
    task_agent: bool,
    /// The lead's task registry (ADR 0030 slice 6), or `None` for a session with no lead tools.
    lead: Option<Arc<crate::task::TaskRegistry>>,
    /// Whether layer 1 is fixed for the session (`--system` or an explicit role): `set_mode`
    /// refuses then.
    role_fixed: bool,
    /// Layer 0b's hook (ADR 0038 decision 1): `set_mode` recomputes `<session>` together with
    /// the layer-1 change, through the same seam `/reload` will use (ADR 0036).
    facts_hooks: Arc<crate::prompt::SessionFactsHooks>,
    projection: Arc<SessionProjection>,
    /// The pending approvals and questions (ADR 0023 U7).
    queue: Arc<SessionQueue>,
    memory: Option<MemoryService>,
    /// The discovered skills (ADR 0025). The TUI reads this list for `/skills`, `/skill:`
    /// completion and `/skill:<name>` expansion; the prompt hook and the `skill` tool share the
    /// same `Arc`.
    skills: Arc<Vec<Skill>>,
    /// The background-job notifier (ADR 0037 slices 3-4). `None` for a host with no coding
    /// tools. The host calls [`JobNotifier::reset_wakes`] when a user message arrives.
    job_notifier: Option<Arc<crate::job_notify::JobNotifier>>,
    /// The background-job registry, for `/jobs` (ADR 0037 decision 4).
    jobs: Option<Arc<radian_coding::Jobs>>,
    /// The project-trust state (ADR 0031 T3). Interior mutability: the startup prompt resolves
    /// it after `open_session` and the startup line reads it.
    trust: Mutex<trust::Setup>,
    runtime: tokio::runtime::Handle,
    driver: DriverTask,
    capabilities: Vec<SessionCapability>,
    facts: SessionFacts,
    config_snapshot: serde_json::Value,
    /// The recovered transcript (resume-perf B): `open_session` already folded it, so the host
    /// does not re-open the journal for its initial view.
    pub history: Vec<Message>,
}

impl LocalSession {
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    /// Queues a configuration change for the **next** turn boundary (ADR 0004 path B,
    /// ADR 0026 D5). `/model` pushes a route; a route never carries a key or a header value.
    pub fn queue_config(&self, change: serde_json::Value) {
        self.config_queue.push(change);
    }

    /// The journal, for host work that reads the session file (`/export`, `/sessions`).
    pub fn journal(&self) -> &Arc<SessionJournal> {
        &self.journal
    }

    /// The background-job registry (ADR 0037). The panic-path test hook starts a job through
    /// it; `/jobs` uses the facade's `jobs()`/`kill_job()` instead.
    pub fn jobs_registry(&self) -> Option<Arc<radian_coding::Jobs>> {
        self.jobs.clone()
    }

    /// The lead's task registry (ADR 0030 slice 6), or `None` for a session with no lead tools
    /// (a task agent, or a host that supplied no task builder). The lead's tools are the model's
    /// interface to it; this is for the host (the TUI's task count, `/tasks`).
    pub fn lead_tasks(&self) -> Option<&Arc<crate::task::TaskRegistry>> {
        self.lead.as_ref()
    }

    pub fn projection(&self) -> &Arc<SessionProjection> {
        &self.projection
    }

    /// The pending approvals and questions (ADR 0023 U7): approvals and questions share it,
    /// so the UI shows one arrival order.
    pub fn queue(&self) -> &Arc<SessionQueue> {
        &self.queue
    }

    /// The turn-boundary config changes (ADR 0004 path B). A host pushes its own startup
    /// backfill here (ADR 0026 D9: an explicit `--approval` is recorded); the driver
    /// journals the change before the next turn.
    pub fn config_queue(&self) -> &Arc<ConfigQueue> {
        &self.config_queue
    }

    /// The inbox, for a host whose loop is not the TUI's: the CLI's line loop enqueues with a
    /// source and a turn-in-flight decision that the facade's commands do not carry.
    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    /// The permission hooks, for the same reason: the CLI sets the turn source per input.
    pub fn hooks(&self) -> &Arc<PermissionHooks> {
        &self.hooks
    }

    /// The sandbox mode in force right now (ADR 0038 slice 2). `None` is the `safe` tier (no
    /// shell). A host reads this after a `/access` switch; the tools read the slot themselves.
    pub fn sandbox_mode(&self) -> Option<radian_sandbox::SandboxMode> {
        self.sandbox_slot.mode()
    }

    pub fn memory(&self) -> Option<&MemoryService> {
        self.memory.as_ref()
    }

    /// The discovered skills, in discovery order (ADR 0025 S1). The UI lists these (`/skills`),
    /// completes them (`/skill:`) and expands them (`/skill:<name>`); reading one is a facade
    /// method because the host owns the `SKILL.md` read.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Expands `/skill:<name> [args]` into the user message ADR 0025 S3 puts in the inbox (the
    /// body with its frontmatter stripped, wrapped in a `<skill name=… location=…>` block, with
    /// `args` appended). `Err` is a notice the UI shows instead of sending: an unknown skill or
    /// a file that can no longer be read is not a turn failure.
    pub fn expand_skill(&self, name: &str, args: &str) -> Result<String, String> {
        let skill = self
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| format!("no skill named `{name}`: /skills lists the available ones"))?;
        skills::expand(skill, args)
    }

    /// The project-trust state this session starts with (ADR 0031 T3). A snapshot: the startup
    /// line and the prompt read it, and `record_trust` replaces it.
    pub fn trust(&self) -> trust::Setup {
        self.trust.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Applies a startup trust choice (ADR 0031 T3): the persistent choices write `trust.json`
    /// through `trust::apply` and journal one `ProjectTrustChanged` per write (non-critical);
    /// the session-only ones change nothing on disk. Returns the new state for the startup line.
    pub fn record_trust(&self, choice: trust::Choice) -> Result<trust::Setup, String> {
        let (setup, changes) = self.apply_trust(choice)?;
        // The audit journals through the session's own writer, like the permission audit
        // (best effort: the choice already happened). The TUI continues running, so the
        // spawned append completes; a host that exits right after uses
        // `record_trust_async` and awaits it.
        for change in changes {
            let event = trust_event(&change);
            let journal = Arc::clone(&self.journal);
            self.runtime.spawn(async move {
                let _ = journal.append_runtime_event(event).await;
            });
        }
        Ok(setup)
    }

    /// [`record_trust`] with the audit awaited, for a host that may exit immediately after
    /// (the line mode). Returns the new setup.
    pub async fn record_trust_async(&self, choice: trust::Choice) -> Result<trust::Setup, String> {
        let (setup, changes) = self.apply_trust(choice)?;
        for change in changes {
            let _ = self
                .journal
                .append_runtime_event(trust_event(&change))
                .await;
        }
        Ok(setup)
    }

    /// Applies the choice (writing `trust.json` and collecting the audit records), then
    /// updates the stored setup. Shared by the sync and async entry points.
    fn apply_trust(
        &self,
        choice: trust::Choice,
    ) -> Result<(trust::Setup, Vec<trust::ProjectTrustChanged>), String> {
        let workspace = self
            .trust
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .workspace
            .clone();
        let changes = std::sync::Mutex::new(Vec::new());
        let audit = |change: &trust::ProjectTrustChanged| {
            changes.lock().unwrap().push(change.clone());
        };
        let resolved = trust::apply(choice, &workspace, &audit)?;
        let setup = {
            let mut setup = self.trust.lock().unwrap_or_else(|e| e.into_inner());
            setup.resolved = resolved;
            setup.needs_ask = false;
            setup.clone()
        };
        Ok((setup, changes.into_inner().unwrap()))
    }

    pub fn capabilities(&self) -> &[SessionCapability] {
        &self.capabilities
    }

    pub fn facts(&self) -> &SessionFacts {
        &self.facts
    }

    pub fn config_snapshot(&self) -> &serde_json::Value {
        &self.config_snapshot
    }

    /// Closes the session: its pending requests are completed with their abort outcome
    /// (ADR 0023 U7 — an approval is denied fail-closed, a question gets
    /// [`radian_coding::PromptError::Gone`]), so nothing is left waiting when the session goes.
    fn close_pending(&self) {
        let aborted = self.queue.abort_session_all(self.id.as_str());
        let _ = aborted;
    }

    /// Asks the driver to stop gracefully (idempotent) and aborts this session's pending
    /// requests.
    pub fn stop(&self) {
        self.close_pending();
        if let Some(stop) = self.driver.stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
    }

    /// Aborts the driver task: the session is dropped, which kills the tool processes at once.
    /// The pending requests are aborted too, so a waiter is not left until the process exits.
    pub fn abort(&self) {
        self.close_pending();
        self.driver.abort.abort();
    }

    /// Waits for the driver task. Callable once; a second call reports `Panicked`.
    pub async fn join(&self) -> DriverEnd {
        let done = self.driver.done.lock().unwrap().take();
        match done {
            Some(done) => done
                .await
                .unwrap_or_else(|_| DriverEnd::Panicked("the driver watcher stopped".into())),
            None => DriverEnd::Panicked("the driver task was already joined".into()),
        }
    }
}

impl SessionFacade for LocalSession {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn capabilities(&self) -> &[SessionCapability] {
        &self.capabilities
    }

    fn projection(&self) -> ProjectionRead<'_> {
        self.projection.read()
    }

    fn pending(&self) -> &SessionQueue {
        &self.queue
    }

    fn queue_config(&self, change: serde_json::Value) {
        self.config_queue.push(change);
    }

    fn model_route(&self) -> Option<(String, String)> {
        radian_runtime::ModelRoute::from_value(&self.config_slot.value())
            .or_else(|| radian_runtime::ModelRoute::from_value(&self.config_snapshot))
            .map(|route| (route.provider, route.model))
    }

    fn budget_for_route(&self, provider: &str, model: &str) -> Option<u64> {
        (self.budget_for.as_ref())?(provider, model).map(|budget| budget.input_budget_tokens)
    }

    fn answer(&self, request_id: u64, response: PendingResponse) -> Result<(), CommandRefused> {
        if !self
            .capabilities
            .contains(&SessionCapability::AnswerRequest)
        {
            return Err(CommandRefused {
                command: "answer".into(),
                capability: Some(SessionCapability::AnswerRequest.as_str().to_string()),
                reason: "this session does not declare the `answer` capability".into(),
            });
        }
        match self.queue.answer(request_id, response) {
            Ok(()) | Err(Answered::Late) | Err(Answered::Unknown) => Ok(()),
        }
    }

    fn set_approval(&self, policy: ApprovalPolicy) -> Result<(), CommandRefused> {
        if !self
            .capabilities
            .contains(&SessionCapability::AnswerRequest)
        {
            return Err(CommandRefused {
                command: "set-approval".into(),
                capability: Some(SessionCapability::AnswerRequest.as_str().to_string()),
                reason: "this session does not declare the `answer` capability".into(),
            });
        }
        // The live switch first (the very next call obeys it), then the durable record: a
        // `ConfigChanged` the driver journals at the next turn boundary (ADR 0004 path B)
        // and a resume replays (ADR 0026 D9). One record per switch.
        self.hooks.set_approval(policy);
        self.config_queue
            .push(serde_json::json!({ "approval": policy.as_str() }));
        Ok(())
    }

    fn set_access(&self, tier: Preset) -> Result<(), CommandRefused> {
        if !self
            .capabilities
            .contains(&SessionCapability::AnswerRequest)
        {
            return Err(CommandRefused {
                command: "set-access".into(),
                capability: Some(SessionCapability::AnswerRequest.as_str().to_string()),
                reason: "this session does not declare the `answer` capability".into(),
            });
        }
        let Some(builder) = &self.access_builder else {
            return Err(CommandRefused {
                command: "set-access".into(),
                capability: None,
                reason: "this session cannot switch the tier at run time".into(),
            });
        };
        // The rebuild runs the sandbox probe for an enforced tier. A failure is a refusal that
        // changes nothing (ADR 0012: a failed probe never degrades).
        let parts = builder(tier).map_err(|reason| CommandRefused {
            command: "set-access".into(),
            capability: None,
            reason,
        })?;
        // The live switch first (the next tool call obeys it), then the durable record the
        // driver journals at the next turn boundary (ADR 0004 path B, ADR 0038 decision 2).
        self.hooks.set_profile(parts.profile);
        self.sandbox_slot.set(parts.sandbox);
        self.config_queue
            .push(serde_json::json!({ "access": tier.as_str() }));
        // The status bar reads the tier from the projection; keep it in step. The startup
        // source marker is a startup fact and is not re-applied here.
        self.projection.set_tier(Some(tier.as_str().to_string()));
        Ok(())
    }

    fn set_mode(&self, mode: crate::prompt::Mode) -> Result<(), CommandRefused> {
        if !self
            .capabilities
            .contains(&SessionCapability::AnswerRequest)
        {
            return Err(CommandRefused {
                command: "set-mode".into(),
                capability: Some(SessionCapability::AnswerRequest.as_str().to_string()),
                reason: "this session does not declare the `answer` capability".into(),
            });
        }
        // A task agent is told what to do by its brief; its role is the orchestrator's, not the
        // user's to switch (ADR 0030 O12, ADR 0038 decision 1).
        if self.task_agent {
            return Err(CommandRefused {
                command: "set-mode".into(),
                capability: None,
                reason: "a task agent's role comes from its brief and cannot be switched".into(),
            });
        }
        if self.role_fixed {
            return Err(CommandRefused {
                command: "set-mode".into(),
                capability: None,
                reason: "this session's role layer is fixed for the whole session".into(),
            });
        }
        // Layer 1 is not swapped here: the hook reads the mode from the config slot, and the
        // driver applies this queued change at the next turn boundary (ADR 0026 D5,
        // ADR 0038 slice 3), so the switch takes effect on the next turn's first model request
        // and never mid-turn. The durable record is that same `ConfigChanged`.
        self.config_queue
            .push(serde_json::json!({ "mode": mode.as_str() }));
        // ADR 0036: the mode switch recomputes `<session>` too. Its bytes do not
        // name the mode, so this only matters if a live read (the date, the git state) moved;
        // doing it here is what keeps the ADR's rule (0a/2/3 fixed, 0b recomputed on an
        // explicit switch) true for `/mode` as it is for `/model`.
        self.facts_hooks.refresh();
        self.projection.set_mode(Some(mode.as_str().to_string()));
        Ok(())
    }

    fn skills(&self) -> &[Skill] {
        &self.skills
    }

    fn expand_skill(&self, name: &str, args: &str) -> Result<String, String> {
        LocalSession::expand_skill(self, name, args)
    }

    fn trust(&self) -> trust::Setup {
        LocalSession::trust(self)
    }

    fn record_trust(&self, choice: trust::Choice) -> Result<trust::Setup, String> {
        LocalSession::record_trust(self, choice)
    }

    fn record_user_bash(&self, record: &UserBashRecord) {
        // Audit only (ADR 0033): append the durable `UserBash` record. A write failure is not
        // the user's problem here (the command already ran); it is dropped like the other
        // best-effort audit writes.
        let _ = self.runtime.block_on(self.journal.append_runtime_event(
            radian_session::RuntimeEvent::UserBash {
                command: record.command.clone(),
                exclude_from_context: record.exclude_from_context,
                exit_code: record.exit_code,
                cancelled: record.cancelled,
            },
        ));
    }

    fn jobs(&self) -> Vec<radian_coding::JobSummary> {
        self.jobs
            .as_ref()
            .map(|jobs| jobs.list())
            .unwrap_or_default()
    }

    fn kill_job(&self, id: &str) -> Result<(), String> {
        match &self.jobs {
            Some(jobs) => jobs.stop(id).map(|_| ()).map_err(|error| error.message),
            None => Err("this session has no background jobs".to_owned()),
        }
    }

    fn kill_all_jobs(&self) {
        if let Some(jobs) = &self.jobs {
            jobs.kill_all();
        }
    }

    /// Re-validates the capability, then runs the command. The check is here as well as in the
    /// UI: a facade whose list the UI ignored still refuses.
    fn command(&self, command: SessionCommand) -> Result<(), CommandRefused> {
        let capability = command.capability();
        if !self.capabilities.contains(&capability) {
            return Err(CommandRefused::undeclared(&command));
        }
        let name = command.name();
        match command {
            SessionCommand::SendFollowup(text) => {
                // `HookContext` has no source field, so the host declares each turn's source
                // before it starts; an unset source is treated as non-human and denied.
                self.hooks
                    .set_turn_source(Some(agent_core::InputSource::User));
                // A user message resets the job-notification wake budget (ADR 0037 decision 3).
                if let Some(notifier) = &self.job_notifier {
                    notifier.reset_wakes();
                }
                self.runtime
                    .block_on(self.inbox.followup(text, agent_core::InputSource::User))
                    .map(|_| ())
                    .map_err(|error| CommandRefused::inbox(name, &error))
            }
            SessionCommand::Steer(text) => {
                self.hooks
                    .set_turn_source(Some(agent_core::InputSource::User));
                if let Some(notifier) = &self.job_notifier {
                    notifier.reset_wakes();
                }
                self.runtime
                    .block_on(self.inbox.steer(text, agent_core::InputSource::User))
                    .map(|_| ())
                    .map_err(|error| CommandRefused::inbox(name, &error))
            }
            SessionCommand::Inject(message) => {
                // ADR 0033 + ADR 0011: `inject` is the "no wake" lane, so a `!` output never
                // starts a turn; the source is `User` (the human typed the command).
                self.hooks
                    .set_turn_source(Some(agent_core::InputSource::User));
                if let Some(notifier) = &self.job_notifier {
                    notifier.reset_wakes();
                }
                self.runtime
                    .block_on(self.inbox.inject(message, agent_core::InputSource::User))
                    .map(|_| ())
                    .map_err(|error| CommandRefused::inbox(name, &error))
            }
            SessionCommand::Cancel => {
                self.inbox.cancel(CancelCause::Requested);
                Ok(())
            }
        }
    }

    fn compact(
        &self,
        instructions: Option<String>,
        on_done: Box<dyn FnOnce(Result<agent_core::CompactionOutcome, String>) + Send>,
    ) {
        self.compact_with_budget(instructions, None, on_done)
    }

    fn compact_with_budget(
        &self,
        instructions: Option<String>,
        budget_tokens: Option<u64>,
        on_done: Box<dyn FnOnce(Result<agent_core::CompactionOutcome, String>) + Send>,
    ) {
        if !self.capabilities.contains(&SessionCapability::SendMessage) {
            on_done(Err(
                "this session does not declare the `send` capability".into()
            ));
            return;
        }
        // The driver runs the request between turns, so this must not block the UI: spawn it
        // and report the outcome through the callback.
        self.runtime.spawn({
            let inbox = Arc::clone(&self.inbox);
            async move {
                let result = inbox
                    .compact_with(
                        instructions,
                        budget_tokens.map(agent_core::ContextBudget::new),
                    )
                    .await;
                on_done(result);
            }
        });
    }
}

/// Opens a session (ADR 0023 U5). This is the one place a session is established: the journal,
/// the hooks, the combined tools, the driver, the inbox and the projection.
///
/// Async because opening the driver is (MCP servers start and the journal is recovered); a
/// synchronous host calls it with `runtime.block_on`.
pub async fn open_session(
    setup: SessionSetup,
    bindings: HostBindings,
) -> Result<LocalSession, SessionError> {
    let t0 = std::time::Instant::now();
    let HostBindings {
        approvals,
        shared_queue,
        ask_user,
        audit,
        events,
        projection,
        runtime_events,
        driver_stopped,
        before_driver,
        memory_warning,
        memory_notice_line,
        task_builder,
    } = bindings;
    let runtime = tokio::runtime::Handle::current();
    let SessionSetup {
        session_id,
        session_dir,
        workspace,
        coding_root,
        model,
        servers,
        role,
        system_prompt,
        context_budget,
        auto_compact,
        compaction_settings,
        compaction_settings_for,
        retries,
        sandbox,
        tier,
        profile,
        access_builder,
        task_agent,
        parent,
        allowed_branch,
        memory_root,
        skills_extra_paths,
        skills_builtin,
        skills_disabled,
        tools_allow,
        skills_allow,
        memory_injection,
        trust,
        resume,
        config_snapshot,
        turn_options,
        turn_timeout,
        wording,
        capabilities,
        config_slot,
        config_queue,
        budget_for,
        price_for,
        protected_branches,
        allowed_filters,
        shell_auto_background,
    } = setup;

    // The pending queue (ADR 0023 U7): approvals and questions share it. It exists before the
    // tools and the hooks, because both route through it. A task agent is handed the lead's
    // queue (ADR 0030 O2), so its approvals and questions arrive in one order with the lead's.
    let queue: Arc<SessionQueue> = shared_queue.unwrap_or_else(|| Arc::new(PendingQueue::new()));
    let origin = session_id.as_str().to_string();

    // ADR 0036 C2: read the project context files once, here. They are workspace input, so
    // they never enter the journal; only the loaded paths and sizes go into the snapshot. The
    // section is appended after the host's base prompt (layer 1) and before the skills/memory
    // hooks (layer 3).
    let context = crate::context::load(&workspace, radian_home().as_deref());
    for warning in &context.warnings {
        memory_warning(warning);
    }
    let mut config_snapshot = config_snapshot;
    if !context.is_empty()
        && let Some(object) = config_snapshot.as_object_mut()
    {
        object.insert("context".to_string(), context.snapshot());
    }

    // The startup description, before the sandbox moves into the tools. The summary word
    // comes from the real backend name, not from searching the description text (which does
    // not name the backend in the `workspace` tier).
    let (profile_line, sandbox_facts) = describe(tier.sandbox_mode(), sandbox.as_ref(), wording);
    let sandbox_word = sandbox_word(
        tier.sandbox_mode(),
        sandbox.as_ref().map(Sandbox::backend_name),
    );
    // ADR 0038 slice 2: the sandbox lives in a shared slot, so `/access` can swap it and the
    // coding tools read the new one on their next call. The host keeps this clone; the tools
    // get another.
    let sandbox_slot = SandboxSlot::from_option(sandbox);
    // ADR 0039 decision 1: the jobs directory is the session's own, e.g.
    // `<sessions_dir>/<session-id>.jobs/`. It is a sibling of the session file, so the
    // sandbox's deny-read rule on the sessions directory covers it.
    let jobs_dir = session_dir.join(format!("{}.jobs", session_id.as_str()));
    let (coding, jobs) = build_coding_tools(
        coding_root.as_deref(),
        sandbox_slot.clone(),
        &jobs_dir,
        shell_auto_background,
    )?;
    // ADR 0024 Q5: `ask_user` is mounted whenever a human is there to answer, **whether or not**
    // there are coding tools. It is its own provider, so a host without `--workspace` still
    // advertises it. `None` is what the model sees as "no such tool".
    let ask = ask_user.then(|| {
        // A task agent's questions go to the task-side pending queue (ADR 0030 slice 5), not to
        // the human's; every other session asks through its own pending queue.
        let task_prompter = match &approvals {
            ApprovalRouting::Task { prompter, .. } => Some(Arc::clone(prompter)),
            _ => None,
        };
        let prompter: Arc<dyn radian_coding::Prompter> = task_prompter
            .unwrap_or_else(|| Arc::new(QueuedPrompter::new(Arc::clone(&queue), origin.clone())));
        radian_coding::AskTools::new(prompter)
    });
    let injection_config = memory_injection;
    let memory = memory_root
        .as_ref()
        .map(|root| MemoryService::open(&workspace, root));
    let memory_tools = memory.as_ref().map(|service| {
        MemoryTools::new(service.clone()).with_notice(Arc::clone(&memory_notice_line))
    });
    let memory_notice = memory_notice(&memory, &injection_config);
    radian_session::trace::took("open/memory", t0);
    let mcp = McpToolProvider::new(servers);
    let mcp_configured = mcp.configured();

    // The session file. Every check that can fail has already run in the host, so this is the
    // first thing that creates durable state.
    let journal_path = session_dir.join(format!("{}.jsonl", session_id.as_str()));
    // resume-perf B/B2: take the single-writer lock **first**, then parse under it, and reuse
    // that one parse for the journal open, the driver's recovery and the host's initial view.
    // Parsing before the lock could miss a concurrent appender's commit and truncate it away.
    let (journal, parsed, wrote_abort) = if resume {
        if !journal_path.exists() {
            return Err(SessionError::Message(format!(
                "no session {:?} in {}",
                session_id.as_str(),
                session_dir.display()
            )));
        }
        let cannot = |error: radian_session::WriteError| {
            SessionError::Message(format!(
                "cannot resume session {:?}: {error}",
                session_id.as_str()
            ))
        };
        let t_parse = std::time::Instant::now();
        let lock =
            radian_session::SessionJournal::lock_for_resume(&journal_path).map_err(cannot)?;
        let parsed = lock.parse().map_err(cannot)?;
        radian_session::trace::took("open/parse-once", t_parse);
        let (journal, wrote_abort) = lock
            .into_journal(session_id.as_str(), &parsed)
            .map_err(cannot)?;
        (Arc::new(journal), Some(parsed), wrote_abort)
    } else {
        (
            open_journal(
                &session_id,
                &session_dir,
                &workspace,
                &config_snapshot,
                parent.as_ref(),
                &journal_path,
            )?,
            None,
            false,
        )
    };
    radian_session::trace::took("open/journal", t0);
    // ADR 0027: the journal records each call's fee, computed at the
    // price in force when the call ran, so a resume does not have to re-price history with
    // whatever the catalog says later. The writer asks this closure per usage record; it
    // resolves the route from the same slot the model reads, so the journal and the live
    // footer agree (both use `cost_micros`). No `price_for`, or a catalog miss, means no fee on
    // the record — never an invented number.
    if let Some(price_for) = price_for.as_ref() {
        let slot = Arc::clone(&config_slot);
        let initial = model.current_route();
        let price_for = Arc::clone(price_for);
        journal.set_usage_fee(Arc::new(move |usage: &ModelUsage| {
            let route = ModelRoute::from_value(&slot.value()).unwrap_or_else(|| initial.clone());
            let price = price_for(&route.provider, &route.model)?;
            crate::cost::cost_micros(&price, usage)
        }));
    }
    let inbox = Inbox::durable(64, Arc::clone(&journal));

    // The lead's task registry (ADR 0030 slice 6): built here because it needs the session's
    // journal (its `TaskSpawned`/`TaskFinished` land there) and the shared pending queue. A task
    // agent never builds one.
    let lead_mounted = !task_agent && task_builder.is_some();
    let lead_registry = if lead_mounted {
        let builder = task_builder.expect("lead_mounted implies a task builder");
        // ADR 0030 O10: the registry tells the host's live view about every `TaskSpawned` and
        // `TaskFinished` it writes, so the TUI shows a notice line and the CLI one stderr line.
        // The sink is the host's own runtime-events handler (the same one the driver calls).
        let runtime_events = Arc::clone(&runtime_events);
        let notifier: crate::task::TaskNotifier =
            Arc::new(move |event: &radian_session::RuntimeEvent| {
                runtime_events.on_runtime_event(event);
            });
        Some(Arc::new(
            crate::task::TaskRegistry::new(
                workspace.clone(),
                session_id.as_str(),
                Arc::clone(&journal),
                Arc::clone(&queue),
                builder,
            )
            .with_notifier(notifier),
        ))
    } else {
        None
    };

    // ADR 0023 U7: a UI host routes approvals through the pending queue; the CLI's line
    // prompt answers them directly. A task agent routes to its own task approver (ADR 0030
    // slice 5), which is the one case a non-human turn may reach the approver.
    let task_routing = matches!(&approvals, ApprovalRouting::Task { .. });
    // ADR 0030 slice 6: an interactive session whose host supplied a task builder mounts the
    // lead tools **from the start** (ADR 0030 item 5); a task agent never does
    // (depth limit 2). The profile allows the tool names and the mode gate refuses them
    // outside `lead`, so `/mode` never adds or removes a tool (ADR 0038 decision 1).
    let profile = if lead_mounted {
        profile.with_allowed_tools(&crate::lead_tools::LEAD_TOOL_NAMES)
    } else {
        profile
    };
    let approver: Arc<dyn Approver> = match approvals {
        ApprovalRouting::Queued => {
            Arc::new(QueuedApprover::new(Arc::clone(&queue), origin.clone()))
        }
        ApprovalRouting::Direct(approver) => approver,
        ApprovalRouting::Task { approver, .. } => approver,
    };
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(approver),
        audit(Arc::clone(&journal)),
    ));
    hooks.set_task_approver(task_routing);
    // Skills (ADR 0025 S1): discovery is best effort — a bad skill warns and is skipped. The
    // section is appended after the memory section, so the tuple below is ((perm, memory),
    // skills) and the prompt order is basic prompt, memory, skills (ADR 0025 S6).
    let skills = {
        // The user level first, then `[skills] extra_paths` (ADR 0025 slice 4): the first
        // discovery of a name wins, so `~/.radian/skills` beats an extra root.
        let discovery = skills::discover_with_extras(
            skills_dir().as_deref(),
            &skills_extra_paths,
            skills_builtin,
            &skills_disabled,
        );
        for warning in &discovery.warnings {
            memory_warning(warning);
        }
        // ADR 0030 O11: an agent definition's `skills` list is the allow-list for this session's
        // catalog. `None` is every discovered skill (the lead and an undefined task). The
        // missing-name warning is reported at spawn by the registry, not here.
        match &skills_allow {
            Some(allow) => skills::restrict_to(discovery.skills, allow),
            None => discovery.skills,
        }
    };
    let shared_skills = Arc::new(skills);
    let skills_hooks = Arc::new(SkillsHooks::new(&shared_skills));
    // ADR 0021: one injection object per session. The fixed block is computed once
    // (by whichever of the two seams runs first) and the driver's per-turn seam appends later
    // turns' new hits as one System message through the inbox's inject lane.
    let injection = MemoryInjection::new(
        memory.clone(),
        Arc::clone(&journal),
        injection_config,
        memory_warning,
    );
    let memory_hooks = Arc::new(MemoryHooks::from_shared(Arc::clone(&injection)));
    radian_session::trace::took("open/hooks+skills", t0);
    // ADR 0030 R10: pin the repository now, before the model runs. A workspace that is not a
    // git repository does not mount `git_commit` at all.
    let git_commit = crate::git_commit::RepoPin::resolve(&workspace).map(|pin| {
        crate::git_commit::GitCommitTools::new(workspace.clone(), Arc::clone(&journal), pin)
            .with_allowed_branch(allowed_branch)
            .with_refuse_main(task_agent)
            .with_protected_branches(protected_branches)
            .with_allowed_filters(
                allowed_filters.unwrap_or_else(crate::git_commit::default_allowed_filters),
            )
    });
    let mut tools =
        HostTools::new(coding, memory_tools, mcp.clone(), git_commit).with_tool_allow(tools_allow);
    let ask_registered = ask.is_some();
    if let Some(ask) = ask {
        tools = tools.with_ask(ask);
    }
    if !shared_skills.is_empty() {
        tools = tools.with_skills(SkillTools::new(Arc::clone(&shared_skills)));
    }
    // ADR 0030 slice 6: mount the lead tools from the start and gate them on the mode. The tools
    // stay in the model's list in every mode (`/mode` never adds or removes one, revision 5 item
    // 5); the gate turns a call into a "switch with /mode lead" refusal in `general`.
    if let Some(registry) = &lead_registry {
        tools = tools.with_lead(crate::lead_tools::LeadTools::new(Arc::clone(registry)));
        hooks.set_tool_gate(Some(lead_mode_gate(Arc::clone(&config_slot))));
    }
    // ADR 0036/2: the system prompt is assembled from layers. 0a <runtime> lists
    // the tools this host actually mounted and the tier/sandbox facts; 1 <role> is the role
    // prompt (or `--system`); 2 is the context files; 3 is appended by the skills/memory hooks;
    // 0b <session> is appended last by its own hook, so the static prefix stays cacheable.
    let model_name = config_snapshot
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_owned();
    let mounted = tools.builtin_tools();
    let workspace_mode = tier.sandbox_mode() == radian_sandbox::SandboxMode::WorkspaceWrite;
    let runtime_layer = crate::prompt::runtime_layer(&crate::prompt::Runtime {
        tools: &mounted,
        tier: tier.as_str(),
        sandbox: &sandbox_facts,
        network: workspace_mode,
        unix_sockets_restricted: workspace_mode,
    });
    let role_override = system_prompt.clone();
    let role_layer = match &role_override {
        Some(system) => system.clone(),
        None => crate::prompt::role_prompt(role.as_deref(), radian_home().as_deref()),
    };
    let system_prompt = Some(crate::prompt::assemble(
        &runtime_layer,
        &role_layer,
        &context.section,
    ));
    // ADR 0038 decision 1: `/mode` swaps layer 1 only, at the next turn's first model request.
    // The hook reads the mode from the same config slot `/model` writes, so the switch is
    // journaled and replayed by the driver like any other change, and it can never land
    // mid-turn. Layer 1 is fixed when `--system` (or an explicit role) set it for the session,
    // and the hook then rebuilds exactly that layer — the same bytes the assemble above made.
    let role_hooks = Arc::new(crate::prompt::RoleHooks::new(
        &runtime_layer,
        &context.section,
        role_override,
        role.clone(),
        Some(Arc::clone(&config_slot)),
    ));
    let role_fixed = role_hooks.fixed();
    let agent_config = AgentConfig {
        system_prompt,
        context_budget,
        auto_compact,
        compaction_settings,
        retries,
        ..AgentConfig::default()
    };
    let facts_hooks = Arc::new(crate::prompt::SessionFactsHooks::new(
        workspace.clone(),
        model_name,
        Some(Arc::clone(&config_slot)),
    ));
    let agent = Arc::new(
        Agent::new(model, tools)
            .with_config(agent_config)
            .with_hooks(session_hooks(
                Arc::clone(&role_hooks),
                Arc::clone(&hooks),
                Arc::clone(&memory_hooks),
                Arc::clone(&skills_hooks),
                Arc::clone(&facts_hooks),
            )),
    );

    // MCP server **processes** start here, when the tool session opens. The session file
    // already exists, so a failure names the way to continue it.
    // A written abort changes the file on disk, so the shared parse is stale: re-parse then.
    //
    // Today an abort does not change the **fold**: an uncommitted segment at end of file and an
    // aborted one both drop the segment and leave the history and the declared calls alone
    // (`agent_core::TranscriptFold`; `an_aborted_segment_and_one_at_end_of_file_fold_identically`
    // pins that). So this re-parse is a no-op for the recovered transcript and no test can tell
    // it apart from reusing the parse. It is kept deliberately: the parse is the host's view of
    // the file, and if the abort ever gains an effect on the fold, the driver must see it.
    let recovery_parse = match (&parsed, wrote_abort) {
        (Some(parsed), false) => Some(parsed),
        _ => None,
    };
    let mut driver = Driver::open_with_slot_parsed(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        events,
        turn_options,
        config_slot.clone(),
        config_queue.clone(),
        recovery_parse,
    )
    .await
    .map_err(|error| {
        SessionError::Message(format!(
            "cannot open the session: {error}\n  the session file {} exists; after fixing the \
             problem, continue it with `--resume {}`",
            journal_path.display(),
            session_id
        ))
    })?;
    radian_session::trace::took("open/driver-recover", t0);
    driver.set_turn_timeout(turn_timeout);
    // Test hook: a controllable gap between the turn's `RunFinished` and its journaled
    // `TurnFinished` (the `/fork` commit-wait test).
    driver.set_commit_delay(
        std::env::var("RADIAN_TURN_COMMIT_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_millis),
    );
    driver.set_runtime_events(runtime_events);
    // ADR 0021: the per-turn memory addition, placed in the inject lane
    // after the user's turn is claimed and before its first model call.
    driver.set_turn_additions(crate::memory::injection::addition_seam(Arc::clone(
        &injection,
    )));
    if let Some(budget_for) = budget_for.clone() {
        driver.set_budget_for(budget_for);
    }
    // ADR 0018: the same per-route resolver for the summary call's settings, so a
    // `/model` switch converts the shrunk level for the route in force.
    if let Some(settings_for) = compaction_settings_for.clone() {
        driver.set_compaction_settings_for(settings_for);
    }
    // resume-perf B: the recovered transcript goes to the host, which would otherwise open and
    // fold the same journal a second time for its initial view.
    let recovered_history = driver.take_recovered_history().unwrap_or_default();

    // ADR 0037 slices 3-4: the job notifier. The driver shares its "turn in flight" flag, so
    // the notifier can choose the `inject` lane (busy) or the waking `followup` lane (idle).
    // It writes the durable start/settle records and sends the completion System message.
    let turn_running = driver.turn_running_handle();
    let job_notifier = jobs.as_ref().map(|jobs| {
        let notifier = crate::job_notify::JobNotifier::new(
            Arc::clone(jobs),
            Arc::clone(&inbox),
            Arc::clone(&journal),
            Arc::clone(&turn_running),
        );
        notifier.spawn(&runtime);
        notifier
    });

    // ADR 0037 decision 6: a start with no settle after a crash is reported once as unknown
    // and never re-run. Injected (no wake), so the model sees it on the next turn.
    if let Some(parsed) = parsed.as_ref() {
        for (id, command) in crate::job_notify::orphan_jobs(parsed) {
            let _ = inbox
                .inject(
                    crate::job_notify::orphan_message(&id, &command),
                    agent_core::InputSource::System,
                )
                .await;
        }
    }

    // The config queue is owned by the driver; the facade only pushes to it, so clone the
    // handle before the driver moves into its task.
    let config_queue = Arc::clone(driver.config_queue());

    // The session is open, so the MCP status is known: what actually connected.
    let mcp_open = mcp.status().map(|status| (status.servers, status.tools));
    projection.set_mcp(mcp_configured, mcp_open);
    projection.set_context_window(
        context_budget
            .as_ref()
            .map(|budget| budget.input_budget_tokens),
    );
    projection.set_tier(Some(tier.as_str().to_string()));
    // ADR 0038 decision 1: the mode in force, as the slot has it after the resume replay (the
    // driver applied the journal's last `{"mode": …}` change before this line). `/mode` with no
    // argument prints it.
    projection.set_mode(Some(
        crate::prompt::Mode::of(&config_slot.value())
            .as_str()
            .to_string(),
    ));
    // F3 (ADR 0027): a resumed session folds its journal's usage records back into
    // the projection. The projection's only other writer is the live event sink, so without
    // this `--resume` showed `-` for the tokens and hid `cost` until the next response — the
    // session's earlier spending was simply gone. Both records count (a conversation request
    // and a summarizer's call, ADR 0018), and each is priced at the route that was
    // in force when it ran, not at whatever model this process starts with.
    if let Some(price_for) = price_for.as_ref()
        && let Some(parsed) = parsed.as_ref()
    {
        crate::cost::restore_usage(parsed, &projection, price_for);
    }

    // The driver runs until the host asks it to stop; dropping its `run()` future releases the
    // borrow so `shutdown()` can be called. Its end is reported by the injected callback, and
    // the result is handed back through a oneshot for the exit code.
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        if let Some(before) = before_driver {
            before();
        }
        tokio::select! {
            result = driver.run() => result,
            _ = &mut stop_rx => {
                driver.shutdown();
                driver.run().await
            }
        }
    });
    let abort = task.abort_handle();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let end = match task.await {
            Ok(Ok(())) => DriverEnd::Ok,
            Ok(Err(error)) => DriverEnd::Failed(error.to_string()),
            Err(join) => DriverEnd::Panicked(join.to_string()),
        };
        driver_stopped(end.clone());
        let _ = done_tx.send(end);
    });

    radian_session::trace::took("open/driver-task", t0);
    let facts = SessionFacts {
        profile_line,
        sandbox_facts,
        sandbox_word,
        memory_notice,
        memory_enabled: memory.is_some(),
        mcp_configured,
        mcp_open,
        // The **actual** registration, not the host's request: the startup line must not claim
        // a tool the model cannot see.
        ask_user: ask_registered,
        context_line: context.line(&workspace, user_home().as_deref()),
        context_files: context.files.clone(),
    };
    Ok(LocalSession {
        id: session_id,
        config_queue,
        config_slot,
        budget_for,
        workspace,
        journal_path,
        journal,
        inbox,
        hooks,
        sandbox_slot,
        access_builder,
        task_agent,
        lead: lead_registry,
        role_fixed,
        facts_hooks,
        projection,
        queue,
        memory,
        skills: shared_skills,
        job_notifier,
        jobs,
        trust: Mutex::new(trust),
        runtime,
        driver: DriverTask {
            stop: Mutex::new(Some(stop_tx)),
            abort,
            done: Mutex::new(Some(done_rx)),
        },
        capabilities,
        facts,
        config_snapshot,
        history: recovered_history,
    })
}

/// The gate that keeps the lead tools behind `lead` mode (ADR 0030 slice 6, ADR 0038 decision
/// 1): a call to one of them is denied unless the session's mode is `lead`. The mode comes from
/// the driver's config slot, which `/mode` only moves at a **turn boundary**, so the switch
/// takes effect on the next turn and never mid-turn. Deny-only: no rule, grant or approver can
/// widen it.
fn lead_mode_gate(slot: Arc<radian_runtime::ConfigSlot>) -> radian_permissions::ToolGate {
    Arc::new(move |tool: &str| {
        if crate::lead_tools::LEAD_TOOL_NAMES.contains(&tool)
            && crate::prompt::Mode::of(&slot.value()) != crate::prompt::Mode::Lead
        {
            Some("the lead tools are off outside lead mode: switch with /mode lead".to_owned())
        } else {
            None
        }
    })
}

/// The agent's hook chain (ADR 0025 S6, ADR 0038 decision 1): the role hook writes layers 0a, 1
/// and 2 first, then the permission rules run, then the memory section is appended, then the
/// skills section. The order is a promise — the role layer comes first, and the sections must be
/// deterministic with memory before skills — so it lives in this one function, which
/// `open_session` calls; `tests/skills_order.rs` builds the real hooks and asserts the order on
/// the composed chain. Swapping `memory` and `skills` here is the mutation that test catches.
pub fn session_hooks(
    role: Arc<crate::prompt::RoleHooks>,
    permissions: Arc<PermissionHooks>,
    memory: Arc<MemoryHooks>,
    skills: Arc<SkillsHooks>,
    facts: Arc<crate::prompt::SessionFactsHooks>,
) -> impl Hooks {
    // The order is the promise: the role layers, permissions, memory, skills, then the volatile
    // <session> facts (ADR 0025 S6, ADR 0036 D1).
    ((((role, permissions), memory), skills), facts)
}

/// The durable audit event for one trust write (ADR 0031 T2).
fn trust_event(change: &trust::ProjectTrustChanged) -> radian_session::RuntimeEvent {
    radian_session::RuntimeEvent::trust_changed(
        change.path.to_string_lossy().into_owned(),
        change.decision.as_bool(),
    )
}

/// Creates or resumes the session file. The caller has already validated everything that can
/// fail, so this only reports the file's own state. `parent` is the derived-session lineage
/// (ADR 0022's fork or ADR 0030 O2's task agent): `Some` writes `SessionCreated.parent` with
/// its `relation`, `None` writes an ordinary session.
fn open_journal(
    session_id: &SessionId,
    dir: &Path,
    workspace: &Path,
    config_snapshot: &serde_json::Value,
    parent: Option<&ForkParent>,
    path: &Path,
) -> Result<Arc<SessionJournal>, SessionError> {
    crate::ensure_private_dir(dir).map_err(|error| {
        SessionError::Message(format!("cannot create {}: {error}", dir.display()))
    })?;
    // The snapshot is a model route reference only: never the key, a header value or a base
    // URL that could carry credentials (ADR 0008 §4, ADR 0013).
    let created = match parent {
        Some(parent) => SessionCreatedData {
            workspace: workspace.to_string_lossy().into_owned(),
            config_snapshot: config_snapshot.clone(),
            parent: Some(parent.clone()),
        },
        None => SessionCreatedData::new(workspace.to_string_lossy(), config_snapshot.clone()),
    };
    match SessionJournal::create(path, session_id.as_str(), created) {
        Ok(journal) => Ok(Arc::new(journal)),
        Err(radian_session::WriteError::NotEmpty { .. }) => Err(SessionError::Message(format!(
            "session {:?} already exists in {}; continue it with `--resume {}`",
            session_id.as_str(),
            dir.display(),
            session_id
        ))),
        Err(error) => Err(SessionError::Message(format!(
            "cannot create session {:?}: {error}",
            session_id.as_str()
        ))),
    }
}

fn build_coding_tools(
    workspace: Option<&Path>,
    sandbox: SandboxSlot,
    jobs_dir: &Path,
    shell_auto_background: Option<Duration>,
) -> Result<(Option<CodingTools>, Option<Arc<radian_coding::Jobs>>), SessionError> {
    match workspace {
        Some(dir) => {
            // The slot decides whether `shell` exists: an empty slot is the `safe` tier (no
            // shell advertised), a non-empty one is a sandbox the shell can run under. The
            // tool set is fixed here at `open_session`; a later switch to a *narrower* tier
            // still denies `shell` through the permission rules (ADR 0038 slice 2).
            let tools = if sandbox.is_some() {
                CodingTools::with_sandbox_slot(dir, sandbox).map_err(|error| {
                    SessionError::Message(format!("--workspace {}: {error}", dir.display()))
                })?
            } else {
                CodingTools::new(dir).map_err(|error| {
                    SessionError::Message(format!("--workspace {}: {error}", dir.display()))
                })?
            };
            // ADR 0037: the adoption threshold comes from `[shell]`; the host
            // resolved the default already, so `None` here means off.
            let tools = tools.with_limits(radian_coding::Limits {
                auto_background_after: shell_auto_background,
                ..radian_coding::Limits::default()
            });
            // ADR 0039 decision 1: the jobs directory is the **session's own** directory, in
            // the sessions dir the sandbox already denies reading. It is created 0700 with the
            // same helper the sessions dir uses, before the registry can write anything, and
            // passed in — the registry never derives a path from the workspace.
            crate::ensure_private_dir(jobs_dir).map_err(|error| {
                SessionError::Message(format!(
                    "cannot create the jobs directory {}: {error}",
                    jobs_dir.display()
                ))
            })?;
            // The host creates the registry so it can subscribe to job lifecycle notices and
            // write the durable start/settle records (ADR 0037).
            let jobs = Arc::new(radian_coding::Jobs::new(jobs_dir));
            Ok((Some(tools.with_jobs(Arc::clone(&jobs))), Some(jobs)))
        }
        None => Ok((None, None)),
    }
}

fn memory_notice(memory: &Option<MemoryService>, config: &InjectionConfig) -> String {
    match memory {
        Some(service) => match service.state() {
            Ok(ProjectState::Known(project)) => format!(
                "[memory] project {project}; injection on, up to {} tokens",
                config.max_tokens
            ),
            Ok(ProjectState::Unlinked { .. }) => format!(
                "[memory] no project linked; global memory only (run `radian memory project new` \
                 or `radian memory relink <ProjectId>`); injection on, up to {} tokens",
                config.max_tokens
            ),
            Err(error) => format!("[memory] unavailable: {error}"),
        },
        None => "[memory] disabled (no RADIAN_HOME or HOME)".to_owned(),
    }
}

/// The model route a **resume** would restore (ADR 0026 D8): the journal's last model
/// `ConfigChanged`. `None` when the file has none — the caller then falls back to the config
/// and, failing that, requires `--model`. Reads the session file line by line; it is the one
/// source of truth, and this runs before anything durable is created.
pub fn resumed_model_route(
    session_dir: &Path,
    session_id: &str,
) -> Option<radian_runtime::ModelRoute> {
    let text = std::fs::read_to_string(session_dir.join(format!("{session_id}.jsonl"))).ok()?;
    let mut route = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["event"] == "ConfigChanged"
            && let Some(found) = radian_runtime::ModelRoute::from_value(&value["data"]["change"])
        {
            route = Some(found);
        }
    }
    route
}

/// The permission tier a **resume** would restore (ADR 0038 decision 2): the journal's last
/// `ConfigChanged` whose change carries an `access` key. `None` when the file has none — the
/// caller then falls back to the remembered per-project tier, the config and the default.
/// Reads the session file line by line; this runs before anything durable is created.
pub fn resumed_access(session_dir: &Path, session_id: &str) -> Option<Preset> {
    let text = std::fs::read_to_string(session_dir.join(format!("{session_id}.jsonl"))).ok()?;
    let mut tier = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["event"] == "ConfigChanged"
            && let Some(name) = value["data"]["change"]["access"].as_str()
            && let Some(found) = crate::access::parse_tier(name)
        {
            tier = Some(found);
        }
    }
    tier
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_sandbox::{SandboxMode, SandboxPolicy};

    fn full_access(ws: &Path) -> Sandbox {
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
        Sandbox::from_policy(policy).expect("full-access needs no backend")
    }

    /// ADR 0039 decision 1: the host builds the registry over the **session's own** jobs
    /// directory (0700), never over `<workspace>/.radian/jobs`, and the output file lands
    /// there at 0600.
    #[cfg(unix)]
    #[test]
    fn the_jobs_directory_is_the_sessions_own_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("radian-host-jobs-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let jobs_dir = root.join("sessions/sid.jobs");

        let slot = SandboxSlot::from_option(Some(full_access(&ws)));
        let (_coding, jobs) = build_coding_tools(Some(&ws), slot, &jobs_dir, None).expect("tools");
        let jobs = jobs.expect("a registry");

        assert_eq!(
            jobs.dir(),
            jobs_dir,
            "the registry uses the session jobs directory"
        );
        assert_eq!(
            std::fs::metadata(&jobs_dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "the jobs directory is 0700"
        );

        let info = jobs
            .start(&full_access(&ws), &ws, "echo hi", None)
            .expect("start the job");
        let output = jobs.output_path(&info.id);
        assert!(output.starts_with(&jobs_dir), "{}", output.display());
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600,
            "the output file is 0600"
        );
        assert!(
            !ws.join(".radian").join("jobs").exists(),
            "the workspace must not grow a jobs directory"
        );

        jobs.kill_all();
        let _ = std::fs::remove_dir_all(&root);
    }
}

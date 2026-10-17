//! What the host resolves and injects (ADR 0023 U5/U7, ADR 0038 slice 2): the
//! pre-session values ([`SessionSetup`]), the shared queue ([`SessionQueue`]), the
//! host's bindings ([`HostBindings`]), and the facts the host renders ([`SessionFacts`]).
//!
//! Split from `session/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{ContextBudget, EventSink, TurnOptions};
use agent_extensions::McpServerConfig;
use radian_permissions::{Approver, AuditSink, Preset, Profile};
use radian_runtime::{BudgetFor, CompactionSettingsFor, ConfigQueue, ConfigSlot, RuntimeEvents};
use radian_sandbox::Sandbox;
use radian_session::{ForkParent, SessionJournal};

use super::facade::DriverEnd;
use super::ids::{SessionCapability, SessionId};
use crate::cost::RoutePrice;
use crate::describe::Wording;
use crate::memory::InjectionConfig;
use crate::model::SwitchableModel;
use crate::pending::{PendingQueue, PendingRequest, PendingResponse};
use crate::projection::SessionProjection;
use crate::subagent::SubagentParts;
use crate::trust;

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
    /// The resolved `[shell] auto_background_secs` (ADR 0037): a **foreground**
    /// `shell` call still running after this is adopted by the job registry in place. `None`
    /// disables adoption (the config's `0`, or a host that does not set it).
    pub shell_auto_background: Option<Duration>,
    pub servers: Vec<McpServerConfig>,
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
    /// The `SessionCreated.parent` a derived session records (ADR 0022's fork, ADR 0043
    /// decision 1a's subagent child). `None` for an ordinary session. A `relation: "subagent"`
    /// parent is also the depth-1 marker: `open_session` never mounts `subagent` in that
    /// session.
    pub parent: Option<ForkParent>,
    /// The `subagent` tool's launch parameters (ADR 0043 decision 1a). `None` for a host that
    /// cannot start a child. Ignored, so the tool is not mounted, when [`SessionSetup::parent`]
    /// says this session is itself a subagent child.
    pub subagent: Option<SubagentParts>,
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

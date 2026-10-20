//! The in-process facade ([`LocalSession`], ADR 0023 U5): it owns the inbox, the
//! driver handle, the projection and the approval routing, and serves the UI.
//!
//! Split from `session/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.
//! [`DriverTask`] moved here from `facade.rs` with it: only this implementation uses it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::Message;
use radian_permissions::{ApprovalPolicy, PermissionHooks, Preset};
use radian_runtime::{BudgetFor, CancelCause, ConfigQueue, ConfigSlot};
use radian_sandbox::SandboxSlot;
use radian_session::SessionJournal;

use super::facade::{DriverEnd, SessionFacade};
use super::ids::{CommandRefused, SessionCapability, SessionCommand, SessionId, UserBashRecord};
use super::setup_types::{AccessBuilder, SessionFacts, SessionQueue};
use super::support::trust_event;
use crate::memory::MemoryService;
use crate::pending::{Answered, PendingResponse};
use crate::projection::{ProjectionRead, SessionProjection};
use crate::skills::{self, Skill};
use crate::trust;

/// The driver's task handle: the host asks it to stop or aborts it, and waits once.
pub(super) struct DriverTask {
    pub(super) stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(super) abort: tokio::task::AbortHandle,
    pub(super) done: Mutex<Option<tokio::sync::oneshot::Receiver<DriverEnd>>>,
}

/// The in-process facade: it owns the inbox, the driver handle, the projection and the
/// approval routing (the injected approver), and serves the UI.
pub struct LocalSession {
    pub(super) id: SessionId,
    /// The config change queue shared with the driver (`/model`, ADR 0026 D5; the in-session
    /// approval switch, D9). The driver journals a queued change before the next turn.
    pub(super) config_queue: Arc<ConfigQueue>,
    /// The slot the model reads and the driver writes (the resume replay lands here, D8).
    pub(super) config_slot: Arc<ConfigSlot>,
    /// The budget resolver (`B` from the warm catalog), shared with the driver (D7).
    pub(super) budget_for: Option<BudgetFor>,
    pub(super) workspace: PathBuf,
    pub(super) journal_path: PathBuf,
    pub(super) journal: Arc<SessionJournal>,
    pub(super) inbox: Arc<radian_runtime::Inbox>,
    pub(super) hooks: Arc<PermissionHooks>,
    /// The sandbox the coding tools and the host's `!`/`!!` path read at each call (ADR 0038
    /// slice 2). `set_access` swaps it; a call in flight keeps the `Arc` it cloned.
    pub(super) sandbox_slot: SandboxSlot,
    /// Rebuilds the profile and sandbox for a tier switch. `None` refuses `set_access`.
    pub(super) access_builder: Option<AccessBuilder>,
    /// Whether a turn is running right now (the driver's own flag, `false` before the first
    /// turn).
    pub(super) turn_running: Arc<std::sync::atomic::AtomicBool>,
    /// Fired whenever the driver stops running a turn or control request. Hosts that
    /// decide on `turn_running` wait on this instead of polling.
    pub(super) idle: Arc<tokio::sync::Notify>,
    pub(super) projection: Arc<SessionProjection>,
    /// The pending approvals and questions (ADR 0023 U7).
    pub(super) queue: Arc<SessionQueue>,
    pub(super) memory: Option<MemoryService>,
    /// The discovered skills (ADR 0025). The TUI reads this list for `/skills`, `/skill:`
    /// completion and `/skill:<name>` expansion; the prompt hook and the `skill` tool share the
    /// same `Arc`.
    pub(super) skills: Arc<Vec<Skill>>,
    /// The background-job notifier (ADR 0037 slices 3-4). `None` for a host with no coding
    /// tools. The host calls [`JobNotifier::reset_wakes`] when a user message arrives.
    pub(super) job_notifier: Option<Arc<crate::job_notify::JobNotifier>>,
    /// The user-level command hooks (ADR 0045 slice A). The host calls
    /// [`crate::CommandHooks::reset_wakes`] when a user message arrives, restarting the
    /// `Stop` continuation budget.
    pub(super) command_hooks: Arc<crate::CommandHooks>,
    /// The background-job registry, for `/jobs` (ADR 0037 decision 4).
    pub(super) jobs: Option<Arc<radian_coding::Jobs>>,
    /// The model-visible tool specs at open time (the inspector's tools page). Published by
    /// `HostTools::open_session` through the spec sink, so the snapshot is the provider's
    /// own list, not a reconstruction. Old alias names are still registered here; the
    /// model request strips them (ADR 0043 decision 3), and the inspector applies the same
    /// strip so its list matches the request's set exactly.
    pub(super) tool_specs: Vec<agent_core::ToolSpec>,
    /// The MCP server configs this session was opened with (the inspector's mcp page).
    pub(super) mcp_servers: Vec<agent_extensions::McpServerConfig>,
    /// Where the MCP servers came from (`--mcp-config <FILE>`). `None` when no file was
    /// given.
    pub(super) mcp_config_path: Option<String>,
    /// The resolved `[hooks]` section (the inspector's hooks page, ADR 0045 slice A).
    /// User level only today; project hooks are not loaded.
    pub(super) hooks_config: crate::config::HooksConfig,
    /// The project-trust state (ADR 0031 T3). Interior mutability: the startup prompt resolves
    /// it after `open_session` and the startup line reads it.
    pub(super) trust: Mutex<trust::Setup>,
    pub(super) runtime: tokio::runtime::Handle,
    pub(super) driver: DriverTask,
    pub(super) capabilities: Vec<SessionCapability>,
    pub(super) facts: SessionFacts,
    pub(super) config_snapshot: serde_json::Value,
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

    /// The background-job notifier (ADR 0037 slices 3-4), for hosts that wait on job
    /// completion (`radian exec -p` watches its handled/degraded counts). `None` for a
    /// host with no coding tools.
    pub fn job_notifier(&self) -> Option<Arc<crate::job_notify::JobNotifier>> {
        self.job_notifier.clone()
    }

    /// The user-level command hooks (ADR 0045), for hosts that wait on degraded Stop
    /// verdicts (`radian exec -p` ends the session on these with exit 3).
    pub fn command_hooks(&self) -> Arc<crate::CommandHooks> {
        Arc::clone(&self.command_hooks)
    }

    /// Whether a turn is running right now (the driver's flag). `false` before the first turn and
    /// between turns.
    pub fn turn_running(&self) -> bool {
        self.turn_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves whenever the driver stops running a turn or control request. Pair with
    /// [`LocalSession::turn_running`]: subscribe first, then check, so a clear between
    /// the check and the wait still resolves the wait.
    pub fn idle_signal(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.idle)
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
    pub fn inbox(&self) -> &Arc<radian_runtime::Inbox> {
        &self.inbox
    }

    /// The permission hooks, for the same reason: the CLI sets the turn source per input.
    pub fn hooks(&self) -> &Arc<PermissionHooks> {
        &self.hooks
    }

    /// The sandbox mode in force right now (ADR 0038 slice 2). `None` means no shell.
    /// A host reads this after a `/access` switch; the tools read the slot themselves.
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

    fn tool_specs(&self) -> Vec<agent_core::ToolSpec> {
        self.tool_specs.clone()
    }

    fn mcp_servers(&self) -> Vec<agent_extensions::McpServerConfig> {
        self.mcp_servers.clone()
    }

    fn mcp_config_path(&self) -> Option<String> {
        self.mcp_config_path.clone()
    }

    fn hooks_config(&self) -> crate::config::HooksConfig {
        self.hooks_config.clone()
    }

    fn job_tail(&self, id: &str, max: usize) -> Option<(radian_coding::JobState, String)> {
        let jobs = self.jobs.as_ref()?;
        jobs.tail(id, max).map(|(state, _, tail)| (state, tail))
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
                // A user message resets the notification wake budgets (ADR 0037 decision 3,
                // ADR 0045).
                if let Some(notifier) = &self.job_notifier {
                    notifier.reset_wakes();
                }
                self.command_hooks.reset_wakes();
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
                self.command_hooks.reset_wakes();
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
                self.command_hooks.reset_wakes();
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

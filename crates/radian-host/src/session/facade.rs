//! The UI's one handle on a session (ADR 0023 U5): the [`SessionFacade`] trait, how
//! the driver task ended ([`DriverEnd`]), and why a session could not be opened
//! ([`SessionError`]).
//!
//! Split from `session/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use radian_permissions::{ApprovalPolicy, Preset};

use super::ids::{CommandRefused, SessionCapability, SessionCommand, SessionId, UserBashRecord};
use super::setup_types::SessionQueue;
use crate::pending::PendingResponse;
use crate::projection::ProjectionRead;
use crate::skills::Skill;
use crate::trust;

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

/// Why a session could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{0}")]
    Message(String),
}

//! The v1 events whose producer is the **runtime** (ADR 0006 §5.5–§5.7).
//!
//! Transcript events are deliberately absent: `MessageAppended`, `TranscriptReplaced` and
//! the tool/usage records go through [`agent_core::TranscriptJournal::append`], which is the
//! only path that keeps `MessageAppended.index` contiguous with what `replay` expects.
//! Writing them from here would decouple `index` from the transcript.
//!
//! `critical` is a property of the event type (ADR 0006 §4), never a caller argument: the
//! same event must not be writable with two different skip policies.

use agent_core::{InputSource, Message, MessageId, UsageSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which inbox lane an input waits in (ADR 0006 §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxLane {
    NextTurn,
    NextStep,
    /// A value this version does not know (ADR 0006 §7(d)).
    #[serde(other)]
    Unknown,
}

/// How a turn ended (ADR 0006 §5.6). This is the Core `RunStatus` set, snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnOutcome {
    Completed,
    Refused,
    BudgetExceeded,
    /// Typed compatibility value for an old context-budget stop (ADR 0015/0018): a caller
    /// can serialize it, but the current driver/host does not emit it.
    ContextBudget,
    Failed,
    Poisoned,
    Incomplete,
    Cancelled,
    /// An outcome value this reader does not know (a future writer's value). A v1 reader
    /// maps it here instead of refusing the file (ADR 0006 §7(d)).
    #[serde(other)]
    Unknown,
}

/// The `reason` sub-field of `TurnFinished` (ADR 0006 §5.6). Required when `outcome` is
/// `incomplete` or `cancelled`; `unknown_stop_reason` is accompanied by `stop_reason`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnReason {
    MaxTokens,
    ContentFiltered,
    ToolUseWithoutCalls,
    UnknownStopReason,
    Requested,
    DeadlineExceeded,
    /// A value this version does not know (ADR 0006 §7(d)).
    #[serde(other)]
    Unknown,
}

/// How a permission decision came out (ADR 0006 §5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionDecision {
    Allow,
    Deny,
    Rewrite,
    /// A value this version does not know (ADR 0006 §7(d)).
    #[serde(other)]
    Unknown,
}

/// Who made a permission decision (ADR 0006 §5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionApprover {
    User,
    Policy,
    /// A value this version does not know (ADR 0006 §7(d)).
    #[serde(other)]
    Unknown,
}

/// An approval policy value, named as `--approval` and `[permissions] approval` name them
/// (ADR 0026 D9). Only the values the in-session switch can set appear in events today,
/// but the enum is the full name set so a future producer never needs a second type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApprovalPolicyValue {
    OnAsk,
    Never,
    Always,
    Auto,
    /// A value this version does not know (ADR 0006 §7(d)).
    #[serde(other)]
    Unknown,
}

/// One record that was injected before a model request (ADR 0021 D6). It carries **only** the
/// id, revision and scope — never the content (D6: the event cannot rebuild an old request).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryInjectedRecord {
    pub id: String,
    pub revision: u64,
    /// The scope as the tagged value of ADR 0021 D1 (`"global"` or `{"project": "<id>"}`).
    /// `radian-session` does not depend on the memory crate, so it stays a [`Value`] here;
    /// a future scope shape round-trips without a new event version.
    pub scope: Value,
}

/// One durable fact the runtime owns. `#[non_exhaustive]`; the variants below are the
/// current set. `ContextBudgetReached` is a typed compatibility record with no current
/// driver/host producer, and `Rewound` is reader-only (no variant here).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RuntimeEvent {
    /// An input entered a lane (ADR 0006 §5.5). `source` is always explicit at the runtime
    /// boundary; `message.meta.id` must equal `message_id`. `wake` records whether the input
    /// may open a turn by itself when the driver is idle (ADR 0011): a `followup`
    /// and a `steer` do, an `inject` does not.
    InboxEnqueued {
        message: Message,
        message_id: MessageId,
        lane: InboxLane,
        source: InputSource,
        wake: bool,
        /// Whether this input is a wake-gate degrade (the wake budget was used up, so it
        /// was injected only and never wakes an idle driver). Written only when `true`, so
        /// an ordinary line is byte-identical to before; an old journal has no such field
        /// and reads as `false` (ADR 0006 §5.5).
        degraded: bool,
    },
    /// The driver claimed these inputs for a turn (ADR 0006 §5.5).
    InputClaimed {
        message_ids: Vec<MessageId>,
        turn_id: String,
    },
    /// The message reached the transcript; `transcript_index` is its `MessageAppended.index`
    /// (ADR 0006 §5.5).
    InputAdmitted {
        message_id: MessageId,
        transcript_index: usize,
    },
    /// A turn began (ADR 0006 §5.6).
    TurnStarted { turn_id: String },
    /// A turn ended (ADR 0006 §5.6).
    TurnFinished {
        turn_id: String,
        outcome: TurnOutcome,
        reason: Option<TurnReason>,
        /// The provider's raw stop reason, only meaningful with
        /// [`TurnReason::UnknownStopReason`].
        stop_reason: Option<String>,
    },
    /// A turn ended because a model or hook error failed it (ADR 0017). Audit only:
    /// `critical = false`. `message` is the adapter's error text, already masked; it is the
    /// only place the host learns *why* a `TurnFinished { outcome: failed }` happened without
    /// reading the live event stream. A reader that skips it still sees the terminal record.
    TurnFailed { turn_id: String, message: String },
    /// A configuration change was applied (ADR 0006 §5.7). `change` stays opaque here; the
    /// registry and the slots own its meaning. The optional `profile_ref` sibling of §5.7 is
    /// not exposed yet (no producer in this phase).
    ConfigChanged { revision: u64, change: Value },
    /// Typed compatibility record for a context-budget stop (ADR 0015 C6). Audit only:
    /// `critical = false`. `append_runtime_event` can serialize a caller-supplied record; the
    /// current driver/host has no production path. `source` says whether the projection is a
    /// fact, a fact plus an estimate, or purely local; `baseline_input_tokens` and
    /// `estimated_growth_tokens` split the projection so a reader is never shown a mixed
    /// number as a precise report.
    ContextBudgetReached {
        turn_id: String,
        source: UsageSource,
        projected_tokens: u64,
        window_tokens: u64,
        threshold_percent: u8,
        baseline_input_tokens: Option<u64>,
        estimated_growth_tokens: Option<u64>,
    },
    /// One permission decision (ADR 0006 §5.9). Audit only: `critical = false`, it does not
    /// take part in state reconstruction and is **not** replayed on resume.
    PermissionResolved {
        request_id: String,
        call_id: String,
        decision: PermissionDecision,
        approver: PermissionApprover,
        reason: Option<String>,
        /// The named policy command that decided, when `approver = "policy"` (ADR 0006 §5.9).
        /// The field stays for old journals; **no producer since ADR 0043 decision 4** (the
        /// edge-policy layer went with the orchestration), so a new record leaves it `None`.
        policy: Option<String>,
    },
    /// The session's approval policy was switched in place (ADR 0026 D9). Audit only:
    /// `critical = false`. The durable state a resume restores is the `ConfigChanged`
    /// record the host writes alongside (`{"approval": …}`); this event is the permission
    /// audit of the switch itself.
    ApprovalPolicyChanged { policy: ApprovalPolicyValue },
    /// The memory set injected before a model request (ADR 0021 D6). Audit only:
    /// `critical = false`. The records are an ordered list of `{ id, revision, scope }` and
    /// carry no content; a reader uses them to see *which* records were injected, not to
    /// rebuild the request.
    MemoryInjected { records: Vec<MemoryInjectedRecord> },
    /// A child session was forked from this one (ADR 0022 §3). Audit only: `critical = false`.
    /// The child's own `SessionCreated.parent` is the authority on the lineage; this record is
    /// the parent's index of it. `parent_seq` names the fork point and matches the child's
    /// `parent.seq`.
    SessionForked {
        child_session_id: String,
        parent_seq: u64,
    },
    /// A project-trust decision was written to `trust.json` (ADR 0031 T2/T6). Audit only:
    /// `critical = false`, never replayed — the store is the state, this is the record that a
    /// change happened. `path` is the canonical directory the decision is about.
    ProjectTrustChanged { path: String, decision: bool },
    /// A user's `!`/`!!` command ran (ADR 0033). Audit only: `critical = false`. The command,
    /// whether it entered the model's context, the exit status and whether it was cancelled —
    /// who ran what and how it ended. `!!` is recorded too (staying out of the context is not
    /// staying out of the audit). The `Ran \`cmd\`` user message, when one is injected, is a
    /// separate `InboxEnqueued`/`MessageAppended` pair.
    UserBash {
        command: String,
        exclude_from_context: bool,
        exit_code: Option<i32>,
        cancelled: bool,
    },
    /// A background `shell` job started (ADR 0037 decision 5). Audit only: `critical =
    /// false`. The start/settle pair is how a resume learns a job existed; a start with no
    /// settle after a crash is reported once as unknown and never re-run.
    BackgroundJobStarted {
        job_id: String,
        /// The command's first line (the summary `job_list` shows).
        command: String,
        /// The `job_output` hint, not a path (ADR 0039 decision 1): the output file lives in
        /// the session directory and is unreadable by the model, so this field names the tool
        /// that reads it. The shape is kept from ADR 0037 decision 5.
        output: String,
    },
    /// A background `shell` job settled (ADR 0037 decision 5). Audit only: `critical =
    /// false`. `state` is the `[status: ...]` spelling (`exited 0`, `signal 9`, `killed`,
    /// `timed out`); `duration_ms` is how long it ran.
    BackgroundJobSettled {
        job_id: String,
        state: String,
        duration_ms: u64,
    },
}

impl RuntimeEvent {
    /// The v1 disk event name (ADR 0006 §4).
    pub fn name(&self) -> &'static str {
        match self {
            RuntimeEvent::InboxEnqueued { .. } => "InboxEnqueued",
            RuntimeEvent::InputClaimed { .. } => "InputClaimed",
            RuntimeEvent::InputAdmitted { .. } => "InputAdmitted",
            RuntimeEvent::TurnStarted { .. } => "TurnStarted",
            RuntimeEvent::TurnFinished { .. } => "TurnFinished",
            RuntimeEvent::TurnFailed { .. } => "TurnFailed",
            RuntimeEvent::ConfigChanged { .. } => "ConfigChanged",
            RuntimeEvent::ContextBudgetReached { .. } => "ContextBudgetReached",
            RuntimeEvent::PermissionResolved { .. } => "PermissionResolved",
            RuntimeEvent::ApprovalPolicyChanged { .. } => "ApprovalPolicyChanged",
            RuntimeEvent::MemoryInjected { .. } => "MemoryInjected",
            RuntimeEvent::SessionForked { .. } => "SessionForked",
            RuntimeEvent::ProjectTrustChanged { .. } => "ProjectTrustChanged",
            RuntimeEvent::UserBash { .. } => "UserBash",
            RuntimeEvent::BackgroundJobStarted { .. } => "BackgroundJobStarted",
            RuntimeEvent::BackgroundJobSettled { .. } => "BackgroundJobSettled",
        }
    }

    /// The `critical` flag (ADR 0006 §4). State/rebuild events are `true`; audit events are
    /// `false` (`TurnFailed`, `ContextBudgetReached`, `PermissionResolved`,
    /// `ApprovalPolicyChanged`, `MemoryInjected`, `SessionForked`, `ProjectTrustChanged`,
    /// `UserBash`, `BackgroundJobStarted`, `BackgroundJobSettled`). A reader may skip a
    /// `false` event without breaking reconstruction.
    pub fn critical(&self) -> bool {
        match self {
            RuntimeEvent::InboxEnqueued { .. }
            | RuntimeEvent::InputClaimed { .. }
            | RuntimeEvent::InputAdmitted { .. }
            | RuntimeEvent::TurnStarted { .. }
            | RuntimeEvent::TurnFinished { .. }
            | RuntimeEvent::ConfigChanged { .. } => true,
            RuntimeEvent::TurnFailed { .. }
            | RuntimeEvent::ContextBudgetReached { .. }
            | RuntimeEvent::PermissionResolved { .. }
            | RuntimeEvent::ApprovalPolicyChanged { .. }
            | RuntimeEvent::MemoryInjected { .. }
            | RuntimeEvent::SessionForked { .. }
            | RuntimeEvent::ProjectTrustChanged { .. }
            | RuntimeEvent::UserBash { .. }
            | RuntimeEvent::BackgroundJobStarted { .. }
            | RuntimeEvent::BackgroundJobSettled { .. } => false,
        }
    }

    /// Builds a `PermissionResolved` event (ADR 0006 §5.9). `policy` names the external policy
    /// command when one decided (ADR 0043), `None` for every other approver.
    pub fn permission_resolved(
        request_id: String,
        call_id: String,
        decision: PermissionDecision,
        approver: PermissionApprover,
        reason: Option<String>,
        policy: Option<String>,
    ) -> Self {
        RuntimeEvent::PermissionResolved {
            request_id,
            call_id,
            decision,
            approver,
            reason,
            policy,
        }
    }

    /// Builds a `ProjectTrustChanged` audit event (ADR 0031 T2): the host writes one when it
    /// changes `trust.json`. Non-critical and never replayed.
    pub fn trust_changed(path: String, decision: bool) -> Self {
        RuntimeEvent::ProjectTrustChanged { path, decision }
    }
}

// ---------- wire `data` shapes (ADR 0006 §5.5–§5.7) ----------

#[derive(Serialize)]
pub(crate) struct InboxEnqueuedData<'a> {
    pub message: &'a Message,
    pub message_id: &'a MessageId,
    pub lane: InboxLane,
    pub source: InputSource,
    /// Whether the input wakes an idle driver and may open the next turn itself (ADR 0011
    /// revision 1). Written explicitly; an old file has no such field and the reader derives
    /// it from the lane (ADR 0006 §5.5).
    pub wake: bool,
    /// Whether the input is a wake-gate degrade. Written only when `true` (an ordinary
    /// line stays byte-identical); an old file has no such field and the reader defaults
    /// it to `false` (ADR 0006 §5.5).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub degraded: bool,
}

#[derive(Serialize)]
pub(crate) struct InputClaimedData<'a> {
    pub message_ids: &'a [MessageId],
    pub turn_id: &'a str,
}

#[derive(Serialize)]
pub(crate) struct InputAdmittedData<'a> {
    pub message_id: &'a MessageId,
    pub transcript_index: usize,
}

#[derive(Serialize)]
pub(crate) struct TurnStartedData<'a> {
    pub turn_id: &'a str,
}

#[derive(Serialize)]
pub(crate) struct TurnFinishedData<'a> {
    pub turn_id: &'a str,
    pub outcome: TurnOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a TurnReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct TurnFailedData<'a> {
    pub turn_id: &'a str,
    pub message: &'a str,
}

#[derive(Serialize)]
pub(crate) struct ConfigChangedData<'a> {
    pub revision: u64,
    pub change: &'a Value,
}

#[derive(Serialize)]
pub(crate) struct ContextBudgetReachedData<'a> {
    pub turn_id: &'a str,
    pub source: UsageSource,
    pub projected_tokens: u64,
    pub window_tokens: u64,
    pub threshold_percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_growth_tokens: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct PermissionResolvedData<'a> {
    pub request_id: &'a str,
    pub call_id: &'a str,
    pub decision: PermissionDecision,
    pub approver: PermissionApprover,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct ApprovalPolicyChangedData {
    pub policy: ApprovalPolicyValue,
}

#[derive(Serialize)]
pub(crate) struct SessionForkedData<'a> {
    pub child_session_id: &'a str,
    pub parent_seq: u64,
}

#[derive(Serialize)]
pub(crate) struct MemoryInjectedData<'a> {
    pub records: &'a [MemoryInjectedRecord],
}

#[derive(Serialize)]
pub(crate) struct ProjectTrustChangedData {
    pub path: String,
    pub decision: bool,
}

#[derive(Serialize)]
pub(crate) struct UserBashData<'a> {
    pub command: &'a str,
    pub exclude_from_context: bool,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
}

#[derive(Serialize)]
pub(crate) struct BackgroundJobStartedData<'a> {
    pub job_id: &'a str,
    pub command: &'a str,
    pub output: &'a str,
}

#[derive(Serialize)]
pub(crate) struct BackgroundJobSettledData<'a> {
    pub job_id: &'a str,
    pub state: &'a str,
    pub duration_ms: u64,
}

/// The `data` view of one runtime event. A manual enum (like the transcript writer's
/// `Render`) keeps the field order of the ADR and avoids an externally tagged wrapper.
pub(crate) enum RuntimeData<'a> {
    InboxEnqueued(InboxEnqueuedData<'a>),
    InputClaimed(InputClaimedData<'a>),
    InputAdmitted(InputAdmittedData<'a>),
    TurnStarted(TurnStartedData<'a>),
    TurnFinished(TurnFinishedData<'a>),
    TurnFailed(TurnFailedData<'a>),
    ConfigChanged(ConfigChangedData<'a>),
    ContextBudgetReached(ContextBudgetReachedData<'a>),
    PermissionResolved(PermissionResolvedData<'a>),
    ApprovalPolicyChanged(ApprovalPolicyChangedData),
    MemoryInjected(MemoryInjectedData<'a>),
    SessionForked(SessionForkedData<'a>),
    ProjectTrustChanged(ProjectTrustChangedData),
    UserBash(UserBashData<'a>),
    BackgroundJobStarted(BackgroundJobStartedData<'a>),
    BackgroundJobSettled(BackgroundJobSettledData<'a>),
}

impl Serialize for RuntimeData<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RuntimeData::InboxEnqueued(d) => d.serialize(serializer),
            RuntimeData::InputClaimed(d) => d.serialize(serializer),
            RuntimeData::InputAdmitted(d) => d.serialize(serializer),
            RuntimeData::TurnStarted(d) => d.serialize(serializer),
            RuntimeData::TurnFinished(d) => d.serialize(serializer),
            RuntimeData::TurnFailed(d) => d.serialize(serializer),
            RuntimeData::ConfigChanged(d) => d.serialize(serializer),
            RuntimeData::ContextBudgetReached(d) => d.serialize(serializer),
            RuntimeData::PermissionResolved(d) => d.serialize(serializer),
            RuntimeData::ApprovalPolicyChanged(d) => d.serialize(serializer),
            RuntimeData::MemoryInjected(d) => d.serialize(serializer),
            RuntimeData::SessionForked(d) => d.serialize(serializer),
            RuntimeData::ProjectTrustChanged(d) => d.serialize(serializer),
            RuntimeData::UserBash(d) => d.serialize(serializer),
            RuntimeData::BackgroundJobStarted(d) => d.serialize(serializer),
            RuntimeData::BackgroundJobSettled(d) => d.serialize(serializer),
        }
    }
}

pub(crate) fn runtime_data(event: &RuntimeEvent) -> RuntimeData<'_> {
    match event {
        RuntimeEvent::InboxEnqueued {
            message,
            message_id,
            lane,
            source,
            wake,
            degraded,
        } => RuntimeData::InboxEnqueued(InboxEnqueuedData {
            message,
            message_id,
            lane: *lane,
            source: *source,
            wake: *wake,
            degraded: *degraded,
        }),
        RuntimeEvent::InputClaimed {
            message_ids,
            turn_id,
        } => RuntimeData::InputClaimed(InputClaimedData {
            message_ids,
            turn_id,
        }),
        RuntimeEvent::InputAdmitted {
            message_id,
            transcript_index,
        } => RuntimeData::InputAdmitted(InputAdmittedData {
            message_id,
            transcript_index: *transcript_index,
        }),
        RuntimeEvent::TurnStarted { turn_id } => {
            RuntimeData::TurnStarted(TurnStartedData { turn_id })
        }
        RuntimeEvent::TurnFinished {
            turn_id,
            outcome,
            reason,
            stop_reason,
        } => RuntimeData::TurnFinished(TurnFinishedData {
            turn_id,
            outcome: *outcome,
            reason: reason.as_ref(),
            stop_reason: stop_reason.as_deref(),
        }),
        RuntimeEvent::TurnFailed { turn_id, message } => {
            RuntimeData::TurnFailed(TurnFailedData { turn_id, message })
        }
        RuntimeEvent::ConfigChanged { revision, change } => {
            RuntimeData::ConfigChanged(ConfigChangedData {
                revision: *revision,
                change,
            })
        }
        RuntimeEvent::ContextBudgetReached {
            turn_id,
            source,
            projected_tokens,
            window_tokens,
            threshold_percent,
            baseline_input_tokens,
            estimated_growth_tokens,
        } => RuntimeData::ContextBudgetReached(ContextBudgetReachedData {
            turn_id,
            source: *source,
            projected_tokens: *projected_tokens,
            window_tokens: *window_tokens,
            threshold_percent: *threshold_percent,
            baseline_input_tokens: *baseline_input_tokens,
            estimated_growth_tokens: *estimated_growth_tokens,
        }),
        RuntimeEvent::PermissionResolved {
            request_id,
            call_id,
            decision,
            approver,
            reason,
            policy,
        } => RuntimeData::PermissionResolved(PermissionResolvedData {
            request_id,
            call_id,
            decision: *decision,
            approver: *approver,
            reason: reason.as_deref(),
            policy: policy.as_deref(),
        }),
        RuntimeEvent::ApprovalPolicyChanged { policy } => {
            RuntimeData::ApprovalPolicyChanged(ApprovalPolicyChangedData { policy: *policy })
        }
        RuntimeEvent::MemoryInjected { records } => {
            RuntimeData::MemoryInjected(MemoryInjectedData { records })
        }
        RuntimeEvent::SessionForked {
            child_session_id,
            parent_seq,
        } => RuntimeData::SessionForked(SessionForkedData {
            child_session_id,
            parent_seq: *parent_seq,
        }),
        RuntimeEvent::ProjectTrustChanged { path, decision } => {
            RuntimeData::ProjectTrustChanged(ProjectTrustChangedData {
                path: path.clone(),
                decision: *decision,
            })
        }
        RuntimeEvent::UserBash {
            command,
            exclude_from_context,
            exit_code,
            cancelled,
        } => RuntimeData::UserBash(UserBashData {
            command,
            exclude_from_context: *exclude_from_context,
            exit_code: *exit_code,
            cancelled: *cancelled,
        }),
        RuntimeEvent::BackgroundJobStarted {
            job_id,
            command,
            output,
        } => RuntimeData::BackgroundJobStarted(BackgroundJobStartedData {
            job_id,
            command,
            output,
        }),
        RuntimeEvent::BackgroundJobSettled {
            job_id,
            state,
            duration_ms,
        } => RuntimeData::BackgroundJobSettled(BackgroundJobSettledData {
            job_id,
            state,
            duration_ms: *duration_ms,
        }),
    }
}

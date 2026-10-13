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
    /// The next request would have exceeded the context budget, so it was not sent (ADR
    /// 0015). The numbers ride on a sibling `ContextBudgetReached` event.
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

/// One durable fact the runtime owns. `#[non_exhaustive]`: `ContextCompacted`, `Rewound`,
/// `SessionForked` and `PermissionResolved` will be added when their producers exist.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RuntimeEvent {
    /// An input entered a lane (ADR 0006 §5.5). `source` is always explicit at the runtime
    /// boundary; `message.meta.id` must equal `message_id`.
    InboxEnqueued {
        message: Message,
        message_id: MessageId,
        lane: InboxLane,
        source: InputSource,
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
    /// A turn ended because the model request failed (ADR 0017). Audit only:
    /// `critical = false`. `message` is the adapter's error text, already masked; it is the
    /// only place the host learns *why* a `TurnFinished { outcome: failed }` happened without
    /// reading the live event stream. A reader that skips it still sees the terminal record.
    TurnFailed { turn_id: String, message: String },
    /// A configuration change was applied (ADR 0006 §5.7). `change` stays opaque here; the
    /// registry and the slots own its meaning. The optional `profile_ref` sibling of §5.7 is
    /// not exposed yet (no producer in this phase).
    ConfigChanged { revision: u64, change: Value },
    /// The next model request was not sent because it would have reached the context budget
    /// (ADR 0015 C6). Audit only: `critical = false`. `source` says whether the projection is
    /// a fact, a fact plus an estimate, or purely local; `baseline_input_tokens` and
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
        }
    }

    /// The `critical` flag (ADR 0006 §4). Every runtime event here is critical except
    /// `PermissionResolved`: it is audit, not state, so a reader may skip it.
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
            | RuntimeEvent::PermissionResolved { .. } => false,
        }
    }

    /// Builds a `PermissionResolved` event (ADR 0006 §5.9).
    pub fn permission_resolved(
        request_id: String,
        call_id: String,
        decision: PermissionDecision,
        approver: PermissionApprover,
        reason: Option<String>,
    ) -> Self {
        RuntimeEvent::PermissionResolved {
            request_id,
            call_id,
            decision,
            approver,
            reason,
        }
    }
}

// ---------- wire `data` shapes (ADR 0006 §5.5–§5.7) ----------

#[derive(Serialize)]
pub(crate) struct InboxEnqueuedData<'a> {
    pub message: &'a Message,
    pub message_id: &'a MessageId,
    pub lane: InboxLane,
    pub source: InputSource,
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
        } => RuntimeData::InboxEnqueued(InboxEnqueuedData {
            message,
            message_id,
            lane: *lane,
            source: *source,
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
        } => RuntimeData::PermissionResolved(PermissionResolvedData {
            request_id,
            call_id,
            decision: *decision,
            approver: *approver,
            reason: reason.as_deref(),
        }),
    }
}

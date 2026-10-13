use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::EventSink;
use crate::message::{ContentPart, Message, OpaqueJson, ToolCall};
use crate::tool::ToolSpec;

/// Why the model stopped, as reported by the provider adapter.
///
/// The core decides whether to run tools from the presence of tool calls, not from this value,
/// except for `MaxTokens` and `ContentFiltered`, whose tool calls are never executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    ContentFiltered,
    PauseTurn,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub settings: Value,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub content: Vec<ContentPart>,
    /// The provider's separate reasoning channel for this response, if any. Kept beside
    /// `content` so reasoning never becomes visible history; adapters that have no such
    /// channel leave it empty.
    pub reasoning: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub provider_payload: Option<OpaqueJson>,
    pub stop_reason: StopReason,
    /// Usage the provider reported for this call, if any. The core never fills missing
    /// fields: `None` means "not reported", never zero.
    pub usage: Option<ModelUsage>,
}

/// Provider-reported token usage of one model call, self-contained and provider-neutral
/// (AC-S-6). Every field is optional and serialized only when present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// The provider's own total. **Never computed** from `input + output`: the definition of
    /// total differs between providers (caching counted or not), so an invented number is
    /// worse than no number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    /// The provider's own usage sub-object (e.g. OpenAI's `prompt_tokens_details`), kept
    /// exactly as received: the core does not interpret, reorder or reformat it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<OpaqueJson>,
}

/// The `Display` prefix of [`ModelError::ContextOverflow`]. A host that only receives the
/// error as text (e.g. `RuntimeEvent::TurnFailed.message`, ADR 0017) recognizes the overflow
/// case by this prefix; [`ModelError::ContextOverflow`]'s own `Display` is pinned to it by a
/// test below, so the two cannot drift.
pub const CONTEXT_OVERFLOW_PREFIX: &str = "model context overflow:";

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model transport error: {0}")]
    Transport(String),
    #[error("model protocol error: {0}")]
    Protocol(String),
    /// The provider refused the request because the input is over its context window.
    ///
    /// An adapter returns this **only** when it can recognize the provider's own overflow
    /// error from the HTTP status and body (ADR 0018's "recognizable overflow" rule); it
    /// carries the provider's text, masked. A provider error that merely mentions tokens or
    /// limits is **not** one of these — guessing would misclassify a rate limit or a bad
    /// request. Core and the runtime treat it as an ordinary model error today (ADR 0017: the
    /// turn ends `failed`); the automatic-compaction retry is a later slice.
    #[error("model context overflow: {0}")]
    ContextOverflow(String),
}

pub struct ModelContext<'a> {
    pub cancellation: CancellationToken,
    /// For streaming deltas; the adapter emits them as they arrive.
    pub events: &'a dyn EventSink,
}

/// A provider adapter. It converts the provider-neutral request and returns one complete response.
pub trait Model: Send + Sync {
    fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> impl Future<Output = Result<ModelResponse, ModelError>> + Send;
}

impl<T: Model> Model for Arc<T> {
    fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> impl Future<Output = Result<ModelResponse, ModelError>> + Send {
        (**self).complete(request, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host sees a failed turn as text (`RuntimeEvent::TurnFailed.message`), so the prefix
    /// is the contract between `agent-core` and the host. Pin it here; a host uses
    /// [`CONTEXT_OVERFLOW_PREFIX`].
    #[test]
    fn the_context_overflow_prefix_is_stable() {
        let error = ModelError::ContextOverflow("HTTP 400: prompt is too long".into());
        assert!(
            error.to_string().starts_with(CONTEXT_OVERFLOW_PREFIX),
            "{error}"
        );
        // A transport or protocol error must never be mistaken for an overflow.
        assert!(
            !ModelError::Transport("HTTP 400: bad request".into())
                .to_string()
                .starts_with(CONTEXT_OVERFLOW_PREFIX)
        );
    }
}

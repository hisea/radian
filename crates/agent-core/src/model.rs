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
///
/// **The numbers are the provider's own, unmodified** (ADR 0007 J1). What "input" *means*
/// differs between providers, so the adapter says which it is: see
/// [`ModelUsage::input_includes_cache`] and use [`ModelUsage::prompt_total`] /
/// [`ModelUsage::uncached_input`] instead of adding the fields up by hand (ADR 0007
/// revision 1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// The provider's own input count, **exactly as reported**.
    ///
    /// Whether it already contains the cache hit is what [`Self::input_includes_cache`] says:
    /// OpenAI-compatible `prompt_tokens` **does** contain `prompt_tokens_details.cached_tokens`
    /// (`total_tokens = prompt_tokens + completion_tokens`), while Anthropic's `input_tokens`
    /// is the *uncached* part, a sibling of `cache_read_input_tokens` and
    /// `cache_creation_input_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    /// The provider's own output count. It **contains** the provider's reasoning tokens when
    /// it reports any (OpenAI's `completion_tokens_details.reasoning_tokens`, Anthropic's
    /// `output_tokens_details.thinking_tokens`), which are billed at the output rate: those
    /// sub-objects live in [`Self::details`] and must never be added to this number.
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
    /// Whether [`Self::input`] **already contains** the cache hit (`cache_read`, and
    /// `cache_write` where the provider reports it), filled by the adapter from its own wire
    /// contract and never computed here (ADR 0007):
    ///
    /// - `Some(true)`: `input` is the whole prompt, cache included (OpenAI-compatible:
    ///   `prompt_tokens` contains `prompt_tokens_details.cached_tokens`). Charging `input`
    ///   **and** `cache_read` at their own rates would bill the cached tokens twice, once at
    ///   the full input price — the long-session overcharge the research recorded;
    /// - `Some(false)`: `input` is the uncached part, a sibling of the cache fields
    ///   (Anthropic);
    /// - `None`: not marked. Records written before ADR 0007 are like this; a
    ///   consumer may infer the meaning from the session's provider protocol, and must not
    ///   invent a fee it cannot derive (ADR 0007). The field is a *semantics* flag,
    ///   not a number: it is always written by an adapter that knows, and `None` never means
    ///   "zero".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_includes_cache: Option<bool>,
    /// The provider's own usage sub-object (e.g. OpenAI's `prompt_tokens_details`), kept
    /// exactly as received: the core does not interpret, reorder or reformat it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<OpaqueJson>,
}

impl ModelUsage {
    /// The whole input of the call: the prompt the provider saw, cache reads and cache writes
    /// included. This is what "how big was this request" means, so the context budget (ADR
    /// 0015) uses it.
    ///
    /// - `input_includes_cache == Some(true)`: `input` *is* the prompt.
    /// - `Some(false)`: `input` plus the cache fields it is a sibling of.
    /// - `None` (an old record): `input` alone. Adding the cache fields could double-count
    ///   (that is exactly what the flag exists to disambiguate), so the unmarked number is
    ///   taken at face value; it is then a **lower bound** on the prompt.
    ///
    /// `None` when the provider reported no input count at all: never zero (ADR 0007 J1).
    pub fn prompt_total(&self) -> Option<u64> {
        let cache = add(self.cache_read, self.cache_write);
        match self.input_includes_cache {
            Some(false) => add(self.input, cache),
            // `Some(true)` and the unmarked case: `input` stands alone.
            _ => self.input.or(cache),
        }
    }

    /// The input billed at the **full input rate**: [`Self::input`] minus the cache hit when
    /// the provider counts the hit inside `input` (ADR 0007). The cache fields are
    /// then billed at their own rates, each once.
    ///
    /// - `Some(true)`: `input − cache_read − cache_write` (saturating: a provider that reports
    ///   more cache than input is inconsistent, and the subtraction must not wrap).
    /// - `Some(false)` and `None`: `input` is already the uncached part.
    pub fn uncached_input(&self) -> Option<u64> {
        match self.input_includes_cache {
            Some(true) => self.input.map(|input| {
                let cache = self.cache_read.unwrap_or(0) + self.cache_write.unwrap_or(0);
                input.saturating_sub(cache)
            }),
            _ => self.input,
        }
    }

    /// Whether the provider reported any **billable** counter (input, output, cache read or
    /// cache write). An empty report (`total` alone, or nothing) has nothing to price, which is
    /// how a zero response stays hidden instead of showing `$0.00` (ADR 0027).
    pub fn has_billable_counters(&self) -> bool {
        [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .flatten()
            .any(|count| count > 0)
    }
}

/// `a + b` over optional counts: `None` only when both are `None` (a missing field is "not
/// reported", never a zero).
fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    }
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

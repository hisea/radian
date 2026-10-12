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
    pub tool_calls: Vec<ToolCall>,
    pub provider_payload: Option<OpaqueJson>,
    pub stop_reason: StopReason,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model transport error: {0}")]
    Transport(String),
    #[error("model protocol error: {0}")]
    Protocol(String),
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

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::EventSink;
use crate::message::{ContentPart, ToolCall};

/// Whether a tool may run at the same time as other calls in its batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    /// Runs alone, after every earlier call in the batch has finished. The safe default.
    #[default]
    Exclusive,
    /// May overlap with neighbouring `Shared` calls. Only for tools without conflicting side effects.
    Shared,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The model-facing name.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub concurrency: ToolConcurrency,
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: Vec<ContentPart>,
    pub structured: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentPart::text(text)],
            structured: None,
        }
    }
}

/// A tool's own failure. It becomes a model-visible error result; the turn continues.
#[derive(Debug, Clone)]
pub struct ToolFailure {
    pub code: String,
    pub message: String,
}

impl ToolFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Failure to open or close a tool session.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

pub struct ToolContext<'a> {
    pub cancellation: CancellationToken,
    pub events: &'a dyn EventSink,
    /// Position of the call in its batch.
    pub index: usize,
}

/// Tool execution for one `AgentSession`. Owns any per-session resources, such as child processes.
///
/// `execute` takes `&self` because calls marked `Shared` run concurrently.
pub trait ToolSession: Send + Sync {
    fn tool_specs(&self) -> &[ToolSpec];

    fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> impl Future<Output = Result<ToolOutput, ToolFailure>> + Send;

    /// Releases the session's resources. The core calls this again after a failure or a
    /// dropped attempt, so it must be safe to retry and must not fail just because an
    /// earlier attempt already released some resources.
    fn close(&mut self) -> impl Future<Output = Result<(), ToolError>> + Send {
        async { Ok(()) }
    }
}

/// Opens a `ToolSession` for each `AgentSession`.
pub trait ToolProvider: Send + Sync {
    type Session: ToolSession;

    fn open_session(&self) -> impl Future<Output = Result<Self::Session, ToolError>> + Send;
}

impl<T: ToolProvider> ToolProvider for Arc<T> {
    type Session = T::Session;

    fn open_session(&self) -> impl Future<Output = Result<Self::Session, ToolError>> + Send {
        (**self).open_session()
    }
}

/// A provider with no tools.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTools;

impl ToolSession for NoTools {
    fn tool_specs(&self) -> &[ToolSpec] {
        &[]
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        Err(ToolFailure::new(
            "unknown_tool",
            format!("no tool named {}", call.name),
        ))
    }
}

impl ToolProvider for NoTools {
    type Session = NoTools;

    async fn open_session(&self) -> Result<NoTools, ToolError> {
        Ok(NoTools)
    }
}

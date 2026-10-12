use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::message::{ToolCall, ToolResult};
use crate::model::{ModelRequest, ModelResponse};
use crate::outcome::RunOutcome;

#[derive(Debug, Clone)]
pub struct HookContext {
    /// Cancelled when the turn is cancelled. A hook that waits, such as for human approval,
    /// should stop waiting when this fires.
    pub cancellation: CancellationToken,
    /// Model requests made so far in this turn, including the current one.
    pub model_calls: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    BeforeModelCall,
    AfterModelResponse,
    BeforeToolCall,
    AfterToolCall,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HookError(pub String);

impl HookError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolDecision {
    Allow,
    /// Do not run the tool; the model sees an error result with this reason.
    Deny {
        reason: String,
    },
    /// Run with these arguments instead. They are validated again; hooks are not re-run.
    Rewrite {
        arguments: Value,
    },
    /// Do not run the tool; use this result. Its `call_id` is overwritten.
    Respond(ToolResult),
}

/// Policy points the core awaits. Unlike events, hooks can change what happens.
///
/// Errors fail closed: a model hook error fails the turn, a `before_tool_call` error denies the
/// call, and an `after_tool_call` error replaces the result so the unprocessed output never
/// reaches the model.
pub trait Hooks: Send + Sync {
    /// Runs before each model request. Changes affect only this request, not the transcript.
    fn before_model_call(
        &self,
        _cx: &HookContext,
        _request: &mut ModelRequest,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        async { Ok(()) }
    }

    /// Runs after a complete response arrives, before it is written to the transcript.
    fn after_model_response(
        &self,
        _cx: &HookContext,
        _response: &ModelResponse,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        async { Ok(()) }
    }

    /// Runs after a call passes validation and before it is scheduled. In a batch, these run
    /// one at a time in call order, so approval prompts appear in the model's order.
    fn before_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        async { Ok(ToolDecision::Allow) }
    }

    /// Runs on each executed call's result before it is written. May run concurrently for
    /// parallel calls.
    fn after_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
        _result: &mut ToolResult,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        async { Ok(()) }
    }

    /// Runs once when the turn reaches a terminal state, after `RunFinished` is emitted.
    fn on_turn_end(&self, _cx: &HookContext, _outcome: &RunOutcome) {}
}

impl Hooks for () {}

impl<T: Hooks> Hooks for Arc<T> {
    fn before_model_call(
        &self,
        cx: &HookContext,
        request: &mut ModelRequest,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        (**self).before_model_call(cx, request)
    }

    fn after_model_response(
        &self,
        cx: &HookContext,
        response: &ModelResponse,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        (**self).after_model_response(cx, response)
    }

    fn before_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        (**self).before_tool_call(cx, call)
    }

    fn after_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
        result: &mut ToolResult,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        (**self).after_tool_call(cx, call, result)
    }

    fn on_turn_end(&self, cx: &HookContext, outcome: &RunOutcome) {
        (**self).on_turn_end(cx, outcome)
    }
}

/// Runs `A` then `B`. For `before_tool_call`, the first decision other than `Allow` wins,
/// and a `Rewrite` from `A` is what `B` sees.
impl<A: Hooks, B: Hooks> Hooks for (A, B) {
    async fn before_model_call(
        &self,
        cx: &HookContext,
        request: &mut ModelRequest,
    ) -> Result<(), HookError> {
        self.0.before_model_call(cx, request).await?;
        self.1.before_model_call(cx, request).await
    }

    async fn after_model_response(
        &self,
        cx: &HookContext,
        response: &ModelResponse,
    ) -> Result<(), HookError> {
        self.0.after_model_response(cx, response).await?;
        self.1.after_model_response(cx, response).await
    }

    async fn before_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        match self.0.before_tool_call(cx, call).await? {
            ToolDecision::Allow => self.1.before_tool_call(cx, call).await,
            ToolDecision::Rewrite { arguments } => {
                let rewritten = ToolCall {
                    arguments,
                    ..call.clone()
                };
                match self.1.before_tool_call(cx, &rewritten).await? {
                    ToolDecision::Allow => Ok(ToolDecision::Rewrite {
                        arguments: rewritten.arguments,
                    }),
                    other => Ok(other),
                }
            }
            other => Ok(other),
        }
    }

    async fn after_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        self.0.after_tool_call(cx, call, result).await?;
        self.1.after_tool_call(cx, call, result).await
    }

    fn on_turn_end(&self, cx: &HookContext, outcome: &RunOutcome) {
        self.0.on_turn_end(cx, outcome);
        self.1.on_turn_end(cx, outcome);
    }
}

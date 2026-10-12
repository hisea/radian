//! Test adapters and gates shared by the integration tests.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::*;
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

/// A gate a test holds a model call or a tool call open on. No sleeps anywhere: the holder
/// signals `entered`, then waits for a semaphore permit; the test waits for the holder and
/// then opens the gate.
#[derive(Clone)]
pub struct Gate {
    entered: Arc<AtomicUsize>,
    wake: Arc<Notify>,
    release: Arc<Semaphore>,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            entered: Arc::new(AtomicUsize::new(0)),
            wake: Arc::new(Notify::new()),
            release: Arc::new(Semaphore::new(0)),
        }
    }
}

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park the current call here: signal that it entered, then wait for [`Gate::open`].
    pub async fn hold(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_one();
        let permit = self
            .release
            .clone()
            .acquire_owned()
            .await
            .expect("gate opened while holders are parked");
        permit.forget();
    }

    /// Wait until `n` holders are parked.
    pub async fn wait_entered(&self, n: usize) {
        loop {
            if self.entered.load(Ordering::SeqCst) >= n {
                return;
            }
            self.wake.notified().await;
        }
    }

    /// Let every parked and future holder through. The permit count is far above anything a
    /// test parks; tokio caps the total at `usize::MAX >> 3`, so do not pass `usize::MAX`.
    pub fn open(&self) {
        self.release.add_permits(1 << 20);
    }
}

fn response_text(text: &str) -> ModelResponse {
    ModelResponse {
        content: vec![ContentPart::text(text)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

fn response_tool_call(id: &str, name: &str) -> ModelResponse {
    ModelResponse {
        content: vec![],
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }],
        provider_payload: None,
        stop_reason: StopReason::ToolUse,
        usage: None,
    }
}

/// One concrete adapter: always replies with the same text. Can hold itself open on a gate.
#[derive(Clone)]
pub struct ConstantModel {
    pub text: &'static str,
    pub calls: Arc<AtomicUsize>,
    pub gate: Option<Arc<Gate>>,
}

impl ConstantModel {
    pub fn new(text: &'static str) -> Self {
        Self {
            text,
            calls: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }

    pub fn gated(text: &'static str, gate: Arc<Gate>) -> Self {
        Self {
            text,
            calls: Arc::new(AtomicUsize::new(0)),
            gate: Some(gate),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Model for ConstantModel {
    async fn complete(
        &self,
        _request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.hold().await;
        }
        Ok(response_text(self.text))
    }
}

/// One scripted reply.
#[derive(Debug, Clone)]
pub enum Reply {
    Text(&'static str),
    ToolCall {
        id: &'static str,
        name: &'static str,
    },
}

/// Another concrete adapter: serves `Reply` items in order (a tool call, then a text, then
/// the tool loop asks again and the turn ends).
#[derive(Clone)]
pub struct ScriptedModel {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    pub calls: Arc<AtomicUsize>,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Model for ScriptedModel {
    async fn complete(
        &self,
        _request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.replies.lock().unwrap().pop_front() {
            Some(Reply::Text(text)) => Ok(response_text(text)),
            Some(Reply::ToolCall { id, name }) => Ok(response_tool_call(id, name)),
            None => Err(ModelError::Protocol("script exhausted".into())),
        }
    }
}

/// A minimal tool provider with one `echo` tool. `before_execute` runs at the start of each
/// execution; a gate parks each execution before it records.
#[derive(Clone)]
pub struct EchoTools {
    pub name: &'static str,
    pub executed: Arc<Mutex<Vec<String>>>,
    pub before_execute: Option<Arc<dyn Fn() + Send + Sync>>,
    pub gate: Option<Arc<Gate>>,
}

impl std::fmt::Debug for EchoTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoTools")
            .field("name", &self.name)
            .field("executed", &self.executed.lock().unwrap().clone())
            .finish_non_exhaustive()
    }
}

impl EchoTools {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            executed: Arc::default(),
            before_execute: None,
            gate: None,
        }
    }

    pub fn gated(name: &'static str, gate: Arc<Gate>) -> Self {
        Self {
            name,
            executed: Arc::default(),
            before_execute: None,
            gate: Some(gate),
        }
    }

    pub fn with_before_execute(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_execute = Some(Arc::new(f));
        self
    }

    pub fn executed(&self) -> Vec<String> {
        self.executed.lock().unwrap().clone()
    }
}

pub struct EchoSession {
    spec: ToolSpec,
    executed: Arc<Mutex<Vec<String>>>,
    before_execute: Option<Arc<dyn Fn() + Send + Sync>>,
    gate: Option<Arc<Gate>>,
}

impl ToolProvider for EchoTools {
    type Session = EchoSession;

    async fn open_session(&self) -> Result<EchoSession, ToolError> {
        Ok(EchoSession {
            spec: ToolSpec {
                name: self.name.into(),
                description: "answers with its arguments".into(),
                input_schema: json!({"type": "object"}),
                concurrency: ToolConcurrency::Exclusive,
            },
            executed: Arc::clone(&self.executed),
            before_execute: self.before_execute.clone(),
            gate: self.gate.clone(),
        })
    }
}

impl ToolSession for EchoSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        std::slice::from_ref(&self.spec)
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if let Some(cb) = &self.before_execute {
            cb();
        }
        if let Some(gate) = &self.gate {
            gate.hold().await;
        }
        self.executed.lock().unwrap().push(call.id.clone());
        Ok(ToolOutput::text("ok"))
    }
}

/// The text written by the tool result for `call_id`, if any.
pub fn tool_result_text(transcript: &[Message], call_id: &str) -> Option<String> {
    transcript.iter().find_map(|m| match m {
        Message::Tool { result, .. } if result.call_id == call_id => Some(result.text()),
        _ => None,
    })
}

/// The text of the transcript's last assistant message, if any.
pub fn last_assistant(transcript: &[Message]) -> Option<String> {
    transcript.iter().rev().find_map(|m| match m {
        Message::Assistant { .. } => Some(m.text()),
        _ => None,
    })
}

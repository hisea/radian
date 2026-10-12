#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::*;
use serde_json::{Value, json};

// ---------- model ----------

pub enum Reply {
    Respond(ModelResponse),
    Fail(String),
    /// Waits until cancelled.
    Hang,
    /// Responds after this many milliseconds.
    Slow(u64, ModelResponse),
    /// Emits these text deltas, then waits until cancelled.
    StreamHang(Vec<String>),
    /// Emits these text deltas, then responds.
    Streamed(Vec<String>, ModelResponse),
}

/// Returns scripted replies in order and records every request.
#[derive(Default)]
pub struct ScriptedModel {
    script: Mutex<VecDeque<Reply>>,
    pub requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::default(),
        })
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn push(&self, reply: Reply) {
        self.script.lock().unwrap().push_back(reply);
    }
}

impl Model for ScriptedModel {
    async fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(Reply::Respond(r)) => Ok(r),
            Some(Reply::Fail(m)) => Err(ModelError::Transport(m)),
            Some(Reply::Hang) => {
                cx.cancellation.cancelled().await;
                Err(ModelError::Transport("cancelled".into()))
            }
            Some(Reply::Slow(ms, r)) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(r)
            }
            Some(Reply::StreamHang(deltas)) => {
                for text in deltas {
                    cx.events.emit(AgentEvent::ModelTextDelta { text });
                }
                cx.cancellation.cancelled().await;
                Err(ModelError::Transport("cancelled".into()))
            }
            Some(Reply::Streamed(deltas, r)) => {
                for text in deltas {
                    cx.events.emit(AgentEvent::ModelTextDelta { text });
                }
                Ok(r)
            }
            None => Err(ModelError::Protocol("script exhausted".into())),
        }
    }
}

pub fn text(s: &str) -> Reply {
    Reply::Respond(ModelResponse {
        content: vec![ContentPart::text(s)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
    })
}

pub fn slow_text(ms: u64, s: &str) -> Reply {
    let Reply::Respond(r) = text(s) else {
        unreachable!()
    };
    Reply::Slow(ms, r)
}

pub fn stop(reason: StopReason, s: &str) -> Reply {
    Reply::Respond(ModelResponse {
        content: vec![ContentPart::text(s)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: reason,
    })
}

pub fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

pub fn calls(list: Vec<ToolCall>) -> Reply {
    calls_with(StopReason::ToolUse, list)
}

pub fn calls_with(reason: StopReason, list: Vec<ToolCall>) -> Reply {
    Reply::Respond(ModelResponse {
        content: vec![],
        tool_calls: list,
        provider_payload: None,
        stop_reason: reason,
    })
}

// ---------- tools ----------

#[derive(Clone)]
pub enum Behavior {
    /// Returns its arguments as text.
    Echo,
    Fail,
    /// Sleeps, then echoes.
    Delay(u64),
    /// Waits until cancelled.
    Hang,
    /// Ignores cancellation and never finishes.
    Stubborn,
    /// On cancellation, spends this many milliseconds cleaning up, then records it.
    CleansUp(u64),
}

#[derive(Default)]
pub struct ToolLog {
    pub cleaned_up: Mutex<Vec<String>>,
    pub started: Mutex<Vec<String>>,
    pub finished: Mutex<Vec<String>>,
    current: AtomicUsize,
    pub max_concurrent: AtomicUsize,
    /// `close()` fails this many more times before succeeding.
    pub close_failures_left: AtomicUsize,
    /// Each `close()` attempt sleeps this long first.
    pub close_delay_ms: AtomicU64,
    pub close_calls: AtomicUsize,
    pub released: AtomicBool,
}

impl ToolLog {
    pub fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }
    pub fn finished(&self) -> Vec<String> {
        self.finished.lock().unwrap().clone()
    }
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent.load(Ordering::SeqCst)
    }
}

struct Running<'a>(&'a ToolLog);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.current.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct MemTools {
    tools: Arc<Vec<(ToolSpec, Behavior)>>,
    pub log: Arc<ToolLog>,
}

impl MemTools {
    pub fn new(tools: Vec<(ToolSpec, Behavior)>) -> Self {
        Self {
            tools: Arc::new(tools),
            log: Arc::default(),
        }
    }
}

pub struct MemSession {
    specs: Vec<ToolSpec>,
    tools: Arc<Vec<(ToolSpec, Behavior)>>,
    log: Arc<ToolLog>,
}

impl ToolProvider for MemTools {
    type Session = MemSession;

    async fn open_session(&self) -> Result<MemSession, ToolError> {
        Ok(MemSession {
            specs: self.tools.iter().map(|(s, _)| s.clone()).collect(),
            tools: Arc::clone(&self.tools),
            log: Arc::clone(&self.log),
        })
    }
}

impl ToolSession for MemSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn close(&mut self) -> Result<(), ToolError> {
        let log = &self.log;
        log.close_calls.fetch_add(1, Ordering::SeqCst);
        let delay = log.close_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        // close() takes &mut self, so attempts never overlap; load then store is enough.
        let left = log.close_failures_left.load(Ordering::SeqCst);
        let failing = left > 0;
        if failing {
            log.close_failures_left.store(left - 1, Ordering::SeqCst);
        }
        if failing {
            return Err(ToolError("server did not exit".into()));
        }
        log.released.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let behavior = self
            .tools
            .iter()
            .find(|(s, _)| s.name == call.name)
            .map(|(_, b)| b.clone())
            .expect("core only dispatches declared tools");
        self.log.started.lock().unwrap().push(call.id.clone());
        let now = self.log.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.log.max_concurrent.fetch_max(now, Ordering::SeqCst);
        let _running = Running(&self.log);
        let result = match behavior {
            Behavior::Echo => Ok(ToolOutput::text(call.arguments.to_string())),
            Behavior::Fail => Err(ToolFailure::new("boom", "the tool failed")),
            Behavior::Delay(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(ToolOutput::text(call.arguments.to_string()))
            }
            Behavior::Hang => {
                cx.cancellation.cancelled().await;
                Err(ToolFailure::new("cancelled", "cancelled"))
            }
            Behavior::Stubborn => std::future::pending().await,
            Behavior::CleansUp(ms) => {
                cx.cancellation.cancelled().await;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                self.log.cleaned_up.lock().unwrap().push(call.id.clone());
                Err(ToolFailure::new("cancelled", "cancelled"))
            }
        };
        self.log.finished.lock().unwrap().push(call.id.clone());
        result
    }
}

pub fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("test tool {name}"),
        input_schema: json!({"type": "object"}),
        concurrency: ToolConcurrency::Exclusive,
    }
}

pub fn shared(name: &str) -> ToolSpec {
    ToolSpec {
        concurrency: ToolConcurrency::Shared,
        ..spec(name)
    }
}

/// Requires an integer `x`.
pub fn typed(name: &str) -> ToolSpec {
    ToolSpec {
        input_schema: json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "required": ["x"]
        }),
        ..spec(name)
    }
}

// ---------- events ----------

#[derive(Default)]
pub struct Recorder(Mutex<Vec<AgentEvent>>);

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Recorder {
    pub fn events(&self) -> Vec<AgentEvent> {
        self.0.lock().unwrap().clone()
    }

    pub fn finished(&self) -> Vec<RunStatus> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::RunFinished { status } => Some(status),
                _ => None,
            })
            .collect()
    }

    pub fn tool_finish_order(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ToolFinished { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect()
    }
}

// ---------- transcript helpers ----------

pub fn result_ids(transcript: &[Message]) -> Vec<String> {
    transcript
        .iter()
        .filter_map(|m| match m {
            Message::Tool { result } => Some(result.call_id.clone()),
            _ => None,
        })
        .collect()
}

pub fn result_for<'a>(transcript: &'a [Message], id: &str) -> &'a ToolResult {
    transcript
        .iter()
        .find_map(|m| match m {
            Message::Tool { result } if result.call_id == id => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

pub fn code(result: &ToolResult) -> Option<&str> {
    result.error_code.as_deref()
}

pub fn agent<P: ToolProvider>(
    model: Arc<ScriptedModel>,
    tools: P,
) -> Arc<Agent<Arc<ScriptedModel>, P>> {
    Arc::new(Agent::new(model, tools))
}

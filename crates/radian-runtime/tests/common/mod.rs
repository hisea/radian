#![allow(dead_code)]

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{
    AgentEvent, ContentPart, EventSink, Model, ModelContext, ModelError, ModelRequest,
    ModelResponse, StopReason, ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure,
    ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_runtime::{ConfigQueue, ConfigSlot};
use radian_session::SessionCreatedData;
use serde_json::{Value, json};

// ---------- temp dirs ----------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under `std::env::temp_dir()`, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("radian-runtime-tests-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = root.join(format!("{label}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub fn created() -> SessionCreatedData {
    SessionCreatedData::new(
        "/work/example",
        json!({"model": {"provider": "example", "model": "m-1"}}),
    )
}

// ---------- live events ----------

#[derive(Default)]
pub struct Recorder {
    events: Mutex<Vec<AgentEvent>>,
    finished: tokio::sync::Notify,
}

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::RunFinished { .. }) {
            self.finished.notify_one();
        }
        self.events.lock().unwrap().push(event);
    }
}

impl Recorder {
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn appended(&self) -> Vec<(usize, String)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::TranscriptAppended { index, message } => Some((index, message.text())),
                _ => None,
            })
            .collect()
    }

    pub async fn wait_finished(&self) {
        self.finished.notified().await;
    }
}

// ---------- scripted model ----------

/// Returns scripted replies in order, records every request, and can watch the applied
/// configuration (for the turn-boundary test).
/// What the model watches for the turn-boundary test.
type ConfigWatch = (Arc<ConfigSlot>, Arc<ConfigQueue>, Value, usize);

#[derive(Default)]
pub struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    pub requests: Mutex<Vec<ModelRequest>>,
    /// The config slot to observe, the queue to push into, the change to push, and the call
    /// index at which to push it.
    config: Mutex<Option<ConfigWatch>>,
    /// (call index, applied value, applied revision) for every call.
    pub seen: Mutex<Vec<(usize, Value, u64)>>,
    /// Call indexes that hang forever (for the deadline test).
    hang: Mutex<Vec<usize>>,
    /// Call indexes that delay by N milliseconds (a slow model, so a turn stays in flight).
    slow: Mutex<Vec<(usize, u64)>>,
    /// Call indexes that park until `release()`: the call signals `parked` and waits. A
    /// deterministically held model response, no sleep (the same shape as `MemTools::gate`).
    park: Mutex<Vec<usize>>,
    /// Notified when a parked call begins. A `Notify` stores one permit, so a test may await
    /// `wait_parked()` after the call parked and still see it.
    parked: tokio::sync::Notify,
    /// Notified by `release()` to let a parked call return.
    release: tokio::sync::Notify,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            config: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
            hang: Mutex::new(Vec::new()),
            slow: Mutex::new(Vec::new()),
            park: Mutex::new(Vec::new()),
            parked: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }

    /// Queue `change` on the `at_call`-th model call (0-based), while observing `slot`.
    pub fn watch_config(
        self: &Arc<Self>,
        slot: Arc<ConfigSlot>,
        queue: Arc<ConfigQueue>,
        change: Value,
        at_call: usize,
    ) {
        *self.config.lock().unwrap() = Some((slot, queue, change, at_call));
    }

    /// The `index`-th call hangs forever.
    pub fn hang_at(self: &Arc<Self>, index: usize) {
        self.hang.lock().unwrap().push(index);
    }

    /// The `index`-th call waits `ms` before returning. Used only to keep a turn in flight
    /// while the test does something else; the assertion is never "how long it took".
    pub fn slow_at(self: &Arc<Self>, index: usize, ms: u64) {
        self.slow.lock().unwrap().push((index, ms));
    }

    /// The `index`-th call parks: it signals [`ScriptedModel::wait_parked`] and waits for
    /// [`ScriptedModel::release`]. Deterministic, unlike `slow_at`: a test can enqueue input
    /// while the call is provably in flight, then let the call return.
    pub fn park_at(self: &Arc<Self>, index: usize) {
        self.park.lock().unwrap().push(index);
    }

    /// Waits until a parked call has begun. A stored permit makes this safe to await after
    /// the call parked.
    pub async fn wait_parked(&self) {
        self.parked.notified().await;
    }

    /// Lets a parked call return.
    pub fn release(&self) {
        self.release.notify_one();
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn seen(&self) -> Vec<(usize, Value, u64)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Model for ScriptedModel {
    async fn complete(
        &self,
        request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() - 1
        };
        if let Some((_, queue, change, at)) = self.config.lock().unwrap().clone()
            && index == at
        {
            queue.push(change);
        }
        if let Some((slot, _, _, _)) = self.config.lock().unwrap().clone() {
            self.seen
                .lock()
                .unwrap()
                .push((index, slot.value(), slot.revision()));
        }
        if self.hang.lock().unwrap().contains(&index) {
            return std::future::pending::<Result<ModelResponse, ModelError>>().await;
        }
        let slow_ms = self
            .slow
            .lock()
            .unwrap()
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, ms)| *ms);
        if let Some(ms) = slow_ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        if self.park.lock().unwrap().contains(&index) {
            // The test can now enqueue input while this call is provably in flight; it
            // resumes only when the test calls `release()`.
            self.parked.notify_one();
            self.release.notified().await;
        }
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::Protocol("script exhausted".into()))
    }
}

pub fn text(s: &str) -> ModelResponse {
    ModelResponse {
        limits: None,
        content: vec![ContentPart::text(s)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
        reasoning: Vec::new(),
    }
}

pub fn calls(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        limits: None,
        content: vec![],
        tool_calls: calls,
        provider_payload: None,
        stop_reason: StopReason::ToolUse,
        usage: None,
        reasoning: Vec::new(),
    }
}

pub fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

// ---------- tools ----------

#[derive(Clone)]
pub enum Behavior {
    Echo,
    /// Signals `started`, then waits for `gate`.
    Gate,
}

#[derive(Clone)]
pub struct MemTools {
    pub log: Arc<Mutex<Vec<String>>>,
    pub started: Arc<tokio::sync::Notify>,
    pub gate: Arc<tokio::sync::Notify>,
}

impl Default for MemTools {
    fn default() -> Self {
        Self::new()
    }
}

impl MemTools {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(tokio::sync::Notify::new()),
            gate: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn started_ids(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

pub struct MemSession {
    log: Arc<Mutex<Vec<String>>>,
    started: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
    specs: Vec<ToolSpec>,
}

impl ToolProvider for MemTools {
    type Session = MemSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(MemSession {
            log: Arc::clone(&self.log),
            started: Arc::clone(&self.started),
            gate: Arc::clone(&self.gate),
            specs: vec![
                ToolSpec {
                    name: "echo".into(),
                    description: "echoes its arguments".into(),
                    input_schema: json!({"type": "object"}),
                    concurrency: ToolConcurrency::Exclusive,
                },
                ToolSpec {
                    name: "gate".into(),
                    description: "waits for the test to release it".into(),
                    input_schema: json!({"type": "object"}),
                    concurrency: ToolConcurrency::Exclusive,
                },
                ToolSpec {
                    name: "write".into(),
                    description: "writes `text` to the file at `path` (an observable side effect)"
                        .into(),
                    input_schema: json!({"type": "object"}),
                    concurrency: ToolConcurrency::Exclusive,
                },
            ],
        })
    }
}

impl ToolSession for MemSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> impl Future<Output = Result<ToolOutput, ToolFailure>> + Send {
        let log = Arc::clone(&self.log);
        let started = Arc::clone(&self.started);
        let gate = Arc::clone(&self.gate);
        let id = call.id.clone();
        let name = call.name.clone();
        let args = call.arguments.to_string();
        let write_path = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let write_text = call
            .arguments
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned();
        async move {
            log.lock().unwrap().push(id.clone());
            if name == "write" {
                // The observable side effect the crash tests reason about: the file exists iff
                // this call actually ran.
                let path = write_path.expect("the write tool needs a `path` argument");
                std::fs::write(&path, write_text).expect("the write tool writes its file");
                return Ok(ToolOutput::text(path));
            }
            let behavior = if name == "gate" {
                Behavior::Gate
            } else {
                Behavior::Echo
            };
            match behavior {
                Behavior::Echo => Ok(ToolOutput::text(args)),
                Behavior::Gate => {
                    started.notify_waiters();
                    gate.notified().await;
                    Ok(ToolOutput::text("released"))
                }
            }
        }
    }
}

// ---------- journal helpers ----------

/// `(event, seq, raw data)` triples, read from the file's own bytes so the test does not
/// depend on how the reader types each event.
pub fn events(path: &Path) -> Vec<(String, u64, Value)> {
    let session = radian_session::Session::open(path).unwrap();
    session
        .events()
        .iter()
        .map(|event| {
            let line =
                radian_session::read_line_at(path, event.byte_offset, event.line_len).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            (event.event.clone(), event.seq, value["data"].clone())
        })
        .collect()
}

pub fn seq_of(path: &Path, event: &str, field: &str, value: &str) -> u64 {
    events(path)
        .into_iter()
        .find(|(name, _, data)| name == event && data[field] == serde_json::json!(value))
        .map(|(_, seq, _)| seq)
        .unwrap_or_else(|| panic!("no {event} with {field}={value} in {path:?}"))
}

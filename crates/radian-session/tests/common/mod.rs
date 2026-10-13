#![allow(dead_code)]

pub mod long_session;

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{
    ContentPart, Model, ModelContext, ModelError, ModelRequest, ModelResponse, StopReason,
    ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider,
    ToolSession, ToolSpec,
};
use radian_session::SessionCreatedData;
use serde_json::{Value, json};

/// The `SessionCreated` payload used across the session tests.
pub fn created() -> SessionCreatedData {
    SessionCreatedData::new(
        "/work/example",
        json!({"model": {"provider": "example", "model": "m-1"}}),
    )
}

// ---------- temp dirs (kept outside the repo, cleaned up on drop) ----------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under `std::env::temp_dir()`, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("radian-session-tests-{}", std::process::id()));
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

/// The repository's fixture directory, read directly (never copied into the test tree).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schema/fixtures")
}

pub fn fixture(name: &str) -> PathBuf {
    fixtures_dir().join(name)
}

// ---------- scripted model ----------

/// Returns scripted replies in order and records every request.
#[derive(Default)]
pub struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    pub requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::default(),
        })
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Model for ScriptedModel {
    async fn complete(
        &self,
        request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::Protocol("script exhausted".into()))
    }
}

pub fn text(s: &str) -> ModelResponse {
    ModelResponse {
        reasoning: Vec::new(),
        content: vec![ContentPart::text(s)],
        tool_calls: vec![],
        provider_payload: None,
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

pub fn calls(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        reasoning: Vec::new(),
        content: vec![],
        tool_calls: calls,
        provider_payload: None,
        stop_reason: StopReason::ToolUse,
        usage: None,
    }
}

pub fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

// ---------- a minimal one-tool session ----------

pub struct MemTools {
    pub log: Arc<Mutex<Vec<String>>>,
}

impl MemTools {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Default for MemTools {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MemSession {
    log: Arc<Mutex<Vec<String>>>,
    specs: Vec<ToolSpec>,
}

impl ToolProvider for MemTools {
    type Session = MemSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(MemSession {
            log: Arc::clone(&self.log),
            specs: vec![ToolSpec {
                name: "echo".into(),
                description: "echoes its arguments".into(),
                input_schema: json!({"type": "object"}),
                concurrency: ToolConcurrency::Exclusive,
            }],
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
        let id = call.id.clone();
        let args = call.arguments.to_string();
        async move {
            log.lock().unwrap().push(id);
            Ok(ToolOutput::text(args))
        }
    }
}

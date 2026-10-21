#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use radian_core::{CancellationToken, HookContext, ToolCall};
use futures_util::future::BoxFuture;
use radian_permissions::{ApprovalError, ApprovalOutcome, ApprovalRequest, Approver};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore};

/// A unique temp directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-perms-test-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments,
    }
}

pub fn ctx() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 0,
    }
}

/// A state gate: the approver parks on `hold` until the test calls `open`, so a test can
/// cancel while the approver is waiting without using a sleep.
pub struct Gate {
    started: AtomicBool,
    wake: Notify,
    release: Arc<Semaphore>,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: AtomicBool::new(false),
            wake: Notify::new(),
            release: Arc::new(Semaphore::new(0)),
        })
    }
    async fn hold(&self) {
        self.started.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        let permit = self
            .release
            .clone()
            .acquire_owned()
            .await
            .expect("gate open");
        permit.forget();
    }
    pub async fn wait_started(&self) {
        loop {
            if self.started.load(Ordering::SeqCst) {
                return;
            }
            self.wake.notified().await;
        }
    }
    pub fn open(&self) {
        self.release.add_permits(1 << 20);
    }
    pub fn released(&self) -> bool {
        self.release.available_permits() > 0
    }
}

/// A scripted approver: a queue of answers, an optional gate, an optional error, and a call
/// counter.
pub struct ScriptedApprover {
    pub calls: AtomicUsize,
    outcomes: Mutex<VecDeque<ApprovalOutcome>>,
    gate: Option<Arc<Gate>>,
    error: Option<String>,
}

impl ScriptedApprover {
    pub fn scripted(outcomes: Vec<ApprovalOutcome>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            outcomes: Mutex::new(outcomes.into()),
            gate: None,
            error: None,
        })
    }
    pub fn once(outcome: ApprovalOutcome) -> Arc<Self> {
        Self::scripted(vec![outcome])
    }
    pub fn gated(gate: Arc<Gate>, outcomes: Vec<ApprovalOutcome>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            outcomes: Mutex::new(outcomes.into()),
            gate: Some(gate),
            error: None,
        })
    }
    pub fn failing(message: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            outcomes: Mutex::new(VecDeque::new()),
            gate: None,
            error: Some(message.into()),
        })
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Approver for ScriptedApprover {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.hold().await;
            }
            if let Some(error) = &self.error {
                return Err(ApprovalError(error.clone()));
            }
            Ok(self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ApprovalOutcome::Deny { reason: None }))
        })
    }
}

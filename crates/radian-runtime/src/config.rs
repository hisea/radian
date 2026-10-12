//! The turn-boundary configuration slot (ADR 0004 path B).
//!
//! Configuration changes are **not** applied when they are queued; the driver drains the
//! queue and applies it immediately before the next `start_turn`. A model call already in
//! flight never sees a change, and neither does a later call in the same turn — only the
//! next turn does. This is deliberately a different mechanism from the inbox's step-safe
//! points (ADR 0011 L5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value;

/// The configuration a turn sees. The host (and its model adapter) reads it; only the
/// driver writes it, and only at a turn boundary.
#[derive(Debug, Default)]
pub struct ConfigSlot {
    value: RwLock<Value>,
    revision: AtomicU64,
}

impl ConfigSlot {
    pub fn new(initial: Value) -> Arc<Self> {
        Arc::new(Self {
            value: RwLock::new(initial),
            revision: AtomicU64::new(0),
        })
    }

    /// The currently applied value.
    pub fn value(&self) -> Value {
        self.value.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The revision of the currently applied value (0 = the initial value).
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    pub(crate) fn apply(&self, value: Value, revision: u64) {
        *self.value.write().unwrap_or_else(|e| e.into_inner()) = value;
        self.revision.store(revision, Ordering::SeqCst);
    }
}

/// Changes queued by the host. The driver drains this at the next turn boundary.
#[derive(Debug, Default)]
pub struct ConfigQueue {
    pending: Mutex<Vec<Value>>,
}

impl ConfigQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queues a change. It takes effect at the next turn boundary, not now.
    pub fn push(&self, change: Value) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(change);
    }

    pub fn len(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn drain(&self) -> Vec<Value> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

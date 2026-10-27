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

    /// The last queued **model** route, when a route change is waiting for the next turn
    /// boundary; `None` when no route change is queued. Reads without draining, so a host can
    /// avoid queueing a redundant route after another switch (ADR 0026 D10).
    pub fn pending_model_route(&self) -> Option<ModelRoute> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find_map(ModelRoute::from_value)
    }

    pub(crate) fn drain(&self) -> Vec<Value> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// One route a `/model` switch can name (ADR 0026 D5). This is the **only** content of a
/// model `ConfigChanged`: no key, no header values — the key comes from `auth.json` at use
/// time, so the journal can never hold a secret through a switch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRoute {
    pub provider: String,
    pub model: String,
}

impl ModelRoute {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// The `ConfigChanged` value this route is: `{"model": {"provider": …, "model": …}}`.
    pub fn to_value(&self) -> Value {
        serde_json::json!({"model": {"provider": self.provider, "model": self.model}})
    }

    /// Reads the route out of a slot value; `None` when the change is not a model change.
    pub fn from_value(value: &Value) -> Option<Self> {
        let model = value.get("model")?;
        Some(Self {
            provider: model.get("provider")?.as_str()?.to_owned(),
            model: model.get("model")?.as_str()?.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0026 D10: a host that wants to queue a route must see a switch already waiting,
    /// without draining it. The last model change wins; other changes are not routes.
    #[test]
    fn the_pending_model_route_is_read_without_draining_the_queue() {
        let queue = ConfigQueue::new();
        assert_eq!(queue.pending_model_route(), None);
        queue.push(serde_json::json!({"approval": "auto"}));
        assert_eq!(queue.pending_model_route(), None);
        queue.push(ModelRoute::new("magpie", "old").to_value());
        assert_eq!(
            queue.pending_model_route(),
            Some(ModelRoute::new("magpie", "old"))
        );
        queue.push(ModelRoute::new("openai", "gpt").to_value());
        assert_eq!(
            queue.pending_model_route(),
            Some(ModelRoute::new("openai", "gpt")),
            "the last queued route is the one that will apply"
        );
        assert_eq!(queue.len(), 3, "reading did not drain");
        assert_eq!(queue.drain().len(), 3);
    }
}

//! Demonstration shim for the core problem described in `docs/adr/0004-hot-config-slots.md`.
//!
//! `agent-core` today gives a [`Model`](agent_core::Model) no turn identity: [`ModelContext`]
//! has only `cancellation` and `events`, and [`HookContext`](agent_core::HookContext) has
//! only `cancellation` and `model_calls`. A slot shared by several sessions therefore cannot
//! tell which turn a `complete` belongs to, so it can only read the route at each call. A
//! turn with a tool loop makes several model calls, and the next call sees a swap that
//! happened mid-turn.
//!
//! This module shows the pinning algorithm keyed by the turn identity the ADR asks Core to
//! add. The identity is carried here by a task-local scope the host establishes around the
//! turn, standing in for `ModelContext.run`; wiring Core's field in would replace the
//! task-local, not this logic.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};

use agent_core::{Model, ModelContext, ModelError, ModelRequest, ModelResponse};

use crate::config::{ConfigChange, ConfigChanged, ConfigLog};
use crate::model_slot::DynModel;

tokio::task_local! {
    /// The turn (run) the current model calls belong to.
    static RUN_ID: u64;
}

/// Runs `future` with `run_id` as the current turn identity, as `ModelContext.run` would.
pub async fn with_run<F: Future>(run_id: u64, future: F) -> F::Output {
    RUN_ID.scope(run_id, future).await
}

/// Like [`crate::ModelSlot`], but pins the adapter chosen at a turn's first model call for
/// every later call of the same turn. The run identity comes from [`with_run`].
pub struct PinnedModelSlot {
    current: RwLock<Arc<dyn DynModel>>,
    pins: Mutex<HashMap<u64, Arc<dyn DynModel>>>,
    log: Arc<ConfigLog>,
}

impl PinnedModelSlot {
    pub fn with_config_log<M: Model + 'static>(model: M, log: Arc<ConfigLog>) -> Self {
        Self {
            current: RwLock::new(Arc::new(model)),
            pins: Mutex::new(HashMap::new()),
            log,
        }
    }

    /// The adapter a request starting now would use if it did not belong to a pinned turn.
    pub fn current(&self) -> Arc<dyn DynModel> {
        Arc::clone(&self.current.read().unwrap())
    }

    pub fn config_log(&self) -> &Arc<ConfigLog> {
        &self.log
    }

    /// Replaces the adapter. Turns that already pinned keep their pinned adapter; later
    /// turns pin the new one.
    pub fn swap<M: Model + 'static>(&self, route: impl Into<String>, model: M) -> ConfigChanged {
        *self.current.write().unwrap() = Arc::new(model);
        self.log.record(ConfigChange::ModelRoute {
            route: route.into(),
        })
    }

    /// Forgets a finished turn's pin so the map does not grow. The host knows when a turn
    /// ends because it awaited `run_turn`/`run`.
    pub fn forget_run(&self, run_id: u64) {
        self.pins.lock().unwrap().remove(&run_id);
    }

    fn pinned(&self, run_id: u64) -> Arc<dyn DynModel> {
        let mut pins = self.pins.lock().unwrap();
        Arc::clone(pins.entry(run_id).or_insert_with(|| self.current()))
    }
}

impl Model for PinnedModelSlot {
    fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> impl Future<Output = Result<ModelResponse, ModelError>> + Send {
        let model = match RUN_ID.try_with(|id| *id) {
            Ok(run_id) => self.pinned(run_id),
            // No run scope: behave like the unpinned slot.
            Err(_) => self.current(),
        };
        async move { model.complete_boxed(request, cx).await }
    }
}

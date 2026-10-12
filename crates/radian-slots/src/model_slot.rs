//! A swappable, type-erased model adapter.

use std::future::Future;
use std::sync::{Arc, RwLock};

use agent_core::{Model, ModelContext, ModelError, ModelRequest, ModelResponse};
use futures_util::future::BoxFuture;

use crate::config::{ConfigChange, ConfigChanged, ConfigLog};

/// The object-safe form of [`Model`].
///
/// [`Model::complete`] returns `impl Future`, so `Model` itself is not dyn-compatible. This
/// trait boxes that future with the context's lifetime: a `'static` box would not compile,
/// because the future borrows `events` out of `ModelContext<'a>`.
pub trait DynModel: Send + Sync {
    fn complete_boxed<'a>(
        &'a self,
        request: ModelRequest,
        cx: ModelContext<'a>,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>>;
}

impl<T: Model> DynModel for T {
    fn complete_boxed<'a>(
        &'a self,
        request: ModelRequest,
        cx: ModelContext<'a>,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(self.complete(request, cx))
    }
}

/// A model adapter that a host can replace at run time.
///
/// Wrap it in `Arc` and use it as the `M` of `Agent<M, P, H>` (`impl Model for Arc<T>`
/// forwards through the `Arc`), so a clone of the `Arc` can [`ModelSlot::swap`] while
/// sessions keep running. `ModelSlot` also implements `Model` by value, so
/// `Agent<ModelSlot, P, H>` works without the `Arc`.
pub struct ModelSlot {
    current: RwLock<Arc<dyn DynModel>>,
    log: Arc<ConfigLog>,
}

impl ModelSlot {
    /// A slot with its own private [`ConfigLog`].
    pub fn new<M: Model + 'static>(model: M) -> Self {
        Self::with_config_log(model, Arc::new(ConfigLog::new()))
    }

    /// A slot that records its changes into a shared log, so revisions are monotonic across
    /// several slots.
    pub fn with_config_log<M: Model + 'static>(model: M, log: Arc<ConfigLog>) -> Self {
        Self {
            current: RwLock::new(Arc::new(model)),
            log,
        }
    }

    /// The adapter a request starting now would use. An in-flight request already holds its
    /// own clone and is not affected by a later swap.
    pub fn current(&self) -> Arc<dyn DynModel> {
        Arc::clone(&self.current.read().unwrap())
    }

    pub fn config_log(&self) -> &Arc<ConfigLog> {
        &self.log
    }

    /// Replaces the adapter and records one [`ConfigChanged`]. Takes `&self`, so the swap is
    /// available through a shared `Arc` and needs no `&mut`.
    pub fn swap<M: Model + 'static>(&self, route: impl Into<String>, model: M) -> ConfigChanged {
        *self.current.write().unwrap() = Arc::new(model);
        self.log.record(ConfigChange::ModelRoute {
            route: route.into(),
        })
    }
}

impl Model for ModelSlot {
    fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> impl Future<Output = Result<ModelResponse, ModelError>> + Send {
        let model = self.current();
        async move { model.complete_boxed(request, cx).await }
    }
}

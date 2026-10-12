//! A swappable, type-erased tool-call policy.

use std::future::Future;
use std::sync::{Arc, RwLock};

use agent_core::{HookContext, HookError, Hooks, ToolCall, ToolDecision};
use futures_util::future::BoxFuture;

use crate::config::{ConfigChange, ConfigChanged, ConfigLog};

/// The object-safe form of the `before_tool_call` policy point.
pub trait DynPolicy: Send + Sync {
    fn decide<'a>(
        &'a self,
        cx: &'a HookContext,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Result<ToolDecision, HookError>>;
}

impl<T: Policy> DynPolicy for T {
    fn decide<'a>(
        &'a self,
        cx: &'a HookContext,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Result<ToolDecision, HookError>> {
        Box::pin(self.before_tool_call(cx, call))
    }
}

/// One tool-call policy: the allow/deny/rewrite decision for a single call.
pub trait Policy: Send + Sync {
    fn before_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send;
}

/// Allows every call.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Policy for AllowAll {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        Ok(ToolDecision::Allow)
    }
}

/// Denies every call with `reason`.
#[derive(Debug, Clone, Default)]
pub struct DenyAll {
    pub reason: String,
}

impl DenyAll {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Policy for DenyAll {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        Ok(ToolDecision::Deny {
            reason: self.reason.clone(),
        })
    }
}

/// A tool-call policy that a host can replace at run time. Wrap it in `Arc` and use it as
/// the `H` of `Agent<M, P, H>` (`impl Hooks for Arc<T>` forwards), exactly like
/// [`crate::ModelSlot`]. Only `before_tool_call` is delegated; every other hook point keeps
/// the trait's no-op default.
pub struct PolicySlot {
    current: RwLock<Arc<dyn DynPolicy>>,
    log: Arc<ConfigLog>,
}

impl PolicySlot {
    pub fn new<P: Policy + 'static>(policy: P) -> Self {
        Self::with_config_log(policy, Arc::new(ConfigLog::new()))
    }

    pub fn with_config_log<P: Policy + 'static>(policy: P, log: Arc<ConfigLog>) -> Self {
        Self {
            current: RwLock::new(Arc::new(policy)),
            log,
        }
    }

    pub fn current(&self) -> Arc<dyn DynPolicy> {
        Arc::clone(&self.current.read().unwrap())
    }

    pub fn config_log(&self) -> &Arc<ConfigLog> {
        &self.log
    }

    /// Replaces the policy and records one [`ConfigChanged`]. Takes `&self`; a decision
    /// already in flight holds its own clone and is unaffected.
    pub fn swap<P: Policy + 'static>(&self, name: impl Into<String>, policy: P) -> ConfigChanged {
        self.swap_dyn(name, Arc::new(policy))
    }

    /// Erased form of [`PolicySlot::swap`], for hosts that already hold the policy as an
    /// `Arc<dyn DynPolicy>` (e.g. the driver's pending-change queue).
    pub fn swap_dyn(&self, name: impl Into<String>, policy: Arc<dyn DynPolicy>) -> ConfigChanged {
        *self.current.write().unwrap() = policy;
        self.log.record(ConfigChange::Policy { name: name.into() })
    }
}

impl Hooks for PolicySlot {
    fn before_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        // Clone the inputs so the returned future does not borrow through `self`; the slot
        // lock is released before any await.
        let policy = self.current();
        let cx = cx.clone();
        let call = call.clone();
        async move { policy.decide(&cx, &call).await }
    }
}

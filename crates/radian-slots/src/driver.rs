//! Per-session driver with boundary-applied configuration changes.
//!
//! This is the path ADR 0004 records as the owner's choice: instead of pinning a route per
//! turn inside a shared slot, every session gets **its own** [`ModelSlot`]/[`PolicySlot`]
//! and a driver that only applies queued changes **before** starting a turn. A change that
//! arrives mid-turn sits in the queue; the running turn keeps its current configuration, and
//! the change takes effect at the next `start_turn` boundary. "Next turn effective" is then
//! guaranteed by the driver's loop structure, not by a Core field and not by a task-local
//! discipline that can silently degrade.
//!
//! The queue is thread-safe, so a host (or another task) can call
//! [`DriverSlots::queue_change`] any time. Internally the driver holds one `Agent` per
//! session (one per driver), which is the precondition of this path: the route is fixed per
//! session, so the problem "one shared slot, several sessions, swap mid-turn" cannot arise.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent_core::*;

use crate::config::ConfigLog;
use crate::model_slot::{DynModel, ModelSlot};
use crate::policy_slot::{AllowAll, DynPolicy, Policy, PolicySlot};

/// One pending configuration change. It carries the already-erased adapter, so the queue is
/// `Send + Sync` and hosts can push from any task without keeping concrete types around.
#[derive(Clone)]
pub enum Change {
    Model {
        route: String,
        model: Arc<dyn DynModel>,
    },
    Policy {
        name: String,
        policy: Arc<dyn DynPolicy>,
    },
}

impl Change {
    pub fn model(route: impl Into<String>, model: impl Model + 'static) -> Self {
        Self::Model {
            route: route.into(),
            model: Arc::new(model),
        }
    }

    pub fn policy(name: impl Into<String>, policy: impl Policy + 'static) -> Self {
        Self::Policy {
            name: name.into(),
            policy: Arc::new(policy),
        }
    }
}

/// One session's configuration: its own model and policy slots, its pending-change queue,
/// and the log it records into. Cheap to clone (everything is `Arc`); the heavy part, the
/// `Agent`/session, lives in [`SessionDriver`].
#[derive(Clone)]
pub struct DriverSlots {
    pub model: Arc<ModelSlot>,
    pub policy: Arc<PolicySlot>,
    pending: Arc<Mutex<VecDeque<Change>>>,
    log: Arc<ConfigLog>,
}

impl DriverSlots {
    pub fn new(model: impl Model + 'static) -> Self {
        Self::with_config_log(model, Arc::new(ConfigLog::new()))
    }

    pub fn with_config_log(model: impl Model + 'static, log: Arc<ConfigLog>) -> Self {
        Self {
            model: Arc::new(ModelSlot::with_config_log(model, Arc::clone(&log))),
            policy: Arc::new(PolicySlot::with_config_log(AllowAll, Arc::clone(&log))),
            pending: Arc::default(),
            log,
        }
    }

    /// Queues `change` for the next turn boundary. Never applies it now: a turn already
    /// running must not see it.
    pub fn queue_change(&self, change: Change) {
        self.pending.lock().unwrap().push_back(change);
    }

    /// Changes waiting for the next boundary.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    pub fn config_log(&self) -> Arc<ConfigLog> {
        Arc::clone(&self.log)
    }

    /// The boundary application: drains the queue and applies every change to this session's
    /// own slots, recording each applied change in the log at this moment.
    pub(crate) fn apply_pending(&self) {
        let changes: Vec<Change> = self.pending.lock().unwrap().drain(..).collect();
        for change in changes {
            match change {
                Change::Model { route, model } => {
                    self.model.swap_dyn(route, model);
                }
                Change::Policy { name, policy } => {
                    self.policy.swap_dyn(name, policy);
                }
            }
        }
    }
}

/// Drives one session's turns. Owns that session's `Agent` (with this driver's slots) and
/// its `AgentSession`, so a driver is exactly one conversation. Turns run one at a time.
pub struct SessionDriver<P: ToolProvider> {
    slots: Arc<DriverSlots>,
    agent: Arc<Agent<Arc<ModelSlot>, P, Arc<PolicySlot>>>,
    session: Option<AgentSession<Arc<ModelSlot>, P, Arc<PolicySlot>>>,
}

impl<P: ToolProvider> SessionDriver<P> {
    pub fn new(model: impl Model + 'static, tools: P) -> Self {
        Self::from_slots(Arc::new(DriverSlots::new(model)), tools)
    }

    pub fn with_config_log(model: impl Model + 'static, tools: P, log: Arc<ConfigLog>) -> Self {
        Self::from_slots(Arc::new(DriverSlots::with_config_log(model, log)), tools)
    }

    /// A driver over pre-built slots. The slots' `Agent` is created here, so a driver always
    /// routes to its own slots and never to another session's.
    pub fn from_slots(slots: Arc<DriverSlots>, tools: P) -> Self {
        let agent = Arc::new(
            Agent::new(Arc::clone(&slots.model), tools).with_hooks(Arc::clone(&slots.policy)),
        );
        Self {
            slots,
            agent,
            session: None,
        }
    }

    /// A handle to the queue and slots, for queueing changes from other tasks while this
    /// driver's turn is running.
    pub fn slots(&self) -> Arc<DriverSlots> {
        Arc::clone(&self.slots)
    }

    pub fn queue_change(&self, change: Change) {
        self.slots.queue_change(change);
    }

    pub fn pending_len(&self) -> usize {
        self.slots.pending_len()
    }

    pub fn config_log(&self) -> Arc<ConfigLog> {
        self.slots.config_log()
    }

    /// One turn: applies queued changes at the boundary, then runs the turn to completion.
    /// The session is opened lazily on first use; it lives across turns so the transcript
    /// accumulates.
    pub async fn run_turn(
        &mut self,
        input: impl Into<Message>,
        events: &dyn EventSink,
    ) -> Result<RunOutcome, RunError> {
        self.slots.apply_pending();
        if self.session.is_none() {
            self.session = Some(self.agent.open_session().await?);
        }
        self.session
            .as_mut()
            .expect("session was just opened")
            .run_turn(input, TurnOptions::default(), events)
            .await
    }
}

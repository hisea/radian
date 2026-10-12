//! Durable, serializable records of hot configuration changes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// What changed. Deliberately small and free of secrets: it names a route or a policy, it
/// never carries credentials. `#[non_exhaustive]` leaves room for later kinds without a
/// breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum ConfigChange {
    /// The model adapter in use changed. `route` is a host-chosen label, never a key.
    ModelRoute { route: String },
    /// The tool-call policy changed. `name` is a host-chosen label.
    Policy { name: String },
}

/// One durable configuration change. `revision` is strictly increasing within one
/// [`ConfigLog`], so a replay can order changes without trusting timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChanged {
    pub revision: u64,
    pub change: ConfigChange,
}

/// The single source of revisions for a runtime's slots. Cloning shares the log, so every
/// slot that writes to it produces one monotonic revision sequence.
#[derive(Debug, Default)]
pub struct ConfigLog {
    next_revision: AtomicU64,
    records: Mutex<Vec<ConfigChanged>>,
}

impl ConfigLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assigns the next revision (starting at 1) and records `change`.
    pub fn record(&self, change: ConfigChange) -> ConfigChanged {
        let revision = self.next_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let record = ConfigChanged { revision, change };
        self.records.lock().unwrap().push(record.clone());
        record
    }

    /// Every record in revision order.
    pub fn records(&self) -> Vec<ConfigChanged> {
        self.records.lock().unwrap().clone()
    }
}

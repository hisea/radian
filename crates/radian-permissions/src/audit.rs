//! The audit record for one decision, shaped after ADR 0006 §5.9.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Who made the decision: a rule/policy path, or a human answering an approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approver {
    User,
    Policy,
}

/// The decision in the §5.9 shape. `rewrite` exists for a hook that rewrites arguments; this
/// layer never produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedDecision {
    Allow,
    Deny,
    Rewrite,
}

/// One durable `PermissionResolved` (ADR 0006 §5.9): audit only, never replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionResolved {
    pub request_id: String,
    pub call_id: String,
    pub decision: ResolvedDecision,
    pub approver: Approver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where decisions go. The host wires this to the session journal so a durable
/// `PermissionResolved` is written (ADR 0012 P6).
pub trait AuditSink: Send + Sync {
    fn record(&self, event: &PermissionResolved);
}

/// Discards decisions.
#[derive(Debug, Default)]
pub struct NoopAudit;

impl AuditSink for NoopAudit {
    fn record(&self, _event: &PermissionResolved) {}
}

/// Keeps decisions in memory; for tests and for hosts that batch them.
#[derive(Debug, Default)]
pub struct RecordingAudit(Mutex<Vec<PermissionResolved>>);

impl RecordingAudit {
    pub fn events(&self) -> Vec<PermissionResolved> {
        self.0.lock().unwrap().clone()
    }
}

impl AuditSink for RecordingAudit {
    fn record(&self, event: &PermissionResolved) {
        self.0.lock().unwrap().push(event.clone());
    }
}

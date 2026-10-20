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

/// The decision in the §5.9 shape. `rewrite` is produced by the built-in read-only
/// git table (`git diff`/`log`/`show` gain `--no-ext-diff --no-textconv`).
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
    /// The named policy command that decided, when `approver = "policy"` (ADR 0006 §5.9).
    /// **No producer since ADR 0043 decision 4** (the edge-policy layer went with the
    /// orchestration): it is always `None` now and is kept so old audit records and the field's
    /// reader do not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

/// Where decisions go. The host wires this to the session journal so a durable
/// `PermissionResolved` is written (ADR 0012 P6).
pub trait AuditSink: Send + Sync {
    fn record(&self, event: &PermissionResolved);

    /// The approval policy was switched in place (ADR 0026 D9's in-session `auto`).
    /// Written by [`crate::PermissionHooks::set_approval`]; the host journals it as an
    /// `ApprovalPolicyChanged` audit event.
    fn record_policy_change(&self, policy: crate::profile::ApprovalPolicy);
}

/// Discards decisions.
#[derive(Debug, Default)]
pub struct NoopAudit;

impl AuditSink for NoopAudit {
    fn record(&self, _event: &PermissionResolved) {}
    fn record_policy_change(&self, _policy: crate::profile::ApprovalPolicy) {}
}

/// Keeps decisions in memory; for tests and for hosts that batch them.
#[derive(Debug, Default)]
pub struct RecordingAudit(
    Mutex<Vec<PermissionResolved>>,
    Mutex<Vec<crate::profile::ApprovalPolicy>>,
);

impl RecordingAudit {
    pub fn events(&self) -> Vec<PermissionResolved> {
        self.0.lock().unwrap().clone()
    }

    /// The policy switches recorded so far, in order.
    pub fn policy_changes(&self) -> Vec<crate::profile::ApprovalPolicy> {
        self.1.lock().unwrap().clone()
    }
}

impl AuditSink for RecordingAudit {
    fn record(&self, event: &PermissionResolved) {
        self.0.lock().unwrap().push(event.clone());
    }

    fn record_policy_change(&self, policy: crate::profile::ApprovalPolicy) {
        self.1.lock().unwrap().push(policy);
    }
}

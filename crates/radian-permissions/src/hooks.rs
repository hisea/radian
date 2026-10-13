//! The permission layer as an `agent_core::Hooks` implementation.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{HookContext, HookError, Hooks, InputSource, ToolCall, ToolDecision};

use crate::approval::{ApprovalOutcome, ApprovalRequest, Approver};
use crate::audit::{Approver as AuditApprover, AuditSink, PermissionResolved, ResolvedDecision};
use crate::profile::{ApprovalPolicy, Profile};
use crate::rule::{Action, Resource, evaluate, resolve, resource_of};

/// The grant key: one tool plus one exact resource. Paths are keyed by their resolved form,
/// so a grant for `a/../b` also covers `b`.
type GrantKey = (String, String);

/// The permission layer. One instance per session: it holds that session's real-time grants,
/// which must not leak between sessions (ADR 0004 / 0006 §5.9).
pub struct PermissionHooks {
    profile: Profile,
    approver: Option<Arc<dyn Approver>>,
    audit: Arc<dyn AuditSink>,
    /// "Allow for this session", consulted on every call (ADR 0004: real-time state).
    grants: Mutex<HashSet<GrantKey>>,
    /// The current turn's source, set by the host before each turn. `HookContext` has no
    /// source field, so the host supplies it; `None` is treated as non-human (fail safe).
    source: Mutex<Option<InputSource>>,
    next_id: AtomicU64,
}

impl PermissionHooks {
    pub fn new(
        profile: Profile,
        approver: Option<Arc<dyn Approver>>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            profile,
            approver,
            audit,
            grants: Mutex::new(HashSet::new()),
            source: Mutex::new(None),
            next_id: AtomicU64::new(0),
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The host sets the source of the turn it is about to start. Until Core exposes the
    /// turn source in `HookContext`, this is a host obligation; an unset source is treated
    /// as non-human, so forgetting it fails closed rather than granting user rights.
    pub fn set_turn_source(&self, source: Option<InputSource>) {
        *self.source.lock().unwrap() = source;
    }

    pub fn turn_source(&self) -> Option<InputSource> {
        *self.source.lock().unwrap()
    }

    /// Reports the source of an input that was just **admitted** into the current turn — the
    /// input that started it, and every `steer`/`inject` admitted while it runs.
    ///
    /// The turn's effective source only ever **lowers** (ADR 0012 decision 5): a non-human
    /// input admitted mid-turn takes the turn's rights down to non-human, and a later `User`
    /// input cannot raise it back. Use [`PermissionHooks::set_turn_source`] at the start of a
    /// turn to reset.
    ///
    /// An unset source ranks lowest (it is already treated as non-human), so observing a
    /// `User` input on a fresh instance does **not** raise it: a host that forgets the reset
    /// fails closed.
    pub fn observe_input_source(&self, source: InputSource) {
        let mut current = self.source.lock().unwrap();
        if rank(Some(source)) < rank(*current) {
            *current = Some(source);
        }
    }

    /// Grants "allow for this session" for one tool and resource. The next matching call is
    /// allowed without asking; the grant is in memory only and dies with this instance.
    pub fn grant_for_session(&self, tool: impl Into<String>, resource: impl Into<String>) {
        self.grants
            .lock()
            .unwrap()
            .insert((tool.into(), resource.into()));
    }

    /// Every decision: rules first, then a session grant or the approver for `ask`.
    async fn decide(&self, cx: &HookContext, call: &ToolCall) -> Result<ToolDecision, HookError> {
        let resource = resource_of(call);
        let evaluated = evaluate(self.profile.rules(), &self.profile.workspace, call);
        let action = self.apply_policy(evaluated.action);
        let request_id = format!("perm-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let call_id = call.id.clone();

        if action == Action::Deny {
            let reason = evaluated
                .reason
                .clone()
                .unwrap_or_else(|| "denied by policy".into());
            self.record(
                &request_id,
                &call_id,
                ResolvedDecision::Deny,
                AuditApprover::Policy,
                Some(reason.clone()),
            );
            return Ok(ToolDecision::Deny { reason });
        }
        if action == Action::Allow {
            self.record(
                &request_id,
                &call_id,
                ResolvedDecision::Allow,
                AuditApprover::Policy,
                evaluated.reason.clone(),
            );
            return Ok(ToolDecision::Allow);
        }

        // `ask`: a human-answered grant can satisfy it, but only for a human turn.
        let human = self.turn_source() == Some(InputSource::User);
        let key = (
            call.name.clone(),
            resource_key(&resource, &self.profile.workspace),
        );
        let granted = human && self.grants.lock().unwrap().contains(&key);
        if granted {
            self.record(
                &request_id,
                &call_id,
                ResolvedDecision::Allow,
                AuditApprover::Policy,
                Some("allowed for this session".into()),
            );
            return Ok(ToolDecision::Allow);
        }
        if !human {
            let reason = if self.turn_source().is_none() {
                "no turn source is set, so this is treated as a non-human turn; deny".to_string()
            } else {
                "this turn is not from the user, so no approval can be obtained".to_string()
            };
            self.record(
                &request_id,
                &call_id,
                ResolvedDecision::Deny,
                AuditApprover::Policy,
                Some(reason.clone()),
            );
            return Ok(ToolDecision::Deny { reason });
        }
        let Some(approver) = &self.approver else {
            let reason = "no approver is configured, so the call cannot be approved".to_string();
            self.record(
                &request_id,
                &call_id,
                ResolvedDecision::Deny,
                AuditApprover::Policy,
                Some(reason.clone()),
            );
            return Ok(ToolDecision::Deny { reason });
        };

        let request = ApprovalRequest {
            request_id: request_id.clone(),
            call_id: call_id.clone(),
            tool: call.name.clone(),
            resource: resource.display().to_string(),
            // What the rules matched: for a path, the resolved one (the same key the session
            // grant uses), so the prompt cannot be made to look harmless with `..`.
            resolved_resource: resource_key(&resource, &self.profile.workspace),
            arguments: call.arguments.clone(),
            reason: evaluated.reason.clone(),
        };
        let outcome = tokio::select! {
            biased;
            _ = cx.cancellation.cancelled() => {
                let reason = "the approval request was cancelled".to_string();
                self.record(&request_id, &call_id, ResolvedDecision::Deny, AuditApprover::Policy, Some(reason.clone()));
                return Ok(ToolDecision::Deny { reason });
            }
            result = approver.decide(request) => match result {
                Ok(outcome) => outcome,
                Err(e) => {
                    // An internal failure is a denial, and it is recorded before the error
                    // reaches Core (which also fails closed on `Err`).
                    let reason = format!("the approver failed: {e}");
                    self.record(
                        &request_id,
                        &call_id,
                        ResolvedDecision::Deny,
                        AuditApprover::Policy,
                        Some(reason.clone()),
                    );
                    return Err(HookError::new(reason));
                }
            },
        };
        match outcome {
            ApprovalOutcome::AllowOnce => {
                self.record(
                    &request_id,
                    &call_id,
                    ResolvedDecision::Allow,
                    AuditApprover::User,
                    None,
                );
                Ok(ToolDecision::Allow)
            }
            ApprovalOutcome::AllowForSession => {
                self.grants.lock().unwrap().insert(key);
                self.record(
                    &request_id,
                    &call_id,
                    ResolvedDecision::Allow,
                    AuditApprover::User,
                    Some("allowed for this session".into()),
                );
                Ok(ToolDecision::Allow)
            }
            ApprovalOutcome::Deny { reason } => {
                let reason = reason.unwrap_or_else(|| "denied by the user".into());
                self.record(
                    &request_id,
                    &call_id,
                    ResolvedDecision::Deny,
                    AuditApprover::User,
                    Some(reason.clone()),
                );
                Ok(ToolDecision::Deny { reason })
            }
        }
    }

    fn apply_policy(&self, action: Action) -> Action {
        match (self.profile.approval, action) {
            (ApprovalPolicy::Never, Action::Ask) => Action::Deny,
            (ApprovalPolicy::Always, Action::Allow) => Action::Ask,
            (_, other) => other,
        }
    }

    fn record(
        &self,
        request_id: &str,
        call_id: &str,
        decision: ResolvedDecision,
        approver: AuditApprover,
        reason: Option<String>,
    ) {
        self.audit.record(&PermissionResolved {
            request_id: request_id.to_owned(),
            call_id: call_id.to_owned(),
            decision,
            approver,
            reason,
        });
    }
}

impl Hooks for PermissionHooks {
    /// The whole permission layer is one hook. Any internal error (a failed approver, a
    /// poisoned lock) is an `Err`, which Core treats as a denial.
    fn before_tool_call(
        &self,
        cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        self.decide(cx, call)
    }
}

/// Permission rank of a turn source: `User` is the highest, unset is the lowest. Every
/// non-`User` source ranks the same, because the layer only ever asks "is this a human
/// turn?" — the deny path is identical for all of them, so a finer order would be a claim
/// with no consumer (and `InputSource` is `#[non_exhaustive]`, so a future variant lands on
/// the fail-safe side).
fn rank(source: Option<InputSource>) -> u8 {
    match source {
        None => 0,
        Some(InputSource::User) => 2,
        Some(_) => 1,
    }
}

/// The key a session grant is stored under. A path is keyed by its **resolved** form, so a
/// grant for `a/../b` also covers `b`; a command or name is keyed verbatim (a grant is for
/// that exact command line).
fn resource_key(resource: &Resource, workspace: &std::path::Path) -> String {
    match resource {
        Resource::Path(raw) => resolve(workspace, raw).to_string_lossy().into_owned(),
        Resource::Command(c) | Resource::Name(c) => c.clone(),
    }
}

/// A convenience so a host can turn a decision into a durable runtime event.
pub fn runtime_event(event: &PermissionResolved) -> radian_session::RuntimeEvent {
    use radian_session::{PermissionApprover, PermissionDecision};
    radian_session::RuntimeEvent::permission_resolved(
        event.request_id.clone(),
        event.call_id.clone(),
        match event.decision {
            ResolvedDecision::Allow => PermissionDecision::Allow,
            ResolvedDecision::Deny => PermissionDecision::Deny,
            ResolvedDecision::Rewrite => PermissionDecision::Rewrite,
        },
        match event.approver {
            AuditApprover::User => PermissionApprover::User,
            AuditApprover::Policy => PermissionApprover::Policy,
        },
        event.reason.clone(),
    )
}

//! The permission layer as an `agent_core::Hooks` implementation.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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
    /// The live profile. Seeded at construction; [`PermissionHooks::set_profile`] swaps it
    /// when `/access` changes the tier (ADR 0038 slice 2). The rules and the workspace are
    /// read from it on every call, so a switch takes effect from the very next call.
    profile: RwLock<Profile>,
    /// The live approval policy. Seeded from `profile.approval`; [`PermissionHooks::
    /// set_approval`] switches it in place (ADR 0026 D9's in-session `auto`), so the
    /// profile's copy is only the starting value.
    approval: RwLock<ApprovalPolicy>,
    approver: Option<Arc<dyn Approver>>,
    /// `true` for a task agent (ADR 0030 slice 5): its asks are answered by the task's own
    /// `Approver` (the three-layer resolver), not by a human. This is the **only** thing that
    /// lets a non-human turn reach the approver; every other non-human source is still denied
    /// before it (ADR 0012). A task can never borrow a human session grant, so this widens
    /// nothing beyond the task's own resolver.
    task_approver: AtomicBool,
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
        let approval = profile.approval;
        Self {
            profile: RwLock::new(profile),
            approval: RwLock::new(approval),
            approver,
            task_approver: AtomicBool::new(false),
            audit,
            grants: Mutex::new(HashSet::new()),
            source: Mutex::new(None),
            next_id: AtomicU64::new(0),
        }
    }

    pub fn profile(&self) -> Profile {
        self.profile
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Switches the session's tier in place (ADR 0038 slice 2's `/access`): the rule set and
    /// the workspace the next call is evaluated against. The grants, the approval policy and
    /// the sandbox are untouched here — the sandbox is the host's `SandboxSlot` — and the
    /// switch takes effect from the very next call. The host records the durable
    /// `ConfigChanged` separately; the hook itself writes no audit line for a tier change.
    pub fn set_profile(&self, profile: Profile) {
        *self.profile.write().unwrap_or_else(|e| e.into_inner()) = profile;
    }

    /// The approval policy `apply_policy` currently applies.
    pub fn approval(&self) -> ApprovalPolicy {
        *self.approval.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Switches the session's approval policy in place (ADR 0026 D9's in-session `auto`).
    /// The rules, the grants and the sandbox are untouched; only how an `ask` resolves
    /// changes, from the very next call. Writes one audit record per call: the switch is
    /// itself a permission fact the journal must show.
    pub fn set_approval(&self, approval: ApprovalPolicy) {
        *self.approval.write().unwrap_or_else(|e| e.into_inner()) = approval;
        self.audit.record_policy_change(approval);
    }

    /// The host sets the source of the turn it is about to start. Until Core exposes the
    /// turn source in `HookContext`, this is a host obligation; an unset source is treated
    /// as non-human, so forgetting it fails closed rather than granting user rights.
    pub fn set_turn_source(&self, source: Option<InputSource>) {
        *self.source.lock().unwrap() = source;
    }

    /// Marks this instance as a **task agent** (ADR 0030 slice 5): a non-human `ask` is then
    /// handed to the task's own `Approver` (the auto / edge-policy / pending-queue resolver)
    /// instead of being denied outright. It is set by `open_session` only for the task routing;
    /// every other session keeps the fail-closed non-human denial. It never lets a task borrow a
    /// human session grant and never lets the approver answer as a human.
    pub fn set_task_approver(&self, task_approver: bool) {
        self.task_approver.store(task_approver, Ordering::SeqCst);
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
        // The profile is read once, before any `await`: the guard must not be held across the
        // approver call (a `std` read guard is not `Send`, and the hook's future is). A
        // `/access` swap between this read and the approver is not visible to this call,
        // which is exactly the "next call takes effect" rule.
        let (evaluated, workspace) = {
            let profile = self.profile.read().unwrap_or_else(|e| e.into_inner());
            (
                evaluate(profile.rules(), &profile.workspace, call),
                profile.workspace.clone(),
            )
        };
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
        let key = (call.name.clone(), resource_key(&resource, &workspace));
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
        if !human && !self.task_approver.load(Ordering::SeqCst) {
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
            resolved_resource: resource_key(&resource, &workspace),
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
            ApprovalOutcome::Policy {
                allow,
                name,
                reason,
            } => {
                // ADR 0030 R1 layer 2 / ADR 0006 §5.9: a policy-command decision is
                // audited as `approver = "policy"` with the policy's own name. It is not a human
                // answer, so it can never mint a session grant.
                let decision = if allow {
                    ResolvedDecision::Allow
                } else {
                    ResolvedDecision::Deny
                };
                self.record_named(
                    &request_id,
                    &call_id,
                    decision,
                    AuditApprover::Policy,
                    reason.clone(),
                    Some(name.clone()),
                );
                if allow {
                    Ok(ToolDecision::Allow)
                } else {
                    Ok(ToolDecision::Deny {
                        reason: reason
                            .unwrap_or_else(|| format!("denied by the edge policy `{name}`")),
                    })
                }
            }
        }
    }

    fn apply_policy(&self, action: Action) -> Action {
        match (self.approval(), action) {
            (ApprovalPolicy::Never, Action::Ask) => Action::Deny,
            // ADR 0026 D9: auto answers every `ask` with "allow". Deny rules and the
            // sandbox are untouched; only the question is removed.
            (ApprovalPolicy::Auto, Action::Ask) => Action::Allow,
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
        self.record_named(request_id, call_id, decision, approver, reason, None);
    }

    /// [`PermissionHooks::record`] with an explicit policy name (ADR 0030 R1 layer 2):
    /// set only when `approver = policy` and an edge policy command decided, so the audit names
    /// which policy (ADR 0006 §5.9). Built-in rules, `auto` and a human leave it unset.
    fn record_named(
        &self,
        request_id: &str,
        call_id: &str,
        decision: ResolvedDecision,
        approver: AuditApprover,
        reason: Option<String>,
        policy: Option<String>,
    ) {
        self.audit.record(&PermissionResolved {
            request_id: request_id.to_owned(),
            call_id: call_id.to_owned(),
            decision,
            approver,
            reason,
            policy,
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
        event.policy.clone(),
    )
}

/// A convenience so a host can journal a policy switch (ADR 0026 D9) as the
/// `ApprovalPolicyChanged` audit event.
pub fn policy_change_event(policy: crate::profile::ApprovalPolicy) -> radian_session::RuntimeEvent {
    use radian_session::{ApprovalPolicyValue, RuntimeEvent};
    RuntimeEvent::ApprovalPolicyChanged {
        policy: match policy {
            crate::profile::ApprovalPolicy::OnAsk => ApprovalPolicyValue::OnAsk,
            crate::profile::ApprovalPolicy::Never => ApprovalPolicyValue::Never,
            crate::profile::ApprovalPolicy::Always => ApprovalPolicyValue::Always,
            crate::profile::ApprovalPolicy::Auto => ApprovalPolicyValue::Auto,
        },
    }
}

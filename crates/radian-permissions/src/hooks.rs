//! The permission layer as an `agent_core::Hooks` implementation.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use agent_core::{HookContext, HookError, Hooks, InputSource, ToolCall, ToolDecision};
use serde_json::Value;

use crate::approval::{ApprovalOutcome, ApprovalRequest, Approver};
use crate::audit::{Approver as AuditApprover, AuditSink, PermissionResolved, ResolvedDecision};
use crate::profile::{ApprovalPolicy, Preset, Profile};
use crate::readonly::{self, ReadOnlyHit};
use crate::rule::{Action, Evaluated, Resource, evaluate, resolve, resource_of};
use crate::shell;

/// The grant key: one tool plus one exact resource. Paths are keyed by their resolved form,
/// so a grant for `a/../b` also covers `b`.
type GrantKey = (String, String);

/// A host-installed gate on tool **names**, consulted before the rules. It returns `Some(reason)`
/// to deny a call outright (a host policy no rule, grant or approver can widen), or `None` to let
/// the ordinary path decide.
///
/// The host uses this to enforce a built-in tool allow-list: it denies the names a definition
/// or a session does not mount, and a fixed `allow` rule lets the rest run without a prompt.
/// It is a **deny-only** seam on purpose: a rule, a session grant or the approver can never
/// lift it, and it can never raise an action the rules did not already allow.
pub type ToolGate = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

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
    /// `true` for a task agent (a non-human session): its asks are answered by the task's own
    /// `Approver` (the three-layer resolver), not by a human. This is the **only** thing that
    /// lets a non-human turn reach the approver; every other non-human source is still denied
    /// before it (ADR 0012). A task can never borrow a human session grant, so this widens
    /// nothing beyond the task's own resolver.
    task_approver: AtomicBool,
    /// The host's name gate (the host tool gate): a deny-only policy consulted before the rules.
    /// `None` in every existing host.
    tool_gate: RwLock<Option<ToolGate>>,
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
            tool_gate: RwLock::new(None),
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

    /// Marks this instance as a **task agent** (a non-human session): a non-human `ask` is then
    /// handed to the task's own `Approver` (the auto / edge-policy / pending-queue resolver)
    /// instead of being denied outright. It is set by `open_session` only for the task routing;
    /// every other session keeps the fail-closed non-human denial. It never lets a task borrow a
    /// human session grant and never lets the approver answer as a human.
    pub fn set_task_approver(&self, task_approver: bool) {
        self.task_approver.store(task_approver, Ordering::SeqCst);
    }

    /// Installs the host's name gate (the host tool gate). Deny-only: a call whose tool name the
    /// gate refuses is denied before the rules run, and nothing else can lift it. `None` clears it.
    pub fn set_tool_gate(&self, gate: Option<ToolGate>) {
        *self.tool_gate.write().unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// The gate's verdict for one tool name, if a gate is installed.
    fn gate(&self, tool: &str) -> Option<String> {
        let gate = self.tool_gate.read().unwrap_or_else(|e| e.into_inner());
        gate.as_ref().and_then(|gate| gate(tool))
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

    /// Every decision: the host's name gate first, then rules, then a session grant or the
    /// approver for `ask`.
    async fn decide(&self, cx: &HookContext, call: &ToolCall) -> Result<ToolDecision, HookError> {
        let request_id = format!("perm-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let call_id = call.id.clone();
        // ADR 0043 decision 3: a call to a renamed tool's old name is judged under the **new**
        // name, so the alias can never bypass a deny on the new tool. The transcript still
        // holds the name the model sent; only this check, the record and the grant key use the
        // canonical one.
        let canonical = ToolCall {
            name: crate::rule::canonical_tool_name(&call.name).to_owned(),
            ..call.clone()
        };
        let call = &canonical;
        // The host's name gate is deny-only and outranks everything (the host tool gate): a rule,
        // a grant or the approver can never widen it.
        if let Some(reason) = self.gate(&call.name) {
            self.record(
                &request_id,
                &call_id,
                ResolvedDecision::Deny,
                AuditApprover::Policy,
                Some(reason.clone()),
            );
            return Ok(ToolDecision::Deny { reason });
        }
        let resource = resource_of(call);
        // The profile is read once, before any `await`: the guard must not be held across the
        // approver call (a `std` read guard is not `Send`, and the hook's future is). A
        // `/access` swap between this read and the approver is not visible to this call,
        // which is exactly the "next call takes effect" rule.
        let (evaluated, workspace, preset, rules) = {
            let profile = self.profile.read().unwrap_or_else(|e| e.into_inner());
            (
                evaluate(profile.rules(), &profile.workspace, call),
                profile.workspace.clone(),
                profile.preset,
                profile.rules().to_vec(),
            )
        };
        let mut evaluated = evaluated;
        let mut action = self.apply_policy(evaluated.action);

        // A rule denial wins before everything else (deny outranks ask and allow
        // under every approval policy).
        if evaluated.action == Action::Deny {
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
        // A compound shell command (`a && b`, `cd x && cargo test`) is allowed only
        // when **every** segment is allowed — by a user `allow` rule or by the
        // read-only table. A rule denial already won above and still runs first.
        // `split_compound` is `None` for anything outside the four separators
        // (`;` aside) or with a non-simple segment, which falls through to the
        // ordinary path below (`ask`). A simple command is one segment, so it
        // takes the same path, plus the wrapper and `cd` restrictions.
        if call.name == "shell" {
            if let Some(command) = call.arguments.get("command").and_then(|v| v.as_str()) {
                if let Some((segments, separators)) = shell::split_compound(command) {
                    match self.decide_segments(&rules, &workspace, preset, &segments, &separators) {
                        SegmentsOutcome::Allow { reason } => {
                            self.record(
                                &request_id,
                                &call_id,
                                ResolvedDecision::Allow,
                                AuditApprover::Policy,
                                Some(reason),
                            );
                            return Ok(ToolDecision::Allow);
                        }
                        SegmentsOutcome::Rewrite { command } => {
                            let mut arguments = call.arguments.clone();
                            arguments["command"] = Value::String(command);
                            self.record(
                                &request_id,
                                &call_id,
                                ResolvedDecision::Rewrite,
                                AuditApprover::Policy,
                                Some(
                                    "read-only git command (built-in table): added --no-ext-diff --no-textconv"
                                        .into(),
                                )
                            );
                            return Ok(ToolDecision::Rewrite { arguments });
                        }
                        SegmentsOutcome::Deny { reason } => {
                            self.record(
                                &request_id,
                                &call_id,
                                ResolvedDecision::Deny,
                                AuditApprover::Policy,
                                Some(reason.clone()),
                            );
                            return Ok(ToolDecision::Deny { reason });
                        }
                        // A segment needs a question: continue below as an `ask` for
                        // the whole command (policy, session grants and the approver
                        // all see the full command line, as before).
                        SegmentsOutcome::Ask { reason } => {
                            evaluated = Evaluated {
                                action: Action::Ask,
                                reason: Some(reason),
                            };
                            action = self.apply_policy(evaluated.action);
                        }
                    }
                }
            }
        }
        // The built-in read-only shell table: allow rules placed after the user's
        // rules and before the tier's blanket shell rule. A hit bypasses the
        // approval policy (`always`/`never`/`auto` never turn it into a question:
        // read-only commands are not worth reviewing, and a headless turn must
        // not lose them) and is audited as a policy decision. A named `ask` rule
        // is an explicit user rule and outranks the table; the tier's blanket
        // shell `ask` and the no-match default carry no reason, so they still
        // fall through to it.
        let table_hit = (evaluated.action != Action::Ask || evaluated.reason.is_none())
            .then(|| readonly::match_command(&workspace, call))
            .flatten();
        if let Some(hit) = table_hit {
            match hit {
                ReadOnlyHit::Allow => {
                    self.record(
                        &request_id,
                        &call_id,
                        ResolvedDecision::Allow,
                        AuditApprover::Policy,
                        Some("read-only shell command (built-in table)".into()),
                    );
                    return Ok(ToolDecision::Allow);
                }
                ReadOnlyHit::Rewrite { command } => {
                    let mut arguments = call.arguments.clone();
                    arguments["command"] = Value::String(command);
                    self.record(
                        &request_id,
                        &call_id,
                        ResolvedDecision::Rewrite,
                        AuditApprover::Policy,
                        Some(
                            "read-only git command (built-in table): added --no-ext-diff --no-textconv"
                                .into(),
                        ),
                    );
                    return Ok(ToolDecision::Rewrite { arguments });
                }
            }
        }

        // A policy denial (`never` turned an `ask` into one) denies here, after
        // the table had its chance to allow.
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
        }
    }

    /// One compound shell command, segment by segment. The command is allowed only
    /// when **every** segment is allowed — by a user `allow` rule or by the
    /// read-only table — and carries the effective directory through the segments:
    /// a `cd` target the earlier segments moved to is where every later path
    /// argument (the table's path checks) resolves. A rule denial for any segment
    /// denies the whole command (the whole-command check above normally wins first).
    /// Anything a segment cannot cover is one `Ask` for the whole command.
    fn decide_segments(
        &self,
        rules: &[crate::rule::Rule],
        workspace: &std::path::Path,
        preset: Preset,
        segments: &[String],
        separators: &[String],
    ) -> SegmentsOutcome {
        let mut effective = workspace.to_path_buf();
        let mut rewritten: Vec<Option<String>> = vec![None; segments.len()];
        let mut any_rule_allow = false;
        let mut blocked: Option<String> = None;
        let mut denied: Option<String> = None;
        for (n, segment) in segments.iter().enumerate() {
            if blocked.is_some() || denied.is_some() {
                break;
            }
            // A runs-its-arguments program only shows the wrapper's own tokens; no
            // prefix rule and no table entry can see what it will run.
            if shell::starts_with_wrapper(segment) {
                let wrapper = shell::tokens(segment).remove(0);
                blocked = Some(format!(
                    "`{segment}` starts with `{wrapper}`, which runs its arguments, \
                     so no allow rule covers it"
                ));
                continue;
            }
            let segment_call = ToolCall {
                id: "segment".into(),
                name: "shell".into(),
                arguments: serde_json::json!({ "command": segment }),
            };
            let judged = evaluate(rules, workspace, &segment_call);
            // `cd <target>` moves the effective directory the later segments
            // resolve against. It is allow-able only in the `workspace` tier, only
            // with a user `allow` rule, and only when the target stays inside the
            // workspace (in `full` git would run the target directory's hooks, and
            // there is no sandbox wall around that).
            if let Some(target) = shell::cd_target(segment) {
                match cd_dir(workspace, &effective, preset, target) {
                    Some(dir) if judged.action == Action::Allow => {
                        effective = dir;
                        any_rule_allow = true;
                    }
                    _ => {
                        blocked = Some(format!(
                            "`{segment}` leaves the workspace or has no allow rule, \
                             so the command needs a question"
                        ));
                    }
                }
                continue;
            }
            // A rule denial for any segment denies the whole command.
            if judged.action == Action::Deny {
                denied = Some(
                    judged
                        .reason
                        .clone()
                        .unwrap_or_else(|| "denied by policy".into()),
                );
                continue;
            }
            // A user `allow` rule covers the segment.
            if judged.action == Action::Allow {
                any_rule_allow = true;
                continue;
            }
            // A named `ask` rule is an explicit user rule and outranks the table;
            // the tier's blanket `ask` and the no-match default carry no reason,
            // so they still fall through to it.
            if judged.reason.is_some() {
                blocked = judged.reason.clone();
                continue;
            }
            match readonly::match_segment(workspace, &effective, segment) {
                Some(ReadOnlyHit::Allow) => {}
                Some(ReadOnlyHit::Rewrite { command }) => {
                    rewritten[n] = Some(command);
                }
                None => {
                    blocked = Some(format!(
                        "no allow rule or read-only table entry covers `{segment}`"
                    ));
                }
            }
        }
        if let Some(reason) = denied {
            return SegmentsOutcome::Deny { reason };
        }
        if let Some(reason) = blocked {
            return SegmentsOutcome::Ask { reason };
        }
        // A user-allowed segment is still reviewed under `approval = always`, the
        // way a single allowed command is; a purely read-only command (table
        // hits only) bypasses review, the way a single table hit does.
        if any_rule_allow && self.approval() == ApprovalPolicy::Always {
            return SegmentsOutcome::Ask {
                reason: "approval policy `always` reviews every non-read-only command".into(),
            };
        }
        if rewritten.iter().any(Option::is_some) {
            let mut command = String::new();
            for (n, segment) in segments.iter().enumerate() {
                if n > 0 {
                    command.push_str(&separators[n - 1]);
                }
                command.push_str(rewritten[n].as_deref().unwrap_or(segment));
            }
            return SegmentsOutcome::Rewrite { command };
        }
        SegmentsOutcome::Allow {
            reason: "every segment is allowed by an allow rule or the read-only table".into(),
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
        self.audit.record(&PermissionResolved {
            request_id: request_id.to_owned(),
            call_id: call_id.to_owned(),
            decision,
            approver,
            reason,
            // No producer since the policy approver went with the orchestration (ADR 0043
            // decision 4); the field stays so the journal shape and old journals are unchanged.
            policy: None,
        });
    }
}

/// What one compound shell command's segments decide.
enum SegmentsOutcome {
    /// Every segment is allowed by an allow rule or the read-only table.
    Allow { reason: String },
    /// Every segment is allowed and at least one table hit rewrote its segment;
    /// `command` is the whole command with each rewritten segment substituted
    /// and every separator as it was.
    Rewrite { command: String },
    /// A rule denial hit a segment.
    Deny { reason: String },
    /// A segment needs a question: the caller continues as an `ask` for the
    /// whole command.
    Ask { reason: String },
}

/// Where a plain `cd <target>` segment moves the effective directory: the target
/// resolved against the current effective directory, when it stays inside the
/// workspace and is an existing directory. `None` in the `workspace` tier means
/// the shell would leave the workspace (or nowhere predictable: `cd -`, options,
/// a missing directory), and in any other tier `cd` never moves anything — a `cd`
/// segment there always needs a question.
fn cd_dir(
    workspace: &std::path::Path,
    effective: &std::path::Path,
    preset: Preset,
    target: &str,
) -> Option<std::path::PathBuf> {
    if preset != Preset::WorkspaceWrite {
        return None;
    }
    if target.starts_with('-') {
        return None;
    }
    let resolved = resolve(effective, target);
    if !resolved.starts_with(workspace) || !resolved.is_dir() {
        return None;
    }
    Some(resolved)
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

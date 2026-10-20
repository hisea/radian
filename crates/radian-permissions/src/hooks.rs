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

/// A session grant: one tool plus one resource scope. Paths are keyed by their resolved
/// form, so a grant for `a/../b` also covers `b`; a shell prefix grant covers every
/// simple command with those leading tokens (`cargo test` covers `cargo test -p b`).
/// Grants are real-time state (ADR 0004): they live only in this instance, die with it,
/// and are never written into a profile, the journal, or any config file — an allow can
/// never come from the project (ADR 0012).
#[derive(Clone, PartialEq, Eq, Hash)]
enum SessionGrant {
    /// One exact resource: the full command line, the resolved path, or the tool name.
    Exact(GrantKey),
    /// A shell prefix (slice 2 of `sandbox-auto-allow-shell`): only ever a simple,
    /// single-segment, non-wrapper command outside the never-prefix list
    /// ([`shell::session_grant_prefix`]), plus whether it was earned leaving the
    /// sandbox — a grant never crosses that boundary.
    ShellPrefix { prefix: Vec<String>, outside: bool },
}

/// The exact-grant key: one tool plus one exact resource. Paths are keyed by their
/// resolved form, so a grant for `a/../b` also covers `b`.
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
    grants: Mutex<HashSet<SessionGrant>>,
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
    /// A `shell` grant for a prefix-shaped command widens to its prefix
    /// ([`shell::session_grant_prefix`]); anything else stays exact, as before.
    pub fn grant_for_session(&self, tool: impl Into<String>, resource: impl Into<String>) {
        let (tool, resource): (String, String) = (tool.into(), resource.into());
        let grant = if tool == "shell" {
            match shell::session_grant_prefix(&resource) {
                Some(prefix) => SessionGrant::ShellPrefix {
                    prefix,
                    outside: false,
                },
                None => SessionGrant::Exact((tool, resource)),
            }
        } else {
            SessionGrant::Exact((tool, resource))
        };
        self.grants.lock().unwrap().insert(grant);
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
        let (evaluated, workspace, preset, rules, blanket_allow) = {
            let profile = self.profile.read().unwrap_or_else(|e| e.into_inner());
            (
                evaluate(profile.rules(), &profile.workspace, call),
                profile.workspace.clone(),
                profile.preset,
                profile.rules().to_vec(),
                profile.shell_blanket_allow(),
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
        // Slice 1 of `sandbox-auto-allow-shell`: a shell call that asks to leave the
        // sandbox (`outside_sandbox: true` plus a required reason) always goes through
        // `ask` — no blanket `allow`, no compound-segment allow and no read-only-table
        // hit may answer it. A rule denial above still wins first, and a missing reason
        // is a denial, not a question (fail closed: the approver must see why). The
        // tool runs that one call unsandboxed only after this `ask` is approved; the
        // layer itself never re-runs anything outside the sandbox.
        let leaving_sandbox = outside_sandbox(call);
        if let Some(reason) = &leaving_sandbox {
            if reason.trim().is_empty() {
                let deny = "`outside_sandbox: true` needs an `outside_sandbox_reason`: \
                     leaving the sandbox without saying why is denied"
                    .to_string();
                self.record(
                    &request_id,
                    &call_id,
                    ResolvedDecision::Deny,
                    AuditApprover::Policy,
                    Some(deny.clone()),
                );
                return Ok(ToolDecision::Deny { reason: deny });
            }
            evaluated = Evaluated {
                action: Action::Ask,
                reason: Some(format!("outside the sandbox: {reason}")),
            };
            action = self.apply_policy(evaluated.action);
        }
        // A compound shell command (`a && b`, `cd x && cargo test`) is allowed only
        // when **every** segment is allowed — by a user `allow` rule, by the tier's
        // blanket `allow` (the `full` tier, or the `workspace` tier with a sandbox in
        // force: the sandbox, not the prompt, is the boundary there), or by the
        // read-only table. A rule denial already won above and still runs first.
        // `split_compound` is `None` for anything outside the four separators
        // (`;` aside) or with a non-simple segment, which falls through to the
        // ordinary path below (`ask`). A simple command is one segment, so it takes
        // the ordinary path too — except `cd` and runs-its-arguments programs,
        // which always need the segment check when there is no blanket `allow`
        // (a `cd` target must resolve inside the workspace; no prefix rule or table
        // entry may see what a wrapper runs). Where the tier's shell rule is a
        // blanket `allow`, the ordinary path already trusts a single command, so a
        // compound is judged like any other there too.
        if call.name == "shell"
            && let Some(command) = call.arguments.get("command").and_then(|v| v.as_str())
            && let Some((segments, separators)) = shell::split_compound(command)
            && (segments.len() > 1 || needs_single_segment_check(blanket_allow, command, &segments))
        {
            match self.decide_segments(
                &rules,
                &workspace,
                blanket_allow,
                preset,
                &segments,
                &separators,
            ) {
                // Leaving the sandbox stays an `ask` for the whole command (set above):
                // no segment allow or rewrite may answer it.
                SegmentsOutcome::Allow { .. } | SegmentsOutcome::Rewrite { .. }
                    if leaving_sandbox.is_some() => {}
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
                        reason,
                    };
                    action = self.apply_policy(evaluated.action);
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

        // `ask`: a human-answered grant can satisfy it, but only for a human turn. A
        // grant for a sandboxed command never covers leaving the sandbox (and vice
        // versa): the key carries the kind, so borrowing a narrower grant to leave is
        // impossible. A shell prefix grant covers a later command with the same prefix
        // ([`shell::session_grant_prefix`]) — never a compound, a wrapper, or a
        // never-prefix command, which stay exact.
        let human = self.turn_source() == Some(InputSource::User);
        let granted = human
            && session_grant_covers(
                &self.grants.lock().unwrap(),
                call,
                &resource,
                &workspace,
                leaving_sandbox.is_some(),
            );
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
                self.grants.lock().unwrap().insert(session_grant_for(
                    call,
                    &resource,
                    &workspace,
                    leaving_sandbox.is_some(),
                ));
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
    /// when **every** segment is allowed — by a user `allow` rule, by the tier's
    /// blanket `allow`, or by the read-only table — and carries the effective
    /// directory through the segments: a `cd` target the earlier segments moved to
    /// is where every later path argument (the table's path checks) resolves. A rule
    /// denial for any segment denies the whole command (the whole-command check
    /// above normally wins first). Anything a segment cannot cover is one `Ask` for
    /// the whole command.
    fn decide_segments(
        &self,
        rules: &[crate::rule::Rule],
        workspace: &std::path::Path,
        blanket_allow: bool,
        preset: Preset,
        segments: &[String],
        separators: &[String],
    ) -> SegmentsOutcome {
        let mut effective = workspace.to_path_buf();
        let mut rewritten: Vec<Option<String>> = vec![None; segments.len()];
        let mut any_rule_allow = false;
        let mut rule_reason: Option<String> = None;
        let mut blocked = false;
        let mut blocked_reason: Option<String> = None;
        let mut denied: Option<String> = None;
        for (n, segment) in segments.iter().enumerate() {
            if blocked || denied.is_some() {
                break;
            }
            // A runs-its-arguments program only shows the wrapper's own tokens; no
            // prefix rule and no table entry can see what it will run — without a
            // blanket `allow`, the segment needs a question. Where the tier's shell
            // rule is a blanket `allow` (`full`, or `workspace` with a sandbox in
            // force: the sandbox, not the prompt, is the boundary) the segment is
            // judged like any other below.
            if !blanket_allow && shell::starts_with_wrapper(segment) {
                blocked = true;
                continue;
            }
            let segment_call = ToolCall {
                id: "segment".into(),
                name: "shell".into(),
                arguments: serde_json::json!({ "command": segment }),
            };
            let judged = evaluate(rules, workspace, &segment_call);
            // `cd <target>` moves the effective directory the later segments
            // resolve against. Without a blanket `allow` it is allow-able only in
            // the `workspace` tier, only with a user `allow` rule, and only when the
            // target stays inside the workspace. Where the tier's shell rule is a
            // blanket `allow` the segment is judged like any other below (even a `cd`
            // leaving the workspace: the sandbox confines it).
            if !blanket_allow && let Some(target) = shell::cd_target(segment) {
                match cd_dir(workspace, &effective, preset, target) {
                    Some(dir) if judged.action == Action::Allow => {
                        effective = dir;
                        any_rule_allow = true;
                        if rule_reason.is_none() {
                            rule_reason = judged.reason.clone();
                        }
                    }
                    // A `cd` out of the workspace, with no allow rule, or in the
                    // wrong tier: the whole command needs a question, with the
                    // default (empty) reason, exactly as before.
                    _ => {
                        blocked = true;
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
            // An `allow` rule covers the segment — a named user rule, the tier's
            // blanket `allow`, or (below) the read-only table. Its reason is kept for
            // the `always` review below, the way a single allowed command shows the
            // rule's reason when it is turned into a question.
            if judged.action == Action::Allow {
                any_rule_allow = true;
                if rule_reason.is_none() {
                    rule_reason = judged.reason.clone();
                }
                continue;
            }
            // A named `ask` rule is an explicit user rule and outranks the table;
            // the tier's blanket `ask` and the no-match default carry no reason,
            // so they still fall through to it. The named reason reaches the
            // approver; a default block asks with the default (empty) reason.
            if judged.reason.is_some() {
                blocked = true;
                if blocked_reason.is_none() {
                    blocked_reason = judged.reason.clone();
                }
                continue;
            }
            match readonly::match_segment(workspace, &effective, segment) {
                Some(ReadOnlyHit::Allow) => {}
                Some(ReadOnlyHit::Rewrite { command }) => {
                    rewritten[n] = Some(command);
                }
                None => {
                    blocked = true;
                }
            }
        }
        if let Some(reason) = denied {
            return SegmentsOutcome::Deny { reason };
        }
        if blocked {
            return SegmentsOutcome::Ask {
                reason: blocked_reason,
            };
        }
        // A user-allowed segment is still reviewed under `approval = always`, the
        // way a single allowed command is; a purely read-only command (table
        // hits only) bypasses review, the way a single table hit does.
        if any_rule_allow && self.approval() == ApprovalPolicy::Always {
            return SegmentsOutcome::Ask {
                reason: rule_reason,
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
    /// whole command. `None` for a default block (the approver and the `never`
    /// denial then see exactly what they saw before compounds existed), or the
    /// named rule's reason.
    Ask { reason: Option<String> },
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

/// A shell call that asks to leave the sandbox (slice 1 of
/// `sandbox-auto-allow-shell`): `outside_sandbox: true` plus a required
/// `outside_sandbox_reason`. `None` for an ordinary sandboxed call (or any other tool);
/// `Some(reason)` — possibly empty, which the caller denies — when the flag is set.
fn outside_sandbox(call: &ToolCall) -> Option<String> {
    if call.name != "shell" {
        return None;
    }
    if call
        .arguments
        .get("outside_sandbox")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return None;
    }
    Some(
        call.arguments
            .get("outside_sandbox_reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    )
}

/// The session grant one `AllowForSession` answer earns: a shell prefix grant when the
/// granted command is prefix-shaped ([`shell::session_grant_prefix`]), otherwise the
/// exact key, as before.
fn session_grant_for(
    call: &ToolCall,
    resource: &Resource,
    workspace: &std::path::Path,
    leaving_sandbox: bool,
) -> SessionGrant {
    if call.name == "shell"
        && let Resource::Command(command) = resource
        && let Some(prefix) = shell::session_grant_prefix(command)
    {
        return SessionGrant::ShellPrefix {
            prefix,
            outside: leaving_sandbox,
        };
    }
    SessionGrant::Exact(grant_key(call, resource, workspace, leaving_sandbox))
}

/// Whether a session grant covers this call: the exact key, as before, or — for a
/// `shell` call — a stored prefix the current command re-derives. The current command
/// must itself be prefix-shaped, so a compound or wrapper never borrows a prefix, and
/// a grant earned leaving the sandbox never covers an inside call (or vice versa).
fn session_grant_covers(
    grants: &HashSet<SessionGrant>,
    call: &ToolCall,
    resource: &Resource,
    workspace: &std::path::Path,
    leaving_sandbox: bool,
) -> bool {
    if grants.contains(&SessionGrant::Exact(grant_key(
        call,
        resource,
        workspace,
        leaving_sandbox,
    ))) {
        return true;
    }
    if call.name != "shell" {
        return false;
    }
    let Resource::Command(command) = resource else {
        return false;
    };
    let Some(prefix) = shell::session_grant_prefix(command) else {
        return false;
    };
    grants.contains(&SessionGrant::ShellPrefix {
        prefix,
        outside: leaving_sandbox,
    })
}

/// The grant key for one call: [`resource_key`], plus the sandbox kind, so a grant for a
/// sandboxed command never covers leaving the sandbox with the same command line (and
/// vice versa). `AllowForSession` for an `outside_sandbox` call therefore covers only
/// later `outside_sandbox` calls with the same command.
fn grant_key(
    call: &ToolCall,
    resource: &Resource,
    workspace: &std::path::Path,
    leaving_sandbox: bool,
) -> GrantKey {
    let resource = resource_key(resource, workspace);
    if leaving_sandbox {
        (
            call.name.clone(),
            format!("{resource}\0outside the sandbox"),
        )
    } else {
        (call.name.clone(), resource)
    }
}
/// Whether a single-segment command still needs the segment check: `cd` (its target
/// must resolve inside the workspace for the allow to hold) and runs-its-arguments
/// programs (no prefix rule or table entry may see what they will run). Multi-segment
/// commands always need it; where the tier's shell rule is a blanket `allow` every
/// segment is judged like any other, so it never does. Without this, a lone
/// `cd ..` covered by a `cd` allow rule would be answered from the ordinary path
/// and never contained.
fn needs_single_segment_check(blanket_allow: bool, command: &str, segments: &[String]) -> bool {
    !blanket_allow
        && segments.len() == 1
        && (shell::cd_target(command).is_some() || shell::starts_with_wrapper(&segments[0]))
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

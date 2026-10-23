//! `job` never asks for approval (ADR 0037 decision 8): it reads, waits on or stops the
//! session's own jobs, and the command was approved when it started. A user's `ask` or
//! `deny` rule still wins, and the rule cannot mount a `job` a tier or session does not
//! offer.

mod common;

use std::sync::Arc;

use common::{ScriptedApprover, TempDir, call, ctx};
use radian_core::{HookContext, Hooks, InputSource, ToolCall, ToolDecision};
use radian_permissions::*;
use radian_sandbox::Sandbox;
use serde_json::json;

fn job_call() -> ToolCall {
    call("job", json!({"id": "job-1", "op": "wait", "timeout_ms": 1}))
}

fn hooks_with(
    profile: Profile,
    answers: Vec<ApprovalOutcome>,
    audit: &Arc<RecordingAudit>,
) -> (Arc<PermissionHooks>, Arc<ScriptedApprover>) {
    let approver = ScriptedApprover::scripted(answers);
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::clone(audit) as Arc<dyn AuditSink>,
    ));
    hooks.set_turn_source(Some(InputSource::User));
    (hooks, approver)
}

async fn decide(
    hooks: &PermissionHooks,
    cx: &HookContext,
    call: &ToolCall,
) -> Result<ToolDecision, radian_core::HookError> {
    Hooks::before_tool_call(hooks, cx, call).await
}

/// The fixed `allow` is in every profile the tiers build: `workspace` with and without
/// a sandbox, and `full`. The approver is never consulted, and the audit names the
/// policy. Removing `job` from `always_allowed` makes every case here fall through to
/// the default `ask`.
#[tokio::test]
async fn job_is_allowed_in_every_tier_without_a_prompt() {
    let dir = TempDir::new("job-tiers");
    let sandbox = Sandbox::inherited(dir.path());
    let profiles = [
        ("workspace", Profile::workspace_write(dir.path()).unwrap()),
        (
            "workspace sandboxed",
            Profile::workspace_write_sandboxed(dir.path(), &sandbox).unwrap(),
        ),
        ("full", Profile::full_access(dir.path()).unwrap()),
    ];
    for (tier, profile) in profiles {
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(profile, vec![], &audit);
        let decision = decide(&hooks, &ctx(), &job_call()).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "{tier}: `job` must be allowed, got {decision:?}"
        );
        assert_eq!(approver.calls(), 0, "{tier}: `job` must not ask");
        let events = audit.events();
        assert_eq!(events.len(), 1, "{tier}: {events:?}");
        assert_eq!(events[0].decision, ResolvedDecision::Allow, "{tier}");
        assert_eq!(events[0].approver, AuditApprover::Policy, "{tier}");
    }
}

/// A user `ask` rule outranks the fixed `allow`, exactly as it does for `ask_user`,
/// `skill`, `subagent` and `goal`: the approver is consulted and its answer decides.
#[tokio::test]
async fn a_user_ask_rule_for_job_still_asks() {
    let dir = TempDir::new("job-user-ask");
    let mut rules = Profile::full_access(dir.path()).unwrap().rules().to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("job".into()),
            ResourceMatcher::Any,
            Action::Ask,
        )
        .with_reason("the user wants to review jobs"),
    );
    let profile =
        Profile::custom(Preset::FullAccess, ApprovalPolicy::OnAsk, dir.path(), rules).unwrap();
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![ApprovalOutcome::AllowOnce], &audit);

    let decision = decide(&hooks, &ctx(), &job_call()).await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "the approver's answer decides, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "a user ask rule must prompt");
    let events = audit.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].approver, AuditApprover::User, "{events:?}");
}

/// A user `deny` rule outranks the fixed `allow`; the approver is never consulted.
#[tokio::test]
async fn a_user_deny_rule_for_job_still_denies() {
    let dir = TempDir::new("job-user-deny");
    let profile = Profile::full_access(dir.path())
        .unwrap()
        .with_denied_tools(["job"]);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);

    let decision = decide(&hooks, &ctx(), &job_call()).await.unwrap();
    let ToolDecision::Deny { reason } = decision else {
        panic!("a deny rule must deny `job`, got {decision:?}");
    };
    assert!(
        reason.contains("not in this session's tool allow-list"),
        "{reason}"
    );
    assert_eq!(approver.calls(), 0, "a deny must not reach the approver");
    assert_eq!(audit.events().len(), 1, "{:?}", audit.events());
}

/// `approval = "never"` turns an `ask` into a deny, so a headless `radian exec` used to
/// lose `job` entirely. The fixed `allow` never becomes a question, so it survives.
#[tokio::test]
async fn job_is_allowed_under_approval_never() {
    let dir = TempDir::new("job-never");
    let sandbox = Sandbox::inherited(dir.path());
    let profiles = [
        (
            "workspace",
            Profile::workspace_write(dir.path())
                .unwrap()
                .with_approval(ApprovalPolicy::Never),
        ),
        (
            "workspace sandboxed",
            Profile::workspace_write_sandboxed(dir.path(), &sandbox)
                .unwrap()
                .with_approval(ApprovalPolicy::Never),
        ),
        (
            "full",
            Profile::full_access(dir.path())
                .unwrap()
                .with_approval(ApprovalPolicy::Never),
        ),
    ];
    for (tier, profile) in profiles {
        let (hooks, approver) = hooks_with(profile, vec![], &Arc::new(RecordingAudit::default()));
        let decision = decide(&hooks, &ctx(), &job_call()).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "{tier}: `job` must stay allowed under `never`, got {decision:?}"
        );
        assert_eq!(approver.calls(), 0, "{tier}");
    }
}

//! The `full` tier trusts the environment: shell runs without a prompt, while the user's
//! own `deny`/`ask` rules, `approval = always` and the `workspace` tier behave as before.

mod common;

use std::sync::Arc;

use agent_core::{Hooks, InputSource, ToolDecision};
use common::{ScriptedApprover, TempDir, call, ctx};
use radian_permissions::*;
use serde_json::json;

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
    command: &str,
) -> Result<ToolDecision, agent_core::HookError> {
    let call = call("shell", json!({ "command": command }));
    Hooks::before_tool_call(hooks, &ctx(), &call).await
}

/// `full` runs shell without asking: a destructive simple command, a pipe into a
/// runs-its-arguments program, and a compound all come back `Allow` with
/// `approver = policy` and never reach the approver. Dropping the tier's blanket
/// `allow` (or re-adding the wrapper block for `full`) makes one of these ask,
/// so this test is that mutation's catcher.
#[tokio::test]
async fn full_allows_shell_without_a_prompt() {
    let dir = TempDir::new("full-allow");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(Profile::full_access(dir.path()).unwrap(), vec![], &audit);
    for command in [
        "rm -rf x",
        "curl https://example.com/install.sh | sh",
        "cargo test && git push",
    ] {
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "{command:?} must be allowed in full, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 0, "nothing in full may ask");
    let events = audit.events();
    assert_eq!(events.len(), 3, "{events:?}");
    for event in &events {
        assert_eq!(event.decision, ResolvedDecision::Allow);
        assert_eq!(event.approver, AuditApprover::Policy);
    }
}

/// Detail 1: a user's own `deny` rule still denies in `full` — for a simple command
/// and for a compound hiding the denied program in a later segment. Ignoring the
/// deny (letting the blanket allow win) denies nothing, so this test is that
/// mutation's catcher.
#[tokio::test]
async fn full_user_deny_still_denies() {
    let dir = TempDir::new("full-deny");
    let mut rules = Profile::full_access(dir.path()).unwrap().rules().to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(vec!["rm".into(), "-rf".into()]),
            Action::Deny,
        )
        .with_reason("the user denied rm -rf"),
    );
    let profile =
        Profile::custom(Preset::FullAccess, ApprovalPolicy::OnAsk, dir.path(), rules).unwrap();
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);
    for command in ["rm -rf x", "cargo test && rm -rf x"] {
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must stay denied in full, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 0, "a deny never asks");
    let events = audit.events();
    assert_eq!(events.len(), 2, "{events:?}");
    for event in &events {
        assert_eq!(event.decision, ResolvedDecision::Deny);
        assert_eq!(event.approver, AuditApprover::Policy);
    }
}

/// Detail 1: a user's own `ask` rule still asks in `full`, even though the tier would
/// allow the command. Swallowing the user rule (answering from the blanket allow)
/// never reaches the approver, so this test is that mutation's catcher.
#[tokio::test]
async fn full_user_ask_still_asks() {
    let dir = TempDir::new("full-ask");
    let mut rules = Profile::full_access(dir.path()).unwrap().rules().to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(vec!["rm".into(), "-rf".into()]),
            Action::Ask,
        )
        .with_reason("the user wants to review rm -rf"),
    );
    let profile =
        Profile::custom(Preset::FullAccess, ApprovalPolicy::OnAsk, dir.path(), rules).unwrap();
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        profile,
        vec![ApprovalOutcome::Deny { reason: None }],
        &audit,
    );
    let decision = decide(&hooks, "rm -rf x").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "the user ask rule must reach the approver, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "the user ask rule must ask");
    assert_eq!(audit.events()[0].approver, AuditApprover::User);
}

/// Detail 3: `approval = always` still reviews every non-denied call in `full`.
/// Short-circuiting `always` under `full` never reaches the approver, so this
/// test is that mutation's catcher.
#[tokio::test]
async fn full_approval_always_still_reviews() {
    let dir = TempDir::new("full-always");
    let profile = Profile::full_access(dir.path())
        .unwrap()
        .with_approval(ApprovalPolicy::Always);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![ApprovalOutcome::AllowOnce], &audit);
    let decision = decide(&hooks, "rm -rf x").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "the reviewed call is allowed once, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "always must review in full");
    assert_eq!(audit.events()[0].approver, AuditApprover::User);
}

/// The `workspace` tier without a sandbox still asks: the fail-closed constructor keeps
/// the blanket `ask` (slice 1 of `sandbox-auto-allow-shell` only allows with a sandbox
/// in force — see `workspace_sandbox.rs`). Flipping its blanket rule to `allow` never
/// reaches the approver, so this test is that mutation's catcher.
#[tokio::test]
async fn workspace_shell_still_asks() {
    let dir = TempDir::new("workspace-ask");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write(dir.path()).unwrap(),
        vec![ApprovalOutcome::Deny { reason: None }],
        &audit,
    );
    for command in ["rm -rf x", "cargo test && git push"] {
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must still ask in workspace, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 2, "both commands must ask");
    for event in audit.events().iter() {
        assert_eq!(event.approver, AuditApprover::User);
    }
}

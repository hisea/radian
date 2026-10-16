//! ADR 0030 slice 5: the task hard-deny list and the policy-command audit (revision 1 R1).
//!
//! The hard-deny list is structural: it is a set of `deny` rules on the task profile, so it is
//! decided before the approver and no edge policy can relax it. The edge policy command, when it
//! decides, is recorded as `approver = "policy"` with its name (ADR 0006 §5.9).

mod common;

use std::sync::Arc;

use agent_core::{Hooks, InputSource, ToolDecision};
use common::{ScriptedApprover, TempDir, call, ctx};
use radian_permissions::*;
use serde_json::json;

/// The profile the task registry builds: `workspace_write` plus the hard-deny list. `on-ask`
/// here so the approver is actually consulted (the registry uses `auto` when there is no edge
/// policy; that path never reaches the approver and is covered by the registry tests).
fn task_profile(dir: &TempDir) -> Profile {
    Profile::workspace_write(dir.path())
        .unwrap()
        .with_task_hard_deny()
        .with_approval(ApprovalPolicy::OnAsk)
}

fn action(profile: &Profile, command: &str) -> Action {
    evaluate(
        profile.rules(),
        &profile.workspace,
        &call("shell", json!({ "command": command })),
    )
    .action
}

/// Every command the plan lists is denied, and ordinary commands are untouched.
#[test]
fn the_hard_deny_list_denies_exactly_what_the_plan_lists() {
    let dir = TempDir::new("hard-deny");
    let profile = task_profile(&dir);
    for denied in [
        "git push",
        "git push origin radian/t1",
        "git merge other",
        "git checkout main",
        "git checkout master",
        "git checkout -- src/lib.rs",
        "git checkout .",
        "git reset --hard",
        "git restore src/lib.rs",
        "git stash",
        // The deny matcher is conservative over a composite command, so a quoted segment counts.
        "sh -c 'git push'",
        "true && git stash",
    ] {
        assert_eq!(action(&profile, denied), Action::Deny, "{denied}");
    }
    for allowed in [
        "git status",
        "git log --oneline",
        "git diff",
        "cargo test",
        "git commit -m x",
    ] {
        assert_ne!(action(&profile, allowed), Action::Deny, "{allowed}");
    }
}

/// A hard-deny command is denied before the approver is ever consulted, so a policy command (or
/// a human) that would allow it cannot: the mutation "hard-deny bypassable by the policy" turns
/// this red.
#[tokio::test]
async fn a_hard_deny_command_never_reaches_the_approver() {
    let dir = TempDir::new("hard-deny-hook");
    let profile = task_profile(&dir);
    // An approver that would allow whatever it is asked.
    let approver = ScriptedApprover::scripted(vec![ApprovalOutcome::AllowOnce]);
    let audit = Arc::new(RecordingAudit::default());
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::clone(&audit) as Arc<dyn AuditSink>,
    ));
    hooks.set_turn_source(Some(InputSource::Subagent));
    let cx = ctx();

    let decision = Hooks::before_tool_call(
        &hooks,
        &cx,
        &call("shell", json!({ "command": "git push origin radian/t1" })),
    )
    .await
    .unwrap();
    match decision {
        ToolDecision::Deny { reason } => {
            assert!(reason.contains("ADR 0030"), "{reason}");
        }
        other => panic!("the hard-deny command must be denied, got {other:?}"),
    }
    assert_eq!(
        approver.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the approver must never see a hard-deny command"
    );
    let events = audit.events();
    assert_eq!(events.len(), 1, "one audit record");
    assert_eq!(events[0].approver, AuditApprover::Policy);
    assert_eq!(events[0].decision, ResolvedDecision::Deny);
}

/// A policy-command decision is audited as `approver = "policy"` with the policy's name, allow
/// and deny alike, and never as a human answer.
#[tokio::test]
async fn a_policy_decision_records_the_policy_name() {
    let dir = TempDir::new("policy-audit");
    for (outcome, want_decision) in [
        (
            ApprovalOutcome::Policy {
                allow: true,
                name: "safe-commands".into(),
                reason: Some("matched".into()),
            },
            ResolvedDecision::Allow,
        ),
        (
            ApprovalOutcome::Policy {
                allow: false,
                name: "safe-commands".into(),
                reason: None,
            },
            ResolvedDecision::Deny,
        ),
    ] {
        let profile = task_profile(&dir);
        let approver = ScriptedApprover::once(outcome);
        let audit = Arc::new(RecordingAudit::default());
        let hooks = Arc::new(PermissionHooks::new(
            profile,
            Some(approver as Arc<dyn Approver>),
            Arc::clone(&audit) as Arc<dyn AuditSink>,
        ));
        hooks.set_turn_source(Some(InputSource::Subagent));
        hooks.set_task_approver(true);
        let _ = Hooks::before_tool_call(
            &hooks,
            &ctx(),
            &call("shell", json!({ "command": "echo hi" })),
        )
        .await
        .unwrap();
        let events = audit.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].approver, AuditApprover::Policy);
        assert_eq!(events[0].policy.as_deref(), Some("safe-commands"));
        assert_eq!(events[0].decision, want_decision);
    }
}

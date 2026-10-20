//! Slice 1 of `sandbox-auto-allow-shell`: in `workspace`, a sandboxed `shell` call is
//! `allow`, while leaving the sandbox (`outside_sandbox: true`) always goes through
//! `ask`. `full` is unchanged by this slice.

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

async fn decide_shell(
    hooks: &PermissionHooks,
    arguments: serde_json::Value,
) -> Result<ToolDecision, agent_core::HookError> {
    Hooks::before_tool_call(hooks, &ctx(), &call("shell", arguments)).await
}

/// A sandboxed `workspace` shell runs without a prompt: a `cargo test`-like simple
/// command comes back `Allow` with `approver = policy` and never reaches the approver.
/// Dropping the tier's blanket `allow` makes one of these ask, so this test is that
/// mutation's catcher.
#[tokio::test]
async fn sandboxed_workspace_allows_a_simple_shell_without_a_prompt() {
    let dir = TempDir::new("sandboxed-allow");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path()).unwrap(),
        vec![],
        &audit,
    );
    for command in ["cargo test -p radian-coding", "echo hi", "git status"] {
        let decision = decide_shell(&hooks, json!({ "command": command }))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "{command:?} must be allowed inside the sandbox, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 0, "nothing sandboxed may ask");
    let events = audit.events();
    assert_eq!(events.len(), 3, "{events:?}");
    for event in &events {
        assert_eq!(event.decision, ResolvedDecision::Allow);
        assert_eq!(event.approver, AuditApprover::Policy);
    }
}

/// Without a sandbox the same tier still asks: the fail-closed constructor keeps the
/// blanket `ask`. Allowing without a sandbox never reaches the approver, so this test
/// (with `workspace_shell_still_asks` in `full_tier.rs`) is that mutation's catcher.
#[tokio::test]
async fn workspace_without_a_sandbox_still_asks() {
    let dir = TempDir::new("sandboxed-fail-closed");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write(dir.path()).unwrap(),
        vec![ApprovalOutcome::Deny { reason: None }],
        &audit,
    );
    assert!(!hooks.profile().is_sandboxed());
    let decision = decide_shell(&hooks, json!({ "command": "echo hi" }))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "without a sandbox the call must ask (and here be denied), got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "the call must reach the approver");
}

/// `deny` is always first: a user `deny` rule denies a sandboxed call before the
/// blanket `allow`. Letting the allow win never denies, so this test is that mutation's
/// catcher.
#[tokio::test]
async fn sandboxed_workspace_deny_still_denies_first() {
    let dir = TempDir::new("sandboxed-deny");
    let mut rules = Profile::workspace_write_sandboxed(dir.path())
        .unwrap()
        .rules()
        .to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(vec!["rm".into(), "-rf".into()]),
            Action::Deny,
        )
        .with_reason("the user denied rm -rf"),
    );
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        rules,
    )
    .unwrap()
    .with_sandboxed(true);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);
    for command in ["rm -rf x", "cargo test && rm -rf x"] {
        let decision = decide_shell(&hooks, json!({ "command": command }))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must stay denied inside the sandbox, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 0, "a deny never asks");
}

/// A user's own `ask` rule still asks inside the sandbox, even though the tier would
/// allow the command. Answering from the blanket allow never reaches the approver, so
/// this test is that mutation's catcher.
#[tokio::test]
async fn sandboxed_workspace_user_ask_still_asks() {
    let dir = TempDir::new("sandboxed-ask");
    let mut rules = Profile::workspace_write_sandboxed(dir.path())
        .unwrap()
        .rules()
        .to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(vec!["rm".into(), "-rf".into()]),
            Action::Ask,
        )
        .with_reason("the user wants to review rm -rf"),
    );
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        rules,
    )
    .unwrap()
    .with_sandboxed(true);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        profile,
        vec![ApprovalOutcome::Deny { reason: None }],
        &audit,
    );
    let decision = decide_shell(&hooks, json!({ "command": "rm -rf x" }))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "the user ask rule must reach the approver, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "the user ask rule must ask");
    assert_eq!(audit.events()[0].approver, AuditApprover::User);
}

/// A compound still asks inside the sandbox unless every segment is covered by a named
/// user `allow` rule or the read-only table: the tier's blanket `allow` covers no
/// segment. Letting it cover one (so `echo ok; echo two` runs silent) never reaches
/// the approver, so this test is that mutation's catcher.
#[tokio::test]
async fn sandboxed_workspace_compound_still_asks() {
    let dir = TempDir::new("sandboxed-compound");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path()).unwrap(),
        vec![
            ApprovalOutcome::Deny { reason: None },
            ApprovalOutcome::Deny { reason: None },
        ],
        &audit,
    );
    for command in ["echo ok; echo two", "cargo test && rm -rf x"] {
        let decision = decide_shell(&hooks, json!({ "command": command }))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must ask inside the sandbox, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 2, "both compounds must ask");
}

/// …while a compound whose every segment the read-only table covers stays silent.
/// Removing the table path (or the blanket allow it now stands next to) makes this
/// ask, so this test pins the other side.
#[tokio::test]
async fn sandboxed_workspace_table_covered_compound_stays_silent() {
    let dir = TempDir::new("sandboxed-compound-table");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path()).unwrap(),
        vec![],
        &audit,
    );
    let decision = decide_shell(&hooks, json!({ "command": "pwd && ls" }))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "a table-covered compound must stay silent, got {decision:?}"
    );
    assert_eq!(approver.calls(), 0, "no segment may ask");
}

/// Leaving the sandbox always goes through `ask`: an `outside_sandbox` call with a
/// reason reaches the approver even though the tier would allow the same command
/// inside. Letting it skip `ask` (answering from the blanket allow) never reaches the
/// approver, so this test is that mutation's catcher.
#[tokio::test]
async fn outside_sandbox_goes_to_ask() {
    let dir = TempDir::new("outside-ask");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path()).unwrap(),
        vec![ApprovalOutcome::AllowOnce],
        &audit,
    );
    let decision = decide_shell(
        &hooks,
        json!({
            "command": "echo hi",
            "outside_sandbox": true,
            "outside_sandbox_reason": "needs the real home directory",
        }),
    )
    .await
    .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "the approved outside call must be allowed, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "leaving the sandbox must ask");
    assert_eq!(audit.events()[0].approver, AuditApprover::User);
}

/// `outside_sandbox: true` without a reason is denied outright — it never even becomes
/// a question. Asking it anyway (or letting it through) fails this test.
#[tokio::test]
async fn outside_sandbox_without_a_reason_is_denied() {
    let dir = TempDir::new("outside-no-reason");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path()).unwrap(),
        vec![],
        &audit,
    );
    for arguments in [
        json!({ "command": "echo hi", "outside_sandbox": true }),
        json!({ "command": "echo hi", "outside_sandbox": true, "outside_sandbox_reason": "" }),
        json!({ "command": "echo hi", "outside_sandbox": true, "outside_sandbox_reason": "  " }),
    ] {
        let decision = decide_shell(&hooks, arguments).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "a reason-less outside call must be denied, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 0, "a reason-less call never asks");
    for event in audit.events().iter() {
        assert_eq!(event.decision, ResolvedDecision::Deny);
        assert_eq!(event.approver, AuditApprover::Policy);
    }
}

/// `deny` still wins first for an outside call: a denied program cannot leave the
/// sandbox by asking. Denying nothing fails this test.
#[tokio::test]
async fn outside_sandbox_deny_still_denies_first() {
    let dir = TempDir::new("outside-deny");
    let mut rules = Profile::workspace_write_sandboxed(dir.path())
        .unwrap()
        .rules()
        .to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(vec!["rm".into(), "-rf".into()]),
            Action::Deny,
        )
        .with_reason("the user denied rm -rf"),
    );
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        rules,
    )
    .unwrap()
    .with_sandboxed(true);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);
    let decision = decide_shell(
        &hooks,
        json!({
            "command": "rm -rf x",
            "outside_sandbox": true,
            "outside_sandbox_reason": "cleanup",
        }),
    )
    .await
    .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "a denied outside call must stay denied, got {decision:?}"
    );
    assert_eq!(approver.calls(), 0, "a deny never asks");
}

/// A session grant for leaving the sandbox covers only later outside calls with the
/// same command — and a grant never crosses the sandbox boundary. Merging the keys
/// (so one grant covers both kinds) fails this test.
#[tokio::test]
async fn a_grant_never_crosses_the_sandbox_boundary() {
    let dir = TempDir::new("outside-grant");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path()).unwrap(),
        vec![ApprovalOutcome::AllowForSession, ApprovalOutcome::AllowOnce],
        &audit,
    );
    let outside = json!({
        "command": "echo hi",
        "outside_sandbox": true,
        "outside_sandbox_reason": "needs the real home directory",
    });
    let first = decide_shell(&hooks, outside.clone()).await.unwrap();
    assert!(matches!(first, ToolDecision::Allow), "{first:?}");
    assert_eq!(approver.calls(), 1);
    // Same outside call: the session grant covers it, no second question.
    let second = decide_shell(&hooks, outside).await.unwrap();
    assert!(matches!(second, ToolDecision::Allow), "{second:?}");
    assert_eq!(
        approver.calls(),
        1,
        "the grant must cover the same outside call"
    );
    // The same command *inside* the sandbox is already silent — but under a profile
    // without the blanket allow it must ask again: the outside grant must not cover it.
    hooks.set_profile(Profile::workspace_write(dir.path()).unwrap());
    let inside = decide_shell(&hooks, json!({ "command": "echo hi" }))
        .await
        .unwrap();
    assert!(
        matches!(inside, ToolDecision::Allow),
        "the scripted approver allows, got {inside:?}"
    );
    assert_eq!(
        approver.calls(),
        2,
        "the outside grant must not cover the same command inside"
    );
}

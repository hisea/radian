//! The hook itself: session grants, cancellation, non-human sources, internal errors, and
//! exactly one audit record per decision.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::{HookContext, Hooks, InputSource, ToolCall, ToolDecision};
use common::{Gate, ScriptedApprover, TempDir, call, ctx};
use radian_permissions::*;
use serde_json::json;

fn asking_profile(dir: &TempDir) -> Profile {
    // No rules: every call is `ask` (the conservative default).
    Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        Vec::new(),
    )
    .unwrap()
}

fn make_hooks(
    profile: Profile,
    approver: Arc<ScriptedApprover>,
    audit: Arc<RecordingAudit>,
) -> Arc<PermissionHooks> {
    Arc::new(PermissionHooks::new(
        profile,
        Some(approver as Arc<dyn Approver>),
        audit,
    ))
}

fn shell() -> ToolCall {
    call("shell", json!({"command": "git status"}))
}

async fn decide(
    hooks: &PermissionHooks,
    cx: &HookContext,
    call: &ToolCall,
) -> Result<ToolDecision, agent_core::HookError> {
    Hooks::before_tool_call(hooks, cx, call).await
}

/// Test 4: "allow for this session" applies to the very next call (no turn boundary), and a
/// new session instance (what resume is) does not have it.
#[tokio::test]
async fn a_session_grant_applies_to_the_next_call_and_not_after_resume() {
    let dir = TempDir::new("grant");
    let profile = asking_profile(&dir);
    let approver = ScriptedApprover::scripted(vec![
        ApprovalOutcome::AllowForSession,
        ApprovalOutcome::Deny { reason: None },
    ]);
    let hooks = make_hooks(
        profile.clone(),
        Arc::clone(&approver),
        Arc::new(RecordingAudit::default()),
    );
    hooks.set_turn_source(Some(InputSource::User));
    let cx = ctx();

    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);
    // Immediately again, same resource: allowed by the grant, no second ask, no boundary.
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(
        approver.calls(),
        1,
        "the grant must take effect on the next call"
    );

    // A fresh instance is what a resumed session looks like: the grant is gone.
    let resumed = make_hooks(
        profile,
        Arc::clone(&approver),
        Arc::new(RecordingAudit::default()),
    );
    resumed.set_turn_source(Some(InputSource::User));
    assert!(matches!(
        decide(&resumed, &cx, &shell()).await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 2, "resume must ask again");
}

/// Test 5: cancelling while the approver waits returns a denial at once; the approver is not
/// allowed to finish.
#[tokio::test]
async fn cancellation_interrupts_the_approver() {
    let dir = TempDir::new("cancel");
    let profile = asking_profile(&dir);
    let gate = Gate::new();
    let approver = ScriptedApprover::gated(Arc::clone(&gate), vec![ApprovalOutcome::AllowOnce]);
    let hooks = make_hooks(profile, approver, Arc::new(RecordingAudit::default()));
    hooks.set_turn_source(Some(InputSource::User));

    let cx = ctx();
    let token = cx.cancellation.clone();
    let task_hooks = Arc::clone(&hooks);
    let task = tokio::spawn(async move {
        let call = shell();
        Hooks::before_tool_call(&*task_hooks, &cx, &call).await
    });

    gate.wait_started().await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the hook must not hang after cancellation")
        .unwrap();
    assert!(
        matches!(result, Ok(ToolDecision::Deny { .. })),
        "{result:?}"
    );
    assert!(
        !gate.released(),
        "after cancellation the hook must not wait for the approver"
    );
}

/// Test 6: a non-human turn cannot turn an `ask` into an `allow`, and cannot borrow a
/// session grant earned in a human turn. An unset source is treated the same way.
#[tokio::test]
async fn a_non_human_source_cannot_ask_or_borrow_a_grant() {
    let dir = TempDir::new("source");
    let profile = asking_profile(&dir);
    let approver = ScriptedApprover::scripted(vec![
        ApprovalOutcome::AllowForSession,
        ApprovalOutcome::AllowOnce,
    ]);
    let hooks = make_hooks(
        profile,
        Arc::clone(&approver),
        Arc::new(RecordingAudit::default()),
    );
    let cx = ctx();

    hooks.set_turn_source(Some(InputSource::User));
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);

    // Same resource, now from a subagent: denied, and the approver is not consulted.
    hooks.set_turn_source(Some(InputSource::Subagent));
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 1, "a non-human turn must not ask");

    // Unset source: fail safe, denied, approver not consulted.
    hooks.set_turn_source(None);
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 1);
}

/// Test 7: an internal error (a failed approver) denies — the hook returns `Err`, which
/// Core treats as a denial — and still records one denial.
#[tokio::test]
async fn an_internal_error_denies() {
    let dir = TempDir::new("error");
    let profile = asking_profile(&dir);
    let approver = ScriptedApprover::failing("boom");
    let audit = Arc::new(RecordingAudit::default());
    let hooks = make_hooks(profile, approver, Arc::clone(&audit) as Arc<RecordingAudit>);
    hooks.set_turn_source(Some(InputSource::User));

    let result = decide(&hooks, &ctx(), &shell()).await;
    assert!(result.is_err(), "an internal error must surface as Err");
    let events = audit.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].decision, ResolvedDecision::Deny);
    assert_eq!(events[0].approver, AuditApprover::Policy);
}

/// Test 8: each decision emits exactly one `PermissionResolved`, with the right `approver`.
#[tokio::test]
async fn each_decision_emits_exactly_one_permission_resolved() {
    // Allow by rule -> policy.
    {
        let dir = TempDir::new("audit-allow");
        let profile = Profile::custom(
            Preset::WorkspaceWrite,
            ApprovalPolicy::OnAsk,
            dir.path(),
            vec![Rule::new(
                ToolMatcher::Name("shell".into()),
                ResourceMatcher::Any,
                Action::Allow,
            )],
        )
        .unwrap();
        let audit = Arc::new(RecordingAudit::default());
        let hooks = make_hooks(
            profile,
            ScriptedApprover::once(ApprovalOutcome::Deny { reason: None }),
            Arc::clone(&audit),
        );
        hooks.set_turn_source(Some(InputSource::User));
        assert!(matches!(
            decide(&hooks, &ctx(), &shell()).await.unwrap(),
            ToolDecision::Allow
        ));
        let events = audit.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision, ResolvedDecision::Allow);
        assert_eq!(events[0].approver, AuditApprover::Policy);
    }
    // Deny by rule -> policy.
    {
        let dir = TempDir::new("audit-deny");
        let profile = Profile::custom(
            Preset::WorkspaceWrite,
            ApprovalPolicy::OnAsk,
            dir.path(),
            vec![Rule::new(
                ToolMatcher::Name("shell".into()),
                ResourceMatcher::Any,
                Action::Deny,
            )],
        )
        .unwrap();
        let audit = Arc::new(RecordingAudit::default());
        let hooks = make_hooks(
            profile,
            ScriptedApprover::once(ApprovalOutcome::Deny { reason: None }),
            Arc::clone(&audit),
        );
        assert!(matches!(
            decide(&hooks, &ctx(), &shell()).await.unwrap(),
            ToolDecision::Deny { .. }
        ));
        let events = audit.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision, ResolvedDecision::Deny);
        assert_eq!(events[0].approver, AuditApprover::Policy);
    }
    // Ask -> user allows -> user.
    {
        let dir = TempDir::new("audit-user-allow");
        let audit = Arc::new(RecordingAudit::default());
        let hooks = make_hooks(
            asking_profile(&dir),
            ScriptedApprover::once(ApprovalOutcome::AllowOnce),
            Arc::clone(&audit),
        );
        hooks.set_turn_source(Some(InputSource::User));
        assert!(matches!(
            decide(&hooks, &ctx(), &shell()).await.unwrap(),
            ToolDecision::Allow
        ));
        let events = audit.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision, ResolvedDecision::Allow);
        assert_eq!(events[0].approver, AuditApprover::User);
    }
    // Ask -> user denies -> user.
    {
        let dir = TempDir::new("audit-user-deny");
        let audit = Arc::new(RecordingAudit::default());
        let hooks = make_hooks(
            asking_profile(&dir),
            ScriptedApprover::once(ApprovalOutcome::Deny {
                reason: Some("no".into()),
            }),
            Arc::clone(&audit),
        );
        hooks.set_turn_source(Some(InputSource::User));
        assert!(matches!(
            decide(&hooks, &ctx(), &shell()).await.unwrap(),
            ToolDecision::Deny { .. }
        ));
        let events = audit.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision, ResolvedDecision::Deny);
        assert_eq!(events[0].approver, AuditApprover::User);
    }
    // Non-human ask -> policy denies.
    {
        let dir = TempDir::new("audit-nonhuman");
        let audit = Arc::new(RecordingAudit::default());
        let hooks = make_hooks(
            asking_profile(&dir),
            ScriptedApprover::once(ApprovalOutcome::AllowOnce),
            Arc::clone(&audit),
        );
        hooks.set_turn_source(Some(InputSource::Subagent));
        assert!(matches!(
            decide(&hooks, &ctx(), &shell()).await.unwrap(),
            ToolDecision::Deny { .. }
        ));
        let events = audit.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision, ResolvedDecision::Deny);
        assert_eq!(events[0].approver, AuditApprover::Policy);
    }
}

/// Test 12: the effective turn source only ever lowers (ADR 0012 decision 5). A user turn
/// that later admits a non-human input loses the grant and can no longer turn `ask` into
/// `allow`; a later human input cannot raise it back; the next turn's reset restores it.
#[tokio::test]
async fn the_turn_source_only_ever_lowers() {
    let dir = TempDir::new("observe");
    let approver = ScriptedApprover::scripted(vec![ApprovalOutcome::AllowForSession]);
    let hooks = make_hooks(
        asking_profile(&dir),
        Arc::clone(&approver),
        Arc::new(RecordingAudit::default()),
    );
    let cx = ctx();

    // A user turn obtains a session grant.
    hooks.set_turn_source(Some(InputSource::User));
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);

    // A subagent input is admitted mid-turn: the effective source lowers to it.
    hooks.observe_input_source(InputSource::Subagent);
    assert_eq!(hooks.turn_source(), Some(InputSource::Subagent));
    // The grant no longer applies, and `ask` cannot become `allow`.
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 1, "a non-human turn must not ask");

    // Observing a human input afterwards does not raise it back.
    hooks.observe_input_source(InputSource::User);
    assert_eq!(
        hooks.turn_source(),
        Some(InputSource::Subagent),
        "observe must never raise the source"
    );
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Deny { .. }
    ));

    // The next turn's reset restores it; the grant itself was never revoked.
    hooks.set_turn_source(Some(InputSource::User));
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);
}

/// An unset source ranks lowest, so observing cannot raise it: a host that forgets the
/// turn-start reset stays fail-closed.
#[tokio::test]
async fn observing_on_an_unset_source_stays_non_human() {
    let dir = TempDir::new("observe-unset");
    let approver = ScriptedApprover::scripted(vec![ApprovalOutcome::AllowOnce]);
    let hooks = make_hooks(
        asking_profile(&dir),
        Arc::clone(&approver),
        Arc::new(RecordingAudit::default()),
    );

    hooks.observe_input_source(InputSource::User);
    assert_eq!(
        hooks.turn_source(),
        None,
        "an unset source must not be raised"
    );
    assert!(matches!(
        decide(&hooks, &ctx(), &shell()).await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 0);
}

// ---------- ADR 0026 D9: the `auto` approval policy ----------

/// `Auto` answers every `ask` with "allow" without reaching the approver — the same
/// observable behavior `--yes` has always had: only the question is removed.
#[tokio::test]
async fn auto_answers_an_ask_without_the_approver() {
    let dir = TempDir::new("auto-ask");
    // No rules: every call is `ask`, so `Auto` is the only thing that can allow it.
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::Auto,
        dir.path(),
        Vec::new(),
    )
    .unwrap();
    let audit = Arc::new(RecordingAudit::default());
    let approver = ScriptedApprover::scripted(vec![]);
    let hooks = make_hooks(profile, Arc::clone(&approver), Arc::clone(&audit));
    hooks.set_turn_source(Some(InputSource::User));

    assert_eq!(
        hooks.approval(),
        ApprovalPolicy::Auto,
        "the profile seeds the live policy"
    );
    assert!(matches!(
        decide(&hooks, &ctx(), &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 0, "auto must not ask");
    // The audit record says the policy allowed it, not a human answer.
    let events = audit.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].decision, ResolvedDecision::Allow);
    assert_eq!(events[0].approver, AuditApprover::Policy);
}

/// `Auto` only removes the question: a `deny` rule still denies, and the denial is audited
/// as a policy denial exactly as without `Auto`.
#[tokio::test]
async fn auto_still_honors_deny_rules() {
    let dir = TempDir::new("auto-deny");
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::Auto,
        dir.path(),
        vec![Rule::new(
            ToolMatcher::Any,
            ResourceMatcher::Any,
            Action::Deny,
        )],
    )
    .unwrap();
    let approver = ScriptedApprover::scripted(vec![]);
    let hooks = make_hooks(profile, approver, Arc::new(RecordingAudit::default()));
    hooks.set_turn_source(Some(InputSource::User));

    let decision = decide(&hooks, &ctx(), &shell()).await.unwrap();
    let ToolDecision::Deny { reason } = decision else {
        panic!("a deny rule must deny under auto, got {decision:?}")
    };
    assert!(!reason.is_empty());
}

/// The in-session switch (ADR 0026 D9): `set_approval` changes how the very next call is
/// resolved, both ways, and writes one audit record per switch.
#[tokio::test]
async fn set_approval_switches_in_place_and_audits_each_switch() {
    let dir = TempDir::new("auto-switch");
    let audit = Arc::new(RecordingAudit::default());
    let approver = ScriptedApprover::scripted(vec![ApprovalOutcome::AllowOnce]);
    let hooks = make_hooks(
        asking_profile(&dir),
        Arc::clone(&approver),
        Arc::clone(&audit),
    );
    hooks.set_turn_source(Some(InputSource::User));
    let cx = ctx();

    // On: the next call is allowed without asking.
    hooks.set_approval(ApprovalPolicy::Auto);
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 0);

    // Off: the same call asks again.
    hooks.set_approval(ApprovalPolicy::OnAsk);
    assert!(matches!(
        decide(&hooks, &cx, &shell()).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1, "on-ask must ask again");

    assert_eq!(
        audit.policy_changes(),
        vec![ApprovalPolicy::Auto, ApprovalPolicy::OnAsk],
        "one audit record per switch"
    );
}

/// The `--approval` / config names (ADR 0012, ADR 0026 D9) round-trip.
#[test]
fn approval_policy_names_round_trip() {
    for name in ["on-ask", "never", "always", "auto"] {
        assert_eq!(ApprovalPolicy::parse(name).unwrap().as_str(), name);
    }
    assert_eq!(ApprovalPolicy::parse("yolo"), None);
    assert_eq!(ApprovalPolicy::parse(""), None);
}

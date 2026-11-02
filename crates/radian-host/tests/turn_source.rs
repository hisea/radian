//! ADR 0011 L2: a turn's effective source is the least-privileged of every input admitted
//! into it. These tests drive the inbox's claim seam directly (the same
//! [`radian_host::CommandHooks`] callbacks `open_session` installs) so the order is
//! deterministic: both inputs are queued before anything claims the turn.
//!
//! The observation is the permission layer's own answer: a `User` turn earns a session
//! grant for a tool, and a turn whose source lowered cannot use it.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::future::BoxFuture;
use radian_core::{
    CancellationToken, HookContext, Hooks, InputSource, Message, ToolCall, ToolDecision,
};
use radian_host::CommandHooks;
use radian_permissions::{
    Action, ApprovalError, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Approver, NoopAudit,
    PermissionHooks, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use serde_json::json;

/// An approver that always grants "allow for this session" and counts its calls.
struct AllowForSession(AtomicUsize);

impl AllowForSession {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicUsize::new(0)))
    }

    fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Approver for AllowForSession {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ApprovalOutcome::AllowForSession) })
    }
}

fn context() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 0,
    }
}

fn write_call(workspace: &Path) -> ToolCall {
    ToolCall {
        id: "call-write".into(),
        name: "write".into(),
        arguments: json!({
            "path": workspace.join("note.txt").to_string_lossy(),
            "content": "x",
        }),
    }
}

/// A workspace with an `ask` rule on `write` and a live permission layer wired to the
/// command hooks' claim/drain observers.
fn hooks(
    workspace: &Path,
    label: &str,
) -> (
    Arc<radian_runtime::Inbox>,
    Arc<PermissionHooks>,
    Arc<AllowForSession>,
) {
    let inbox = radian_runtime::Inbox::new(16);
    let commands = Arc::new(CommandHooks::new(
        &radian_host::config::HooksConfig::default(),
        &radian_host::config::HooksConfig::default(),
        workspace,
        label,
        radian_sandbox::SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ));
    let approver = AllowForSession::new();
    let profile = Profile::custom(
        Preset::FullAccess,
        ApprovalPolicy::OnAsk,
        workspace,
        vec![Rule::new(
            ToolMatcher::Name("write".into()),
            ResourceMatcher::Any,
            Action::Ask,
        )],
    )
    .unwrap();
    let permissions = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    commands.set_turn_source_hooks(Arc::downgrade(&permissions));
    (inbox, permissions, approver)
}

/// A `User` turn earns the grant this test then tries to reuse.
async fn earn_the_grant(
    permissions: &PermissionHooks,
    approver: &AllowForSession,
    workspace: &Path,
) {
    permissions.set_turn_source(Some(InputSource::User));
    let decision = permissions
        .before_tool_call(&context(), &write_call(workspace))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "the first (human) call earns the grant: {decision:?}"
    );
    assert_eq!(approver.calls(), 1);
}

/// Edge 1: an owner `followup` is queued, then a no-token `steer` arrives before the claim.
/// The turn that admits both runs as `Scheduler`, so the session grant no longer applies.
#[tokio::test]
async fn an_idle_owner_turn_that_absorbs_a_queued_scheduler_steer_runs_non_human() {
    let workspace = common::TempDir::canonical("radian-turn-source-idle");
    let (inbox, permissions, approver) = hooks(workspace.path(), "idle-owner-steer");
    earn_the_grant(&permissions, &approver, workspace.path()).await;

    // Idle: the owner's followup is queued first, then a no-token steer. Both control-port
    // branches keep the permission layer current; the last one leaves `Scheduler`.
    let owner = inbox
        .followup(Message::user("owner task"), InputSource::User)
        .await
        .unwrap();
    let steer = inbox
        .steer(Message::user("scheduled steer"), InputSource::Scheduler)
        .await
        .unwrap();
    permissions.set_turn_source(Some(InputSource::Scheduler));

    let claimed = inbox.claim_next_turn().await.expect("a turn opens");
    assert_eq!(claimed.id, owner, "the owner's followup opens the turn");
    assert_ne!(claimed.id, steer);
    assert_eq!(
        permissions.turn_source(),
        Some(InputSource::Scheduler),
        "the claim must not raise the source above the queued steer it admits (ADR 0011 L2)"
    );

    // The admitted steer is non-human: the session grant cannot answer the ask, and the
    // approver is never asked.
    let decision = permissions
        .before_tool_call(&context(), &write_call(workspace.path()))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "a non-human turn must not borrow the human grant: {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "the approver is never asked");
}

/// Edge 5: two owner inputs in one turn still leave the turn `User`.
#[tokio::test]
async fn an_owner_turn_with_two_owner_inputs_stays_user() {
    let workspace = common::TempDir::canonical("radian-turn-source-owner-only");
    let (inbox, permissions, approver) = hooks(workspace.path(), "owner-only");
    earn_the_grant(&permissions, &approver, workspace.path()).await;

    let owner = inbox
        .followup(Message::user("owner task"), InputSource::User)
        .await
        .unwrap();
    inbox
        .steer(Message::user("owner steer"), InputSource::User)
        .await
        .unwrap();
    permissions.set_turn_source(Some(InputSource::User));

    let claimed = inbox.claim_next_turn().await.expect("a turn opens");
    assert_eq!(claimed.id, owner);
    assert_eq!(
        permissions.turn_source(),
        Some(InputSource::User),
        "two owner inputs leave the turn the owner's (ADR 0011 L2)"
    );

    // The grant still answers the ask without a new prompt.
    let decision = permissions
        .before_tool_call(&context(), &write_call(workspace.path()))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "an owner-only turn keeps the session grant: {decision:?}"
    );
    assert_eq!(approver.calls(), 1);
}

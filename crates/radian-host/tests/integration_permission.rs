//! ADR 0030: `integrate_task`'s permission is `merge_needs_human`.
//!
//! Hook level, on the exact profile the lead session builds: with the default `true` the tool is
//! left unmatched, so it resolves to `ask` and reaches the human queue with the lead session as
//! the origin; with `false` it is allowed without asking; and a non-`User` turn's ask is refused,
//! never auto-allowed (ADR 0012).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_core::{CancellationToken, HookContext, Hooks, InputSource, ToolCall, ToolDecision};
use radian_host::{PendingRequest, PendingResponse, QueuedApprover, SessionQueue};
use radian_permissions::{ApprovalOutcome, NoopAudit, PermissionHooks, Profile};

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-integ-perm-{label}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// The lead profile exactly as `open_session` builds it.
fn lead_profile(dir: &Path, merge_needs_human: bool) -> Profile {
    Profile::workspace_write(dir)
        .unwrap()
        .with_allowed_tools(&radian_host::lead_allowed_tools(merge_needs_human))
}

fn call() -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "integrate_task".into(),
        arguments: serde_json::json!({ "id": "t1" }),
    }
}

fn cx() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 1,
    }
}

async fn head(queue: &SessionQueue) -> radian_host::PendingView<PendingRequest> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(view) = queue.head() {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no approval reached the human queue"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The default: `merge_needs_human = true` makes `integrate_task` an ask on the human queue.
#[tokio::test]
async fn with_merge_needs_human_it_asks_the_human() {
    let dir = scratch("ask");
    let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "lead"));
    let hooks = Arc::new(PermissionHooks::new(
        lead_profile(&dir, true),
        Some(approver),
        Arc::new(NoopAudit),
    ));
    hooks.set_turn_source(Some(InputSource::User));

    let cx = cx();
    let call = call();
    let h = Arc::clone(&hooks);
    let decide = tokio::spawn(async move { Hooks::before_tool_call(&h, &cx, &call).await });

    let pending = head(&queue).await;
    assert_eq!(pending.origin, "lead", "the lead session is the origin");
    match &pending.request {
        PendingRequest::Approval(request) => assert_eq!(request.tool, "integrate_task"),
        other => panic!("expected an approval, got {other:?}"),
    }
    queue
        .answer(
            pending.request_id,
            PendingResponse::Approval(ApprovalOutcome::Deny { reason: None }),
        )
        .expect("answer");
    let decision = decide.await.unwrap().expect("the hook ran");
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "a denied ask is a denial: {decision:?}"
    );
}

/// `merge_needs_human = false` allows it without asking.
#[tokio::test]
async fn without_merge_needs_human_it_is_allowed_without_asking() {
    let dir = scratch("allow");
    let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "lead"));
    let hooks = PermissionHooks::new(
        lead_profile(&dir, false),
        Some(approver),
        Arc::new(NoopAudit),
    );
    hooks.set_turn_source(Some(InputSource::User));
    let decision = Hooks::before_tool_call(&hooks, &cx(), &call())
        .await
        .expect("the hook ran");
    assert_eq!(decision, ToolDecision::Allow);
    assert!(queue.head().is_none(), "nothing may be raised on the queue");
}

/// A non-`User` turn's ask is refused, never auto-allowed, and never reaches the human queue.
#[tokio::test]
async fn a_non_user_turn_is_refused_not_auto_allowed() {
    let dir = scratch("nonuser");
    let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "lead"));
    let hooks = PermissionHooks::new(
        lead_profile(&dir, true),
        Some(approver),
        Arc::new(NoopAudit),
    );
    hooks.set_turn_source(Some(InputSource::Subagent));
    match Hooks::before_tool_call(&hooks, &cx(), &call())
        .await
        .expect("the hook ran")
    {
        ToolDecision::Deny { reason } => {
            assert!(reason.contains("approval"), "{reason}");
        }
        other => panic!("a non-User ask must be refused, got {other:?}"),
    }
    assert!(
        queue.head().is_none(),
        "a non-User turn must not reach the human queue"
    );
}

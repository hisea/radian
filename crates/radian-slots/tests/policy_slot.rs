//! `PolicySlot`: swappable allow/deny policy with the same gate semantics as `ModelSlot`.

mod common;

use std::sync::Arc;

use agent_core::*;

use common::{EchoTools, Gate, Reply, ScriptedModel};
use radian_slots::*;

/// A test policy that parks the decision on a gate, then denies. Stands in for a human
/// approval prompt or a slow policy lookup.
struct GatedDeny {
    gate: Arc<Gate>,
    reason: &'static str,
}

impl Policy for GatedDeny {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        self.gate.hold().await;
        Ok(ToolDecision::Deny {
            reason: self.reason.into(),
        })
    }
}

fn agent_with(
    model: Arc<ScriptedModel>,
    tools: EchoTools,
    slot: Arc<PolicySlot>,
) -> Arc<Agent<Arc<ScriptedModel>, EchoTools, Arc<PolicySlot>>> {
    Arc::new(Agent::new(model, tools).with_hooks(slot))
}

fn scripted_deny_then_allow() -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new([
        Reply::ToolCall {
            id: "c1",
            name: "echo",
        },
        Reply::Text("done1"),
        Reply::ToolCall {
            id: "c2",
            name: "echo",
        },
        Reply::Text("done2"),
    ]))
}

#[tokio::test]
async fn policy_slot_denies_then_allows_after_a_swap() {
    let slot = Arc::new(PolicySlot::new(DenyAll::new("not now")));
    let tools = EchoTools::new("echo");
    let agent = agent_with(scripted_deny_then_allow(), tools.clone(), slot.clone());
    let mut session = agent.open_session().await.unwrap();

    let outcome = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "done1");
    // Denied: the tool never ran, and the model saw a denied result.
    assert!(tools.executed().is_empty());
    assert!(session.transcript().iter().any(|m| matches!(
        m,
        Message::Tool { result } if result.error_code.as_deref() == Some("denied")
    )));

    let record = slot.swap("allow", AllowAll);
    assert_eq!(record.revision, 1);
    let outcome = session
        .run_turn("hi2", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "done2");
    assert_eq!(tools.executed(), ["c2"]);
}

#[tokio::test]
async fn policy_swap_does_not_affect_an_inflight_decision() {
    let gate = Arc::new(Gate::new());
    let slot = Arc::new(PolicySlot::new(GatedDeny {
        gate: gate.clone(),
        reason: "old policy",
    }));
    let tools = EchoTools::new("echo");
    let agent = agent_with(scripted_deny_then_allow(), tools.clone(), slot.clone());
    let mut session = agent.open_session().await.unwrap();
    let handle = tokio::spawn(async move {
        session
            .run_turn("hi", TurnOptions::default(), &NoopSink)
            .await
            .unwrap()
    });

    gate.wait_entered(1).await; // the old policy is mid-decision
    slot.swap("allow", AllowAll); // swap while that decision is in flight
    gate.open();

    let outcome = handle.await.unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "done1");
    // The in-flight decision used the OLD policy, so the call was denied.
    assert!(tools.executed().is_empty());

    // The next decision uses the new policy.
    let mut session = agent.open_session().await.unwrap();
    let outcome = session
        .run_turn("hi2", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "done2");
    assert_eq!(tools.executed(), ["c2"]);
}

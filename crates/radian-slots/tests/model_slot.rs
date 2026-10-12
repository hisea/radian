//! `ModelSlot`: type erasure, swap semantics during and between model calls.

mod common;

use std::sync::Arc;

use agent_core::*;

use common::{ConstantModel, EchoTools, Gate, Reply, ScriptedModel, last_assistant};
use radian_slots::*;

#[tokio::test]
async fn model_slot_is_a_model_and_erases_two_distinct_adapter_types() {
    // `Agent<ModelSlot, ...>` works directly (ModelSlot: Model), and the same slot holds two
    // different concrete adapter types over its lifetime.
    let slot = Arc::new(ModelSlot::new(ConstantModel::new("first")));
    let agent = Arc::new(Agent::new(Arc::clone(&slot), NoTools));
    let mut session = agent.open_session().await.unwrap();
    let outcome = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(
        last_assistant(session.transcript()).as_deref(),
        Some("first")
    );
    assert!(matches!(outcome.status, RunStatus::Completed));

    // A different concrete adapter type goes through the same erased slot.
    let record = slot.swap("second", ScriptedModel::new([Reply::Text("second")]));
    assert_eq!(record.revision, 1);
    let outcome = session
        .run_turn("again", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "second");
}

#[tokio::test]
async fn swap_during_an_inflight_request_leaves_it_on_the_old_adapter() {
    let gate = Arc::new(Gate::new());
    let slot = Arc::new(ModelSlot::new(ConstantModel::gated("old", gate.clone())));
    let agent = Arc::new(Agent::new(Arc::clone(&slot), NoTools));
    let mut session = agent.open_session().await.unwrap();
    let handle = tokio::spawn(async move {
        session
            .run_turn("hi", TurnOptions::default(), &NoopSink)
            .await
            .unwrap()
    });
    gate.wait_entered(1).await; // the request is parked inside the old adapter
    let record = slot.swap("new", ConstantModel::new("new"));
    assert!(matches!(
        record.change,
        ConfigChange::ModelRoute { route } if route == "new"
    ));
    gate.open();
    // The in-flight request still finishes on the adapter it started on.
    let outcome = handle.await.unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "old");
    // The next request uses the new adapter.
    let mut session = agent.open_session().await.unwrap();
    let outcome = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "new");
}

#[tokio::test]
async fn swap_mid_turn_affects_the_same_turns_next_model_call() {
    // Pin down the actual semantics with today's `agent-core` surface: `ModelSlot` reads the
    // current adapter on every `complete`, so a swap between two model calls of the SAME turn
    // (here: during the tool execution) is seen by the turn's next call. This is the behavior
    // that ADR 0004 documents and proposes Core to fix for a next-turn guarantee.
    let slot = Arc::new(ModelSlot::new(ScriptedModel::new([
        Reply::ToolCall {
            id: "c1",
            name: "echo",
        },
        Reply::Text("would-have-been-A"),
    ])));
    let tools = EchoTools::new("echo").with_before_execute({
        let slot = Arc::clone(&slot);
        move || {
            slot.swap("B", ConstantModel::new("B"));
        }
    });
    let agent = Arc::new(Agent::new(Arc::clone(&slot), tools.clone()));
    let mut session = agent.open_session().await.unwrap();
    let outcome = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    // The tool ran on call 1, then call 2 saw the swapped-in adapter B.
    assert_eq!(tools.executed(), ["c1"]);
    assert_eq!(outcome.final_message.unwrap().text(), "B");
}

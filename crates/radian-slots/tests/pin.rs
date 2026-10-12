//! The core question of ADR 0004: with today's `agent-core` surface a shared slot has no turn
//! identity, so it can only read the route per call. `PinnedModelSlot` + `with_run` show that
//! a turn identity — the one the ADR asks Core to put into `ModelContext` — is sufficient to
//! pin a route for a whole turn, and that it isolates concurrent turns sharing one slot.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::*;
use tokio::time::timeout;

use common::{ConstantModel, EchoTools, Gate, Reply, ScriptedModel};
use radian_slots::*;

/// The same scenario in all tests: turn call 1 returns a tool call (the tool swaps the
/// shared route to `B` mid-turn), then call 2 answers. With a pinned route the answer is
/// still `A`; without one it is `B`.

#[tokio::test]
async fn pinned_slot_keeps_route_across_the_model_calls_of_one_turn() {
    let log = Arc::new(ConfigLog::new());
    let slot = Arc::new(PinnedModelSlot::with_config_log(
        ScriptedModel::new([
            Reply::ToolCall {
                id: "c1",
                name: "echo",
            },
            Reply::Text("A-final"),
        ]),
        log,
    ));
    let tools = EchoTools::new("echo").with_before_execute({
        let slot = Arc::clone(&slot);
        move || {
            slot.swap("B", ConstantModel::new("B-final"));
        }
    });
    let agent = Arc::new(Agent::new(Arc::clone(&slot), tools));
    let mut session = agent.open_session().await.unwrap();
    let outcome = with_run(7, session.run_turn("hi", TurnOptions::default(), &NoopSink))
        .await
        .unwrap();
    // The tool ran, then the turn's second model call still used the pinned adapter A.
    assert_eq!(outcome.final_message.unwrap().text(), "A-final");
    slot.forget_run(7);
}

#[tokio::test]
async fn without_a_run_scope_the_pinned_slot_falls_back_to_the_current_route() {
    // Same scenario, but the host forgot to scope the turn: no run identity, so the slot
    // behaves like the plain `ModelSlot` and the swap is visible on call 2.
    let slot = Arc::new(PinnedModelSlot::with_config_log(
        ScriptedModel::new([
            Reply::ToolCall {
                id: "c1",
                name: "echo",
            },
            Reply::Text("A-final"),
        ]),
        Arc::new(ConfigLog::new()),
    ));
    let tools = EchoTools::new("echo").with_before_execute({
        let slot = Arc::clone(&slot);
        move || {
            slot.swap("B", ConstantModel::new("B-final"));
        }
    });
    let agent = Arc::new(Agent::new(Arc::clone(&slot), tools));
    let mut session = agent.open_session().await.unwrap();
    let outcome = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.unwrap().text(), "B-final");
}

#[tokio::test]
async fn run_ids_isolate_concurrent_turns_sharing_one_slot() {
    // Two turns of two sessions share one slot. Each pins the route that was current when
    // it STARTED: turn 1 pins A, then the host swaps to B, then turn 2 pins B. Both then run
    // their tool loops; a shared "turn" cell (e.g. a `model_calls == 1` heuristic) could not
    // tell the two turns apart, the run ids can.
    let gate = Arc::new(Gate::new());
    let log = Arc::new(ConfigLog::new());
    let slot = Arc::new(PinnedModelSlot::with_config_log(
        ScriptedModel::new([
            Reply::ToolCall {
                id: "c1",
                name: "echo",
            },
            Reply::Text("A-final"),
        ]),
        log,
    ));
    let tools = EchoTools::gated("echo", Arc::clone(&gate));
    let agent = Arc::new(Agent::new(Arc::clone(&slot), tools));

    let mut s1 = agent.open_session().await.unwrap();
    let run1 = tokio::spawn(async move {
        with_run(1, s1.run_turn("t1", TurnOptions::default(), &NoopSink))
            .await
            .unwrap()
    });
    // Bounded watchdogs, not synchronization: with a correct pin map both waits complete in
    // milliseconds. A broken pin map (e.g. one shared turn cell) deadlocks instead, and the
    // timeout turns that hang into a failing assertion.
    timeout(Duration::from_secs(10), gate.wait_entered(1))
        .await
        .expect("turn 1 should pin A and park in its tool"); // turn 1 has pinned A
    slot.swap(
        "B",
        ScriptedModel::new([
            Reply::ToolCall {
                id: "c2",
                name: "echo",
            },
            Reply::Text("B-final"),
        ]),
    );

    let mut s2 = agent.open_session().await.unwrap();
    let run2 = tokio::spawn(async move {
        with_run(2, s2.run_turn("t2", TurnOptions::default(), &NoopSink))
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(10), gate.wait_entered(2))
        .await
        .expect("turn 2 should pin B and park in its tool"); // turn 2 has pinned B
    gate.open();

    let (o1, o2) = tokio::join!(run1, run2);
    assert_eq!(o1.unwrap().final_message.unwrap().text(), "A-final");
    assert_eq!(o2.unwrap().final_message.unwrap().text(), "B-final");
    for id in [1, 2] {
        slot.forget_run(id);
    }
}

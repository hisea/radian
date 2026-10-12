//! SessionDriver: boundary-applied configuration changes (ADR 0004, owner's chosen path).
//!
//! T1–T6 as named in the task brief. The contrast test for T1's semantics (a direct
//! `ModelSlot::swap` mid-turn IS seen by the same turn's next call) lives in `model_slot.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::*;
use tokio::time::timeout;

use common::{ConstantModel, EchoTools, Gate, Reply, ScriptedModel};
use radian_slots::*;

/// Bounded watchdogs so a broken boundary (e.g. a change applied mid-turn that never lets a
/// turn park) fails with an assertion instead of hanging. Synchronization is the gate itself.
async fn wait_entered(gate: &Gate, n: usize, what: &str) {
    timeout(Duration::from_secs(10), gate.wait_entered(n))
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for {what}"));
}

/// T1: a change queued mid-turn is not seen by the rest of this turn.
#[tokio::test]
async fn turn_midway_queued_change_is_not_seen_this_turn() {
    let slots = Arc::new(DriverSlots::new(ScriptedModel::new([
        Reply::ToolCall {
            id: "c1",
            name: "echo",
        },
        Reply::Text("A-final"),
    ])));
    let tools = EchoTools::new("echo").with_before_execute({
        let slots = Arc::clone(&slots);
        move || {
            // Queued during the tool execution, between model call 1 and 2 of THIS turn.
            slots.queue_change(Change::model("B", ConstantModel::new("B-final")));
        }
    });
    let mut driver = SessionDriver::from_slots(slots, tools.clone());
    let outcome = driver.run_turn("hi", &NoopSink).await.unwrap();
    // The tool ran (call 1, old route), then call 2 of the same turn still used the old
    // route: the queued change is not applied until the next boundary.
    assert_eq!(tools.executed(), ["c1"]);
    assert_eq!(outcome.final_message.unwrap().text(), "A-final");
}

/// T2: the change queued mid-turn takes effect at the next turn's boundary.
#[tokio::test]
async fn queued_change_switches_route_at_the_next_turn_boundary() {
    let slots = Arc::new(DriverSlots::new(ScriptedModel::new([
        Reply::ToolCall {
            id: "c1",
            name: "echo",
        },
        Reply::Text("A-final"),
    ])));
    let tools = EchoTools::new("echo").with_before_execute({
        let slots = Arc::clone(&slots);
        move || {
            slots.queue_change(Change::model("B", ConstantModel::new("B-final")));
        }
    });
    let mut driver = SessionDriver::from_slots(slots, tools);
    let first = driver.run_turn("hi", &NoopSink).await.unwrap();
    assert_eq!(first.final_message.unwrap().text(), "A-final");
    let second = driver.run_turn("hi again", &NoopSink).await.unwrap();
    assert_eq!(second.final_message.unwrap().text(), "B-final");
}

/// T3: concurrent sessions with their own slots do not leak each other's changes.
#[tokio::test]
async fn concurrent_drivers_do_not_leak_each_others_changes() {
    let gate = Arc::new(Gate::new());
    let mut da = SessionDriver::new(
        ConstantModel::gated("A-own", gate.clone()),
        EchoTools::new("echo"),
    );
    let mut db = SessionDriver::new(
        ConstantModel::gated("B-own", gate.clone()),
        EchoTools::new("echo"),
    );
    let a_slots = da.slots();
    let b_slots = db.slots();

    let ha = tokio::spawn(async move {
        let o = da.run_turn("t1", &NoopSink).await.unwrap();
        (o, da)
    });
    let hb = tokio::spawn(async move {
        let o = db.run_turn("t1", &NoopSink).await.unwrap();
        (o, db)
    });
    wait_entered(&gate, 2, "both turns parked mid-request").await;

    a_slots.queue_change(Change::model("B", ConstantModel::new("B-new")));
    gate.open();

    let (oa, mut da) = ha.await.unwrap();
    let (ob, mut db) = hb.await.unwrap();
    // Both in-flight turns finished on their own adapters; B never saw A's change.
    assert_eq!(oa.final_message.unwrap().text(), "A-own");
    assert_eq!(ob.final_message.unwrap().text(), "B-own");
    assert_eq!(b_slots.pending_len(), 0);

    // A switches at its own next boundary; B keeps its own route.
    let oa2 = da.run_turn("t2", &NoopSink).await.unwrap();
    let ob2 = db.run_turn("t2", &NoopSink).await.unwrap();
    assert_eq!(oa2.final_message.unwrap().text(), "B-new");
    assert_eq!(ob2.final_message.unwrap().text(), "B-own");
    assert_eq!(a_slots.config_log().records().len(), 1);
    assert_eq!(b_slots.config_log().records().len(), 0);
}

/// T4: a queued policy change applies at the boundary, not mid-turn.
#[tokio::test]
async fn policy_change_applies_at_boundary_not_mid_turn() {
    // The first turn has TWO tool calls so the mid-turn queue lands between two
    // `before_tool_call` decisions of the same turn: call 1 runs, the change is queued
    // during its execution, call 2 must still be governed by the OLD policy.
    let slots = Arc::new(DriverSlots::new(ScriptedModel::new([
        Reply::ToolCall {
            id: "c1",
            name: "echo",
        },
        Reply::ToolCall {
            id: "c2",
            name: "echo",
        },
        Reply::Text("done1"),
        Reply::ToolCall {
            id: "c3",
            name: "echo",
        },
        Reply::Text("done2"),
    ])));
    let tools = EchoTools::new("echo").with_before_execute({
        let slots = Arc::clone(&slots);
        move || {
            // Queued while c1 is running: between the c1 and c2 decisions of THIS turn.
            slots.queue_change(Change::policy("deny", DenyAll::new("not allowed")));
        }
    });
    let mut driver = SessionDriver::from_slots(slots, tools.clone());

    let first = driver.run_turn("t1", &NoopSink).await.unwrap();
    assert_eq!(first.final_message.unwrap().text(), "done1");
    // c1 AND c2 were allowed: the old policy was in force all through this turn, even
    // though the change was queued before c2's decision. (With immediate application, c2
    // would be denied and never run.)
    assert_eq!(tools.executed(), ["c1", "c2"]);

    let second = driver.run_turn("t2", &NoopSink).await.unwrap();
    assert_eq!(second.final_message.unwrap().text(), "done2");
    // c3 was denied at the boundary: the tool never ran for it.
    assert_eq!(tools.executed(), ["c1", "c2"]);
}

/// T5: `ConfigChanged` is recorded when a change is APPLIED, not when it is queued.
///
/// Rationale (also in ADR 0004): the record must match the configuration that really took
/// effect. A queued-but-never-applied change (driver closed before the next turn) would
/// otherwise claim an effect it never had, and a replay could not reconstruct effective
/// history from records alone.
#[tokio::test]
async fn records_config_changes_at_apply_time() {
    let log = Arc::new(ConfigLog::new());
    let mut driver = SessionDriver::with_config_log(
        ConstantModel::new("A"),
        EchoTools::new("echo"),
        log.clone(),
    );

    let first = driver.run_turn("t1", &NoopSink).await.unwrap();
    assert_eq!(first.final_message.unwrap().text(), "A");
    assert!(log.records().is_empty(), "no change was queued or applied");

    driver.queue_change(Change::model("B", ConstantModel::new("B")));
    assert!(
        log.records().is_empty(),
        "queueing is intent, not effect: nothing recorded yet"
    );
    assert_eq!(driver.pending_len(), 1);

    let second = driver.run_turn("t2", &NoopSink).await.unwrap();
    assert_eq!(second.final_message.unwrap().text(), "B");
    let records = log.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision, 1);
    assert!(matches!(
        &records[0].change,
        ConfigChange::ModelRoute { route } if route == "B"
    ));
    assert_eq!(
        driver.pending_len(),
        0,
        "the queue was drained by the application"
    );

    // The queue having been drained, the next turn applies nothing and records nothing.
    let third = driver.run_turn("t3", &NoopSink).await.unwrap();
    assert_eq!(third.final_message.unwrap().text(), "B");
    assert_eq!(log.records().len(), 1);
}

/// T6: three drivers share one `ConfigLog`; each switches at its own next boundary, without
/// waiting for the others or being overwritten by them.
#[tokio::test]
async fn three_drivers_share_one_log_and_switch_each_at_own_boundary() {
    let gate = Arc::new(Gate::new());
    let log = Arc::new(ConfigLog::new());
    let make = || {
        SessionDriver::with_config_log(
            ScriptedModel::new([
                Reply::ToolCall {
                    id: "c",
                    name: "echo",
                },
                Reply::Text("A-final"),
            ]),
            EchoTools::gated("echo", Arc::clone(&gate)),
            Arc::clone(&log),
        )
    };
    let mut d1 = make();
    let mut d2 = make();
    let mut d3 = make();
    let (s1, s2, s3) = (d1.slots(), d2.slots(), d3.slots());

    let h1 = tokio::spawn(async move {
        let o = d1.run_turn("t", &NoopSink).await.unwrap();
        (o, d1)
    });
    let h2 = tokio::spawn(async move {
        let o = d2.run_turn("t", &NoopSink).await.unwrap();
        (o, d2)
    });
    let h3 = tokio::spawn(async move {
        let o = d3.run_turn("t", &NoopSink).await.unwrap();
        (o, d3)
    });
    wait_entered(&gate, 3, "all three turns parked mid-request").await;

    // The same change is queued for all three while their turns are in flight.
    s1.queue_change(Change::model("B", ConstantModel::new("B-final")));
    s2.queue_change(Change::model("B", ConstantModel::new("B-final")));
    s3.queue_change(Change::model("B", ConstantModel::new("B-final")));
    gate.open();

    let (o1, mut d1) = h1.await.unwrap();
    let (o2, mut d2) = h2.await.unwrap();
    let (o3, mut d3) = h3.await.unwrap();
    for outcome in [&o1, &o2, &o3] {
        assert_eq!(outcome.final_message.as_ref().unwrap().text(), "A-final");
    }
    assert!(
        log.records().is_empty(),
        "queued mid-turn, nothing applied yet"
    );

    // Each driver applies at its own next boundary, in its own order.
    for driver in [&mut d1, &mut d2, &mut d3] {
        let o = driver.run_turn("t", &NoopSink).await.unwrap();
        assert_eq!(o.final_message.unwrap().text(), "B-final");
    }
    let records = log.records();
    assert_eq!(records.len(), 3);
    for (i, record) in records.iter().enumerate() {
        assert_eq!(record.revision, (i + 1) as u64);
        assert!(matches!(
            &record.change,
            ConfigChange::ModelRoute { route } if route == "B"
        ));
    }
}

//! The driver: turn-boundary configuration, steer at the core's safe point, durable event
//! order, and the structural absence of `ActiveRun` from the public API.

mod common;

use std::sync::Arc;

use agent_core::{Agent, InputSource, Message, MessageId, Step, TurnOptions};
use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};
use serde_json::json;

#[allow(dead_code)]
struct Env {
    inbox: Arc<Inbox>,
    journal: Arc<SessionJournal>,
    recorder: Arc<Recorder>,
    model: Arc<ScriptedModel>,
    tools: MemTools,
    dir: TempDir,
    path: std::path::PathBuf,
}

async fn open_env(
    label: &str,
    model: Arc<ScriptedModel>,
    tools: MemTools,
) -> (Driver<Arc<ScriptedModel>, MemTools, ()>, Env) {
    let dir = TempDir::new(label);
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools.clone()));
    let driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    (
        driver,
        Env {
            inbox,
            journal,
            recorder,
            model,
            tools,
            dir,
            path,
        },
    )
}

fn message_meta_id(message: &Message) -> Option<MessageId> {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.id.clone(),
    }
}

// ---------- 4. steer takes effect after the tool batch ----------

#[tokio::test]
async fn steer_applies_after_the_tool_batch() {
    let model = ScriptedModel::new([calls(vec![call("c1", "gate", json!({}))]), text("done")]);
    let tools = MemTools::new();
    let (started, gate) = (Arc::clone(&tools.started), Arc::clone(&tools.gate));
    let (mut driver, env) = open_env("steer", model, tools).await;
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox.followup("go", InputSource::User).await.unwrap();
    // The tool is now running; the steer arrives while the batch is in flight.
    started.notified().await;
    let steer_id = env
        .inbox
        .steer("change direction", InputSource::User)
        .await
        .unwrap();
    gate.notify_one();
    env.recorder.wait_finished().await;
    handle.abort();

    let history = Session::open(&env.path).unwrap().history().unwrap();
    let texts: Vec<String> = history.iter().map(Message::text).collect();
    assert_eq!(
        texts,
        ["go", "", "released", "change direction", "done"],
        "the steer lands after the tool result, before the final answer"
    );
    // And it was recorded: claimed, then admitted at its real transcript index.
    let events = common::events(&env.path);
    let admitted = events
        .iter()
        .find(|(name, _, data)| name == "InputAdmitted" && data["message_id"] == json!(steer_id.0))
        .expect("the steer was admitted");
    assert_eq!(admitted.2["transcript_index"], 3);
}

// ---------- 8. durable event order and index correspondence ----------

#[tokio::test]
async fn durable_event_order_and_index_correspondence() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({"n": 1}))]),
        text("done"),
    ]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("order", model, tools).await;
    let handle = tokio::spawn(async move { driver.run().await });

    let followup_id = env.inbox.followup("go", InputSource::User).await.unwrap();
    // Steer before the turn starts so both inputs are claimed in the same turn.
    let steer_id = env
        .inbox
        .steer("also this", InputSource::User)
        .await
        .unwrap();
    env.recorder.wait_finished().await;
    handle.abort();

    let events = common::events(&env.path);
    let seq = |name: &str, field: &str, value: &str| -> u64 {
        events
            .iter()
            .find(|(n, _, data)| n == name && data[field] == json!(value))
            .map(|(_, s, _)| *s)
            .unwrap_or_else(|| panic!("no {name} with {field}={value} in {events:?}"))
    };
    for id in [&followup_id.0, &steer_id.0] {
        let enqueued = seq("InboxEnqueued", "message_id", id);
        let claimed = events
            .iter()
            .find(|(n, _, data)| {
                n == "InputClaimed"
                    && data["message_ids"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|m| m == &json!(id)))
            })
            .map(|(_, s, _)| *s)
            .unwrap();
        let admitted = seq("InputAdmitted", "message_id", id);
        assert!(
            enqueued < claimed && claimed < admitted,
            "order for {id}: enqueued={enqueued} claimed={claimed} admitted={admitted}"
        );
    }

    // Every admitted message id matches the `MessageAppended` at that exact index.
    let session = Session::open(&env.path).unwrap();
    for event in session.events() {
        if let EventData::InputAdmitted {
            message_id,
            transcript_index,
        } = &event.data
        {
            let found = session.events().iter().find_map(|e| match &e.data {
                EventData::MessageAppended { index, message }
                    if message_meta_id(message).as_ref() == Some(message_id) =>
                {
                    Some(*index)
                }
                _ => None,
            });
            assert_eq!(
                found,
                Some(*transcript_index),
                "InputAdmitted.transcript_index must be the MessageAppended.index"
            );
        }
    }
}

// ---------- 9. configuration applies at the turn boundary ----------

#[tokio::test]
async fn a_config_change_is_not_seen_by_the_turn_that_queued_it() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({}))]),
        text("turn 1 done"),
        text("turn 2 done"),
    ]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("config", Arc::clone(&model), tools).await;
    // On the first model call (turn 1, before the tool batch) queue a change.
    model.watch_config(
        Arc::clone(driver.config_slot()),
        Arc::clone(driver.config_queue()),
        json!({"route": "fast"}),
        0,
    );
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox
        .followup("first", InputSource::User)
        .await
        .unwrap();
    env.inbox
        .followup("second", InputSource::User)
        .await
        .unwrap();
    env.recorder.wait_finished().await;
    // Wait for the second turn too: two RunFinished events.
    while env
        .recorder
        .events()
        .iter()
        .filter(|e| matches!(e, agent_core::AgentEvent::RunFinished { .. }))
        .count()
        < 2
    {
        env.recorder.wait_finished().await;
    }
    handle.abort();

    let seen = model.seen();
    assert_eq!(seen.len(), 3, "{seen:?}");
    // Turn 1: both calls see the initial (null) config.
    assert_eq!(seen[0].1, json!(null), "call 0 is before the change");
    assert_eq!(
        seen[1].1,
        json!(null),
        "call 1 is the same turn: not yet applied"
    );
    // Turn 2 sees it.
    assert_eq!(seen[2].1, json!({"route": "fast"}), "the next turn sees it");
    assert!(seen[2].2 > 0, "the applied revision advanced");
    // And it is durable, at the turn boundary.
    let events = common::events(&env.path);
    assert!(events.iter().any(|(n, _, data)| n == "ConfigChanged"
        && data["revision"] == 1
        && data["change"]["route"] == "fast"));
}

// ---------- the per-turn timeout ----------

#[tokio::test(start_paused = true)]
async fn a_turn_timeout_ends_the_turn_as_deadline_exceeded() {
    let model = ScriptedModel::new([]);
    model.hang_at(0);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("timeout", model, tools).await;
    driver.set_turn_timeout(Some(std::time::Duration::from_secs(1)));
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });

    env.inbox.followup("go", InputSource::User).await.unwrap();
    // Under paused time the deadline fires as soon as the runtime is idle (the model is
    // hanging), so this resolves without a real wait.
    env.recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    let events = common::events(&env.path);
    let finished = events
        .iter()
        .find(|(name, _, _)| name == "TurnFinished")
        .expect("the turn finished durably");
    assert_eq!(finished.2["outcome"], "cancelled");
    assert_eq!(finished.2["reason"], "deadline_exceeded");
}

// ---------- 10. the public API never exposes `ActiveRun` ----------

/// Structural guarantee, not a runtime assertion: `ActiveRun` appears in no public
/// signature of this crate. The driver owns the session and the run; callers only ever
/// touch `Inbox`, `Driver`, `ConfigSlot`/`ConfigQueue` and the outcome values. This test
/// exercises that surface end to end; the type cannot be named from here because it is
/// never returned. (`Step` is imported above only to prove the driver, not the caller,
/// matches on it — see `driver.rs`.)
#[tokio::test]
async fn the_public_api_never_exposes_active_run() {
    let model = ScriptedModel::new([text("ok")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("no-active-run", model, tools).await;
    let handle = tokio::spawn(async move { driver.run().await });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    handle.abort();
    // The only way to observe a turn is its durable record and the live events.
    assert!(
        common::events(&env.path)
            .iter()
            .any(|(n, _, _)| n == "TurnFinished")
    );
    let _: Option<Step> = None; // `Step` exists, but no API here returns one.
}

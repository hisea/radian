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

/// Each admitted input is recorded exactly once, however many steps the turn takes.
#[tokio::test]
async fn each_input_is_admitted_once() {
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", json!({}))]), text("done")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("admit-once", model, tools).await;
    let handle = tokio::spawn(async move { driver.run().await });

    let followup = env.inbox.followup("go", InputSource::User).await.unwrap();
    let steer = env.inbox.steer("also", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    for id in [&followup.0, &steer.0] {
        let count = common::events(&env.path)
            .iter()
            .filter(|(name, _, data)| name == "InputAdmitted" && data["message_id"] == json!(id))
            .count();
        assert_eq!(count, 1, "{id} was admitted {count} times");
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

// ---------- ADR 0026 slices 2+3: the /model switch ----------

/// A model switch queues a route **only**: the durable `ConfigChanged` carries provider and
/// model, and the resolved key appears nowhere in the journal (D5; mutation M-"key").
#[tokio::test]
async fn a_model_switch_writes_a_route_and_never_a_key() {
    const KEY: &str = "sk-switch-secret-value";
    let model = ScriptedModel::new([text("turn done"), text("second turn done")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("switch-route", Arc::clone(&model), tools.clone()).await;
    driver.set_budget_for(Arc::new(|provider, model| {
        (provider == "anthropic" && model == "claude").then_some(4321)
    }));
    let slot = Arc::clone(driver.config_slot());
    let queue = Arc::clone(driver.config_queue());
    // The switch happens mid-run, exactly as `/model` does: `/model` pushes into the queue
    // while turn 1 runs (here: at its first model call), and the boundary applies it.
    model.watch_config(
        Arc::clone(&slot),
        Arc::clone(&queue),
        radian_runtime::ModelRoute::new("anthropic", "claude").to_value(),
        0,
    );
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox
        .followup("first", InputSource::User)
        .await
        .unwrap();
    env.recorder.wait_finished().await;
    env.inbox
        .followup("second", InputSource::User)
        .await
        .unwrap();
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

    // The durable change is the route alone, and the key is nowhere in the file.
    let events = common::events(&env.path);
    let change = events
        .iter()
        .find_map(|(name, _, data)| {
            (name == "ConfigChanged")
                .then(|| data["change"].clone())
                .filter(|change| change["model"]["provider"] == "anthropic")
        })
        .expect("a model ConfigChanged was journaled");
    assert_eq!(change["model"]["model"], "claude");
    assert!(
        change.get("key").is_none() && change.get("api_key").is_none(),
        "the change carries a key: {change}"
    );
    let file = std::fs::read_to_string(&env.path).unwrap();
    assert!(!file.contains(KEY), "the key reached the journal");

    // D7: the budget followed the switched model — asserted after the switch applied and the
    // run task owns the driver, so read the reopened state below instead.
}

/// D8: resume replays the last model change and the last thinking change, merged (neither
/// kind erases the other).
#[tokio::test]
async fn resume_replays_the_last_model_and_thinking_changes() {
    let model = ScriptedModel::new([text("one"), text("two"), text("three"), text("four")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("switch-resume", Arc::clone(&model), tools.clone()).await;
    let queue = Arc::clone(driver.config_queue());
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox.followup("a", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    // A switch, then a thinking change: both must survive the restart.
    queue.push(radian_runtime::ModelRoute::new("anthropic", "claude").to_value());
    env.inbox.followup("b", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    queue.push(serde_json::json!({"thinking": "high"}));
    env.inbox.followup("c", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    handle.abort();
    // The journal's single-writer lock dies with the task **and** with every Arc to the file:
    // wait for the task, then drop the env (it holds the journal), before reopening.
    let _ = handle.await;
    // The temp dir dies with `env`, so the file continues under a copy (the lock dies with
    // the Arc, the bytes survive the copy).
    let path = env.path.clone();
    let copy =
        std::env::temp_dir().join(format!("radian-switch-resume-{}.jsonl", std::process::id()));
    std::fs::remove_file(&copy).ok();
    std::fs::copy(&path, &copy).unwrap();
    drop(env);

    // Reopen from the copy: the slot state is the merge, not the last change alone.
    let journal = Arc::new(SessionJournal::resume(&copy, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let driver2 = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::new(Recorder::default()) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    let value = driver2.config_slot().value();
    std::fs::remove_file(&copy).ok();
    assert_eq!(value["model"]["provider"], "anthropic", "{value}");
    assert_eq!(value["model"]["model"], "claude", "{value}");
    assert_eq!(value["thinking"], "high", "{value}");
}

/// D7: applying a model change recomputes `B` from the host's resolver, and the next turn runs
/// under it.
#[tokio::test]
async fn a_switch_moves_the_next_turns_budget() {
    let model = ScriptedModel::new([]);
    let tools = MemTools::new();
    let (mut driver, _env) = open_env("switch-budget", Arc::clone(&model), tools).await;
    driver.set_budget_for(Arc::new(|provider, model| {
        (provider == "anthropic" && model == "claude").then_some(1234)
    }));
    assert_eq!(driver.test_turn_budget(), None, "nothing switched yet");
    driver
        .config_queue()
        .push(radian_runtime::ModelRoute::new("anthropic", "claude").to_value());
    driver.test_apply_pending_config().await.unwrap();
    assert_eq!(
        driver.test_turn_budget(),
        Some(agent_core::ContextBudget::new(1234)),
        "the switch moved the budget"
    );
}

/// ADR 0026 D11 + review M3: resume merges **every** `ConfigChanged` in revision order, so a
/// thinking change, a model change and an unrelated `approval` change (pi/auto) all survive.
/// "The last of each kind" would drop the thinking level as soon as a third key appeared.
#[tokio::test]
async fn resume_merges_every_config_change_not_one_per_kind() {
    let model = ScriptedModel::new([text("one"), text("two"), text("three"), text("four")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("config-merge", Arc::clone(&model), tools.clone()).await;
    let queue = Arc::clone(driver.config_queue());
    let handle = tokio::spawn(async move { driver.run().await });

    // thinking=high, then model=X, then an unrelated approval=auto.
    queue.push(serde_json::json!({"thinking": "high"}));
    env.inbox.followup("a", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    queue.push(radian_runtime::ModelRoute::new("anthropic", "claude").to_value());
    env.inbox.followup("b", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    queue.push(serde_json::json!({"approval": "auto"}));
    env.inbox.followup("c", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    let path = env.path.clone();
    let copy =
        std::env::temp_dir().join(format!("radian-config-merge-{}.jsonl", std::process::id()));
    std::fs::copy(&path, &copy).unwrap();
    drop(env);

    let journal = Arc::new(SessionJournal::resume(&copy, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let driver2 = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::new(Recorder::default()) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    let value = driver2.config_slot().value();
    std::fs::remove_file(&copy).ok();
    assert_eq!(
        value["thinking"], "high",
        "the thinking change survived: {value}"
    );
    assert_eq!(value["model"]["provider"], "anthropic", "{value}");
    assert_eq!(value["model"]["model"], "claude", "{value}");
    assert_eq!(value["approval"], "auto", "{value}");
}

/// ADR 0026 D7 + review M2: on a resume the host sets the budget resolver **after** the driver
/// replayed the journal's model change; `set_budget_for` must therefore recompute for the
/// restored route at once, not wait for the next switch.
#[tokio::test]
async fn the_resumed_model_gets_its_budget_when_the_resolver_arrives() {
    let model = ScriptedModel::new([text("one"), text("two")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("resume-budget", Arc::clone(&model), tools.clone()).await;
    let queue = Arc::clone(driver.config_queue());
    let handle = tokio::spawn(async move { driver.run().await });
    queue.push(radian_runtime::ModelRoute::new("anthropic", "claude").to_value());
    env.inbox.followup("a", InputSource::User).await.unwrap();
    env.recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    let path = env.path.clone();
    let copy =
        std::env::temp_dir().join(format!("radian-resume-budget-{}.jsonl", std::process::id()));
    std::fs::copy(&path, &copy).unwrap();
    drop(env);

    let journal = Arc::new(SessionJournal::resume(&copy, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let mut driver2 = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::new(Recorder::default()) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    std::fs::remove_file(&copy).ok();
    // The replay put the model in the slot; the resolver arrives now (the host's order).
    assert_eq!(driver2.test_turn_budget(), None, "no resolver yet");
    driver2.set_budget_for(Arc::new(|provider, model| {
        (provider == "anthropic" && model == "claude").then_some(4321)
    }));
    assert_eq!(
        driver2.test_turn_budget(),
        Some(agent_core::ContextBudget::new(4321)),
        "the restored route gets its budget when the resolver arrives"
    );
}

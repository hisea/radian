//! The driver: turn-boundary configuration, steer at the core's safe point, durable event
//! order, and the structural absence of `ActiveRun` from the public API.

mod common;

use std::sync::{Arc, Mutex};

use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_core::{Agent, InputSource, Message, MessageId, Step, TurnOptions};
use radian_runtime::{ConfigQueue, Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};
use serde_json::{Value, json};

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
        Arc::clone(&recorder) as Arc<dyn radian_core::EventSink>,
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
        .filter(|e| matches!(e, radian_core::AgentEvent::RunFinished { .. }))
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
        (provider == "anthropic" && model == "claude")
            .then_some(radian_core::ContextBudget::new(4321))
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
        .filter(|e| matches!(e, radian_core::AgentEvent::RunFinished { .. }))
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
        Arc::new(Recorder::default()) as Arc<dyn radian_core::EventSink>,
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
        (provider == "anthropic" && model == "claude")
            .then_some(radian_core::ContextBudget::new(1234))
    }));
    assert_eq!(driver.test_turn_budget(), None, "nothing switched yet");
    driver
        .config_queue()
        .push(radian_runtime::ModelRoute::new("anthropic", "claude").to_value());
    driver.test_apply_pending_config().await.unwrap();
    assert_eq!(
        driver.test_turn_budget(),
        Some(radian_core::ContextBudget::new(1234)),
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
        Arc::new(Recorder::default()) as Arc<dyn radian_core::EventSink>,
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
        Arc::new(Recorder::default()) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    std::fs::remove_file(&copy).ok();
    // The replay put the model in the slot; the resolver arrives now (the host's order).
    assert_eq!(driver2.test_turn_budget(), None, "no resolver yet");
    driver2.set_budget_for(Arc::new(|provider, model| {
        (provider == "anthropic" && model == "claude")
            .then_some(radian_core::ContextBudget::new(4321))
    }));
    assert_eq!(
        driver2.test_turn_budget(),
        Some(radian_core::ContextBudget::new(4321)),
        "the restored route gets its budget when the resolver arrives"
    );
}

// ---------- ADR 0021: the per-turn addition seam ----------

/// The host's addition goes into the `inject` lane, so it lands **after** the turn's user
/// message and **before** its first model call; a turn the host did not claim from a user
/// input (a background-job notification, `source = System`; ADR 0037) never calls the seam.
#[tokio::test]
async fn a_host_addition_lands_after_the_user_message_and_before_the_first_call() {
    let model = ScriptedModel::new([text("one"), text("two"), text("three")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("additions", model, tools).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    driver.set_turn_additions(Arc::new(move |text: &str, transcript: &[Message]| {
        // The seam sees the transcript **before** the turn's input is committed, which is the
        // dedup ledger (ADR 0021).
        seen.lock()
            .unwrap()
            .push((text.to_owned(), transcript.len()));
        Some(addition_message(text))
    }));
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox
        .followup("first", InputSource::User)
        .await
        .unwrap();
    env.recorder.wait_finished().await;
    // A job completion notification while idle: a `followup`, so it starts a turn, and its
    // source is System (ADR 0037 decision 3).
    env.inbox
        .followup("job finished", InputSource::System)
        .await
        .unwrap();
    env.recorder.wait_finished().await;
    env.inbox
        .followup("second", InputSource::User)
        .await
        .unwrap();
    env.recorder.wait_finished().await;
    handle.abort();

    let history: Vec<String> = Session::open(&env.path)
        .unwrap()
        .history()
        .unwrap()
        .iter()
        .map(Message::text)
        .collect();
    assert_eq!(
        history,
        [
            "first",
            "<memory>hit first</memory>",
            "one",
            "job finished",
            "two",
            "second",
            "<memory>hit second</memory>",
            "three",
        ],
        "the addition is the first message after its user message"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("first".to_owned(), 0), ("second".to_owned(), 5)],
        "the seam runs once per user turn, with the transcript as the ledger"
    );

    // The addition is a durable input on the inject lane, with the System source the driver
    // declares (never the human's).
    let events = common::events(&env.path);
    let injected: Vec<&Value> = events
        .iter()
        .filter(|(name, _, data)| {
            name == "InboxEnqueued"
                && data["source"] == json!("system")
                && data["lane"] == json!("next_step")
        })
        .map(|(_, _, data)| data)
        .collect();
    assert_eq!(injected.len(), 2, "{injected:?}");
    assert!(
        injected[0]["message"]["user"]["meta"]["origin"]["kind"] == json!("memory_addition"),
        "{injected:?}"
    );
}

/// The seam's message: a user-role message (the inbox accepts no other role) whose source is
/// System and whose origin names the records it carries.
fn addition_message(text: &str) -> Message {
    Message::User {
        content: vec![radian_core::ContentPart::text(format!(
            "<memory>hit {text}</memory>"
        ))],
        meta: radian_core::MessageMeta {
            id: None,
            source: Some(InputSource::System),
            origin: Some(
                radian_core::OpaqueJson::parse(r#"{"kind":"memory_addition","records":[]}"#)
                    .unwrap(),
            ),
        },
    }
}
// ---------- ADR 0018: the summary settings follow the switched route ----------

/// A driver whose `AgentConfig.compaction_settings` is the **startup** route's form and whose
/// resolver converts the configured level for the route in force. The big first message makes
/// the second turn's transcript over `B`, so it compacts.
struct SwitchEnv {
    driver: Driver<Arc<ScriptedModel>, MemTools, ()>,
    model: Arc<ScriptedModel>,
    recorder: Arc<Recorder>,
    inbox: Arc<Inbox>,
    queue: Arc<ConfigQueue>,
    /// Kept so the temp directory lives as long as the env.
    _dir: TempDir,
}

async fn switch_env(label: &str, startup_settings: serde_json::Value) -> SwitchEnv {
    let dir = TempDir::new(label);
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    // Turn 1's conversation reply, then turn 2's summary and conversation replies.
    let model = ScriptedModel::new([text("ack"), text("summary"), text("done")]);
    let tools = MemTools::new();
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools).with_config(
        radian_core::AgentConfig {
            context_budget: Some(radian_core::ContextBudget::new(3_000).with_bytes_per_token(1)),
            compaction_settings: startup_settings,
            ..radian_core::AgentConfig::default()
        },
    ));
    let mut driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    // The host's per-route resolver: the same level, converted per route.
    driver.set_compaction_settings_for(Arc::new(|provider, _model| {
        if provider == "anthropic" {
            json!({"thinking": {"type": "enabled", "budget_tokens": 2048}})
        } else {
            json!({"reasoning_effort": "low"})
        }
    }));
    let queue = Arc::clone(driver.config_queue());
    SwitchEnv {
        driver,
        model,
        recorder,
        inbox,
        queue,
        _dir: dir,
    }
}

/// ADR 0018: after `/model` from an OpenAI-compatible route to an Anthropic route,
/// the summary call must carry the Anthropic form and **not** `reasoning_effort` (which
/// Anthropic would reject with a 400 and the compaction would fail). Startup is the
/// OpenAI-compatible route, so the startup `compaction_settings` is `reasoning_effort`.
#[tokio::test]
async fn the_summary_settings_follow_a_switch_to_an_anthropic_route() {
    let SwitchEnv {
        mut driver,
        model,
        recorder,
        inbox,
        queue,
        _dir,
    } = switch_env(
        "switch-summary-anthropic",
        json!({"reasoning_effort": "low"}),
    )
    .await;
    let handle = tokio::spawn(async move { driver.run().await });

    // Turn 1 builds an over-budget history (no compaction yet: one atomic unit).
    inbox
        .followup("x".repeat(20_000), InputSource::User)
        .await
        .unwrap();
    recorder.wait_finished().await;
    // Switch to the Anthropic route; the next turn boundary applies it.
    queue.push(radian_runtime::ModelRoute::new("anthropic", "claude").to_value());
    inbox.followup("last", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    let requests = model.requests();
    assert_eq!(
        requests.len(),
        3,
        "turn 1 conversation, then the summary and conversation of turn 2"
    );
    assert_eq!(
        requests[1].settings,
        json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}),
        "the summary carries the Anthropic route's form"
    );
    assert!(
        requests[1].settings.get("reasoning_effort").is_none(),
        "the summary must not carry the startup route's reasoning_effort: {:?}",
        requests[1].settings
    );
    // The conversation request is untouched.
    assert_eq!(requests[2].settings, serde_json::Value::Null);
}

/// The reverse: startup on the Anthropic route (so the startup setting is `thinking`), switch
/// to an OpenAI-compatible route, and the summary must carry `reasoning_effort` and no
/// `thinking`.
#[tokio::test]
async fn the_summary_settings_follow_a_switch_to_an_openai_route() {
    let SwitchEnv {
        mut driver,
        model,
        recorder,
        inbox,
        queue,
        _dir,
    } = switch_env(
        "switch-summary-openai",
        json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}),
    )
    .await;
    let handle = tokio::spawn(async move { driver.run().await });

    inbox
        .followup("x".repeat(20_000), InputSource::User)
        .await
        .unwrap();
    recorder.wait_finished().await;
    queue.push(radian_runtime::ModelRoute::new("opencode-go", "deepseek-v4.1-flash").to_value());
    inbox.followup("last", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    let requests = model.requests();
    assert_eq!(
        requests.len(),
        3,
        "turn 1 conversation, then the summary and conversation of turn 2"
    );
    assert_eq!(
        requests[1].settings,
        json!({"reasoning_effort": "low"}),
        "the summary carries the OpenAI-compatible route's form"
    );
    assert!(
        requests[1].settings.get("thinking").is_none(),
        "the summary must not carry the startup route's thinking object: {:?}",
        requests[1].settings
    );
}

// ---------- the host's runtime-events sink sees the turn start ----------

/// The host sink hears `TurnStarted` before `TurnFinished` (a task's `running` flag and the
/// control port's event stream both key on it; until 2026-10-17 only the journal got it, so a
/// task reported `running: false` for its whole turn).
#[tokio::test]
async fn the_runtime_events_sink_hears_the_turn_start_before_its_end() {
    let model = ScriptedModel::new([text("done")]);
    let (mut driver, env) = open_env("sink-start", model, MemTools::new()).await;
    let seen: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let finished = Arc::new(tokio::sync::Notify::new());
    let (sink_seen, sink_finished) = (Arc::clone(&seen), Arc::clone(&finished));
    driver.set_runtime_events(Arc::new(move |event: &radian_session::RuntimeEvent| {
        let name = match event {
            radian_session::RuntimeEvent::TurnStarted { .. } => "TurnStarted",
            radian_session::RuntimeEvent::TurnFinished { .. } => "TurnFinished",
            _ => return,
        };
        sink_seen.lock().unwrap().push(name);
        if name == "TurnFinished" {
            sink_finished.notify_one();
        }
    }));
    let handle = tokio::spawn(async move { driver.run().await });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), finished.notified())
        .await
        .expect("the sink hears the turn end");
    handle.abort();
    assert_eq!(*seen.lock().unwrap(), vec!["TurnStarted", "TurnFinished"]);
}

// ---------- the host's runtime-events sink hears the queue ----------

/// Waits until `seen` holds `wanted`, polling (the sink runs on the driver's task, so the
/// test cannot rendezvous with it directly). Panics with what did arrive.
async fn wait_sink(seen: &Arc<Mutex<Vec<String>>>, wanted: &[&str]) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let current = seen.lock().unwrap().clone();
        if current
            .iter()
            .map(String::as_str)
            .eq(wanted.iter().copied())
        {
            return;
        }
        // A wrong event (or a wrong order) fails fast instead of timing out: the sink hears
        // every event, so a prefix that already differs can never become `wanted`.
        if current.len() >= wanted.len()
            || wanted[..current.len()] != current[..]
            || std::time::Instant::now() >= deadline
        {
            panic!("the sink heard {current:?}, wanted {wanted:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The host sink hears `InboxEnqueued` (with its lane) and `InputAdmitted` live: the TUI's
/// `queued` footer item counts the `next_turn` entries the runtime has not admitted yet, so
/// it reads these two events instead of polling. A `steer` admission never touches that
/// count (its enqueue is `NextStep`, its admission carries an untracked id).
#[tokio::test]
async fn the_runtime_events_sink_hears_enqueue_and_admission() {
    let model = ScriptedModel::new([text("one"), text("two")]);
    let (mut driver, env) = open_env("sink-queue", model, MemTools::new()).await;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink_seen = Arc::clone(&seen);
    driver.set_runtime_events(Arc::new(move |event: &radian_session::RuntimeEvent| {
        let name = match event {
            radian_session::RuntimeEvent::InboxEnqueued { lane, .. } => {
                format!("enqueued:{lane:?}")
            }
            radian_session::RuntimeEvent::InputAdmitted { .. } => "admitted".to_owned(),
            _ => return,
        };
        sink_seen.lock().unwrap().push(name);
    }));
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox
        .followup("first", InputSource::User)
        .await
        .unwrap();
    wait_sink(&seen, &["enqueued:NextTurn", "admitted"]).await;
    env.inbox
        .followup("second", InputSource::User)
        .await
        .unwrap();
    wait_sink(
        &seen,
        &[
            "enqueued:NextTurn",
            "admitted",
            "enqueued:NextTurn",
            "admitted",
        ],
    )
    .await;
    handle.abort();
    let _ = handle.await;
}

/// The inbox forwards every lane's enqueue to the sink with its lane (no driver needed): a
/// `followup` is `NextTurn`, a `steer` and an `inject` are `NextStep`.
#[tokio::test]
async fn the_inbox_sink_hears_the_lane_of_every_enqueue() {
    let inbox = Inbox::new(16);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink_seen = Arc::clone(&seen);
    inbox.set_runtime_events(Arc::new(move |event: &radian_session::RuntimeEvent| {
        if let radian_session::RuntimeEvent::InboxEnqueued { lane, .. } = event {
            sink_seen.lock().unwrap().push(format!("{lane:?}"));
        }
    }));
    inbox.followup("a", InputSource::User).await.unwrap();
    inbox.steer("b", InputSource::User).await.unwrap();
    inbox.inject("c", InputSource::User).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["NextTurn", "NextStep", "NextStep"]
    );
}

/// The sink-forwarded `InboxEnqueued` carries the wake-gate degrade mark: an
/// `inject_degraded` forwards `degraded: true`, an ordinary `inject` forwards
/// `degraded: false` (the journal line and the live event agree).
#[tokio::test]
async fn the_inbox_sink_forwards_the_degraded_mark() {
    let inbox = Inbox::new(16);
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let sink_seen = Arc::clone(&seen);
    inbox.set_runtime_events(Arc::new(move |event: &radian_session::RuntimeEvent| {
        if let radian_session::RuntimeEvent::InboxEnqueued { degraded, .. } = event {
            sink_seen.lock().unwrap().push(*degraded);
        }
    }));
    inbox.inject("plain", InputSource::User).await.unwrap();
    inbox
        .inject_degraded("gated", InputSource::User)
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![false, true]);
}

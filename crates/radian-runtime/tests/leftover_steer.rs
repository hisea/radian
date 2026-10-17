//! ADR 0011: a waking `next_step` entry (a `steer`) left in the inbox when no
//! turn is running opens the next turn itself, as its first input. An `inject` never does.
//!
//! The four acceptance cases: a steer left during the last model call of a turn; an idle
//! inject waits for a followup; a resume with a durable waking entry starts work on open; an
//! old journal (no `wake` field) keeps today's behaviour.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::{Agent, EventSink, InputSource, Message, TurnOptions};
use common::{MemTools, Recorder, ScriptedModel, TempDir, created, text};
use radian_runtime::{Driver, Inbox, RuntimeEvents};
use radian_session::{EventData, RuntimeEvent, Session, SessionJournal};
use serde_json::{Value, json};

struct Env {
    inbox: Arc<Inbox>,
    path: std::path::PathBuf,
    /// Kept so the temp directory lives as long as the env.
    _dir: TempDir,
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
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    (
        driver,
        Env {
            inbox,
            path,
            _dir: dir,
        },
    )
}

/// The host's runtime-events view of the turn boundaries. `TurnStarted` and `TurnFinished`
/// reach this sink **after** their journal append, so a wait on it also guarantees the
/// durable record is on disk (no file polling, no sleep).
#[derive(Default)]
struct TurnSink {
    started: Mutex<Vec<String>>,
    finished: Mutex<Vec<String>>,
    signal: tokio::sync::Notify,
}

impl TurnSink {
    fn install(driver: &mut Driver<Arc<ScriptedModel>, MemTools, ()>) -> Arc<Self> {
        let sink = Arc::new(Self::default());
        driver.set_runtime_events(Arc::clone(&sink) as Arc<dyn RuntimeEvents>);
        sink
    }

    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }

    async fn wait_finished(&self, n: usize) {
        while self.finished.lock().unwrap().len() < n {
            self.signal.notified().await;
        }
    }
}

impl RuntimeEvents for TurnSink {
    fn on_runtime_event(&self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::TurnStarted { turn_id } => {
                self.started.lock().unwrap().push(turn_id.clone());
            }
            RuntimeEvent::TurnFinished { turn_id, .. } => {
                self.finished.lock().unwrap().push(turn_id.clone());
                self.signal.notify_one();
            }
            _ => {}
        }
    }
}

/// The `seq` of the first event named `event` whose `data` satisfies `pred`.
fn seq_of(events: &[(String, u64, Value)], event: &str, pred: impl Fn(&Value) -> bool) -> u64 {
    events
        .iter()
        .find(|(name, _, data)| name == event && pred(data))
        .map(|(_, seq, _)| *seq)
        .unwrap_or_else(|| panic!("no {event} matching in {events:?}"))
}

fn count_of(events: &[(String, u64, Value)], event: &str, pred: impl Fn(&Value) -> bool) -> usize {
    events
        .iter()
        .filter(|(name, _, data)| name == event && pred(data))
        .count()
}

fn history(path: &Path) -> Vec<String> {
    Session::open(path)
        .unwrap()
        .history()
        .unwrap()
        .iter()
        .map(Message::text)
        .collect()
}

// ---------- 1. a steer left during the turn's last model call opens the next turn ----------

/// The mock model parks in its last call (a deterministic hold, not a sleep); the steer is
/// enqueued while it is parked, so it is left over when the turn ends. It must open a new
/// turn as its first input — on the old code the driver slept and no second turn ran.
#[tokio::test]
async fn a_waking_steer_left_at_the_end_of_a_turn_opens_the_next_one() {
    let model = ScriptedModel::new([text("first answer"), text("second answer")]);
    model.park_at(0);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("leftover-steer", Arc::clone(&model), tools).await;
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox.followup("go", InputSource::User).await.unwrap();
    // The turn's only model call is parked: the step loop cannot drain `next_step` again
    // before the turn ends, so the entry below is left over exactly as in the incident.
    model.wait_parked().await;
    let steer_id = env
        .inbox
        .steer("change direction", InputSource::User)
        .await
        .unwrap();
    model.release();

    tokio::time::timeout(Duration::from_secs(10), sink.wait_finished(2))
        .await
        .expect("the leftover steer must open a second turn (old code: the driver sleeps)");
    handle.abort();
    let _ = handle.await;

    assert_eq!(
        sink.started(),
        ["t-1", "t-2"],
        "a new turn, not a continuation of the first"
    );
    assert_eq!(
        history(&env.path),
        ["go", "first answer", "change direction", "second answer"],
        "the steer is the new turn's first input"
    );

    // The durable order (ADR 0011 + L3): the first turn is finished
    // before the steer is claimed, and the new turn starts before the steer is admitted.
    let events = common::events(&env.path);
    let steer = json!(steer_id.0);
    let finished1 = seq_of(&events, "TurnFinished", |d| d["turn_id"] == json!("t-1"));
    let claimed = seq_of(&events, "InputClaimed", |d| {
        d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&steer))
    });
    let started2 = seq_of(&events, "TurnStarted", |d| d["turn_id"] == json!("t-2"));
    let admitted = seq_of(&events, "InputAdmitted", |d| d["message_id"] == steer);
    assert!(
        finished1 < claimed && claimed < started2 && started2 < admitted,
        "order: finished1={finished1} claimed={claimed} started2={started2} admitted={admitted}"
    );
    assert_eq!(
        count_of(&events, "InputClaimed", |d| d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&steer))),
        1,
        "the steer is claimed exactly once"
    );
    assert_eq!(
        count_of(&events, "InputAdmitted", |d| d["message_id"] == steer),
        1
    );
}

// ---------- 2. an idle inject never opens a turn ----------

/// With no turn running, an `inject` waits (it is not a wake): no turn starts, and a later
/// `followup`'s turn absorbs it at its first step. Paused time makes the "no turn" half
/// deterministic: the clock advances only once every task has reached a wait.
#[tokio::test(start_paused = true)]
async fn an_idle_inject_does_not_open_a_turn_and_is_absorbed_by_the_next_followup() {
    let model = ScriptedModel::new([text("answer")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env("idle-inject", Arc::clone(&model), tools).await;
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    let inject_id = env
        .inbox
        .inject("context", InputSource::System)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        model.requests().is_empty(),
        "an inject must not start a turn: {:?}",
        model.requests().len()
    );
    assert!(sink.started().is_empty(), "no turn started while idle");

    env.inbox.followup("go", InputSource::User).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), sink.wait_finished(1))
        .await
        .expect("the followup's turn runs");
    handle.abort();
    let _ = handle.await;

    assert_eq!(
        history(&env.path),
        ["go", "context", "answer"],
        "the inject is absorbed after the turn's user message, before its first call"
    );
    let events = common::events(&env.path);
    let inject = json!(inject_id.0);
    let started = seq_of(&events, "TurnStarted", |d| d["turn_id"] == json!("t-1"));
    let claimed = seq_of(&events, "InputClaimed", |d| {
        d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&inject))
    });
    let admitted = seq_of(&events, "InputAdmitted", |d| d["message_id"] == inject);
    assert!(
        started < claimed && claimed < admitted,
        "the inject is claimed and admitted inside the followup's turn"
    );
}

// ---------- 3. a resume with a durable waking entry starts work on open ----------

/// A journal that ends with a `wake = true` `next_step` `InboxEnqueued` and no `InputClaimed`
/// (the writer produces it; no driver ever ran): opening the session starts a turn
/// immediately with that entry as its first input.
#[tokio::test]
async fn a_resumed_waking_next_step_entry_opens_a_turn_on_open() {
    let dir = TempDir::new("resume-waking-steer");
    let path = dir.file("s.jsonl");
    let steer_id = {
        let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
        let inbox = Inbox::durable(16, Arc::clone(&journal));
        inbox.steer("leftover", InputSource::User).await.unwrap()
        // The journal and the inbox drop here, releasing the single-writer lock.
    };

    // The writer recorded `wake = true`; the reader reads it back.
    let session = Session::open(&path).unwrap();
    let wake = session
        .events()
        .iter()
        .find_map(|event| match &event.data {
            EventData::InboxEnqueued { wake, .. } => Some(*wake),
            _ => None,
        })
        .expect("the journal holds the steering entry");
    assert!(wake, "a `steer` entry is waking on disk");

    let journal = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let model = ScriptedModel::new([text("answer")]);
    let tools = MemTools::new();
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    tokio::time::timeout(Duration::from_secs(10), sink.wait_finished(1))
        .await
        .expect("a resumed waking entry must open a turn on open");
    handle.abort();
    let _ = handle.await;

    assert_eq!(history(&path), ["leftover", "answer"]);
    let events = common::events(&path);
    let started = seq_of(&events, "TurnStarted", |d| d["turn_id"] == json!("t-1"));
    let admitted = seq_of(&events, "InputAdmitted", |d| {
        d["message_id"] == json!(steer_id.0)
    });
    assert!(
        started < admitted,
        "the new turn starts before the steer is admitted"
    );
}

// ---------- 4. an old journal keeps today's behaviour ----------

/// An old journal has no `wake` field. A `next_step` entry then reads as an `inject`: it does
/// not open a turn on open, exactly as before the revision.
#[tokio::test(start_paused = true)]
async fn an_old_journal_next_step_entry_without_wake_does_not_open_a_turn() {
    let dir = TempDir::new("old-journal-steer");
    let path = dir.file("s.jsonl");
    write_old_journal(&path);

    // The reader derives the missing field from the lane: `next_step` is not waking.
    let session = Session::open(&path).unwrap();
    let wake = session
        .events()
        .iter()
        .find_map(|event| match &event.data {
            EventData::InboxEnqueued { wake, .. } => Some(*wake),
            _ => None,
        })
        .expect("the fixture has an InboxEnqueued");
    assert!(!wake, "an old `next_step` entry reads as an inject");

    let journal = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let model = ScriptedModel::new([text("must not run")]);
    let tools = MemTools::new();
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        model.requests().is_empty(),
        "an old journal's next_step entry must not open a turn"
    );
    assert!(sink.started().is_empty(), "no turn started on open");
    handle.abort();
    let _ = handle.await;
}

/// A two-line session file in the pre-revision shape: `SessionCreated` then an
/// `InboxEnqueued` with **no** `wake` field, lane `next_step`.
fn write_old_journal(path: &Path) {
    let lines = [
        json!({
            "schema_version": 1, "session_id": "sid", "seq": 0,
            "ts": "2026-10-17T00:00:00.000000Z", "event": "SessionCreated", "critical": true,
            "data": {
                "workspace": "/work/example",
                "config_snapshot": {"model": {"provider": "example", "model": "m-1"}},
            },
        }),
        json!({
            "schema_version": 1, "session_id": "sid", "seq": 1,
            "ts": "2026-10-17T00:00:01.000000Z", "event": "InboxEnqueued", "critical": true,
            "data": {
                "message": {
                    "user": {
                        "content": [{"text": {"text": "leftover"}}],
                        "meta": {"id": "inbox-1", "source": "user"},
                    },
                },
                "message_id": "inbox-1",
                "lane": "next_step",
                "source": "user",
            },
        }),
    ];
    let body: Vec<String> = lines.iter().map(Value::to_string).collect();
    std::fs::write(path, format!("{}\n", body.join("\n"))).unwrap();
}

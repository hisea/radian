//! ADR 0011: a job completion notice enqueued while a turn runs waits for a
//! step boundary; stranded past the turn end it opens the next turn itself. A plain
//! `inject` in the same window never opens one, and a notice absorbed mid-turn never
//! opens another.
//!
//! The stranding window is the driver's `commit_delay` test hook (the gap between the
//! core's `RunFinished` and the journaled `TurnFinished`): the test enqueues while the
//! turn is provably in that gap, never by sleeping.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_core::{Agent, EventSink, InputSource, Message, MessageId, TurnOptions};
use radian_runtime::{Driver, Inbox, InboxEntry, Lane, RuntimeEvents};
use radian_session::{EventData, RuntimeEvent, Session, SessionJournal};
use serde_json::{Value, json};

struct Env {
    inbox: Arc<Inbox>,
    path: std::path::PathBuf,
    promotions: Arc<Mutex<Vec<String>>>,
    /// Kept so the temp directory lives as long as the env.
    _dir: TempDir,
}

async fn open_env(
    label: &str,
    model: Arc<ScriptedModel>,
    tools: MemTools,
    commit_delay: Option<Duration>,
) -> (Driver<Arc<ScriptedModel>, MemTools, ()>, Env, Arc<Recorder>) {
    let dir = TempDir::new(label);
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let promotions: Arc<Mutex<Vec<String>>> = Arc::default();
    let promotions_in = Arc::clone(&promotions);
    inbox.set_pending_wake_callback(Some(Arc::new(move |id: MessageId| {
        promotions_in.lock().unwrap().push(id.0.clone());
    })));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), tools));
    let mut driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    driver.set_commit_delay(commit_delay);
    (
        driver,
        Env {
            inbox,
            path,
            promotions,
            _dir: dir,
        },
        recorder,
    )
}

/// The host's runtime-events view of the turn boundaries. `TurnStarted` and `TurnFinished`
/// reach this sink **after** their journal append, so a wait on it also guarantees the
/// durable record is on disk (no file polling, no sleep).
#[derive(Default)]
struct TurnSink {
    started: Mutex<Vec<String>>,
    finished: Mutex<Vec<String>>,
    /// `(message id, turn id)` for every `InputClaimed` the driver forwarded.
    claimed: Mutex<Vec<(String, String)>>,
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

    /// The turn the host was told claimed `message`, or `None`.
    fn claimed_turn(&self, message: &str) -> Option<String> {
        self.claimed
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == message)
            .map(|(_, turn)| turn.clone())
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
            RuntimeEvent::InputClaimed {
                message_ids,
                turn_id,
            } => {
                let mut claimed = self.claimed.lock().unwrap();
                for id in message_ids {
                    claimed.push((id.0.clone(), turn_id.clone()));
                }
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

/// A notice stranded in the commit gap opens the next turn itself, as its first input —
/// the incident's journal sequence, replayed deterministically. The promotion signal fires
/// exactly once (the wake-gate count).
#[tokio::test]
async fn a_stranded_pending_wake_opens_the_next_turn() {
    let model = ScriptedModel::new([text("one"), text("two")]);
    let (mut driver, env, recorder) = open_env(
        "stranded-opens",
        Arc::clone(&model),
        MemTools::new(),
        Some(Duration::from_secs(2)),
    )
    .await;
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox.followup("go", InputSource::User).await.unwrap();
    // The turn's answer is done (`RunFinished`), but the journaled `TurnFinished` is
    // still `commit_delay` away: this enqueue lands exactly in the incident's window.
    recorder.wait_finished().await;
    let notice = env
        .inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(20), sink.wait_finished(2))
        .await
        .expect("the stranded notice must open a second turn");
    handle.abort();
    let _ = handle.await;

    assert_eq!(
        sink.started(),
        ["t-1", "t-2"],
        "a new turn, not a continuation of the first"
    );
    assert_eq!(
        history(&env.path),
        ["go", "one", "notice", "two"],
        "the notice is the new turn's first input"
    );
    assert_eq!(
        *env.promotions.lock().unwrap(),
        vec![notice.0.clone()],
        "the opening counts exactly one wake"
    );

    // The durable order (ADR 0011 + L3): the first turn is finished before
    // the notice is claimed, and the new turn starts before the notice is admitted.
    let events = common::events(&env.path);
    let notice_json = json!(notice.0);
    let finished1 = seq_of(&events, "TurnFinished", |d| d["turn_id"] == json!("t-1"));
    let claimed = seq_of(&events, "InputClaimed", |d| {
        d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&notice_json))
    });
    let started2 = seq_of(&events, "TurnStarted", |d| d["turn_id"] == json!("t-2"));
    let admitted = seq_of(&events, "InputAdmitted", |d| d["message_id"] == notice_json);
    assert!(
        finished1 < claimed && claimed < started2 && started2 < admitted,
        "order: finished1={finished1} claimed={claimed} started2={started2} admitted={admitted}"
    );
    assert_eq!(
        count_of(&events, "InputClaimed", |d| d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&notice_json))),
        1,
        "the notice is claimed exactly once"
    );
}

/// A plain `inject` stranded in the same gap never opens a turn: after the first turn
/// ends the driver stays idle, and a later `followup` absorbs the inject in its turn.
/// Mutation: `claim_next_turn` treats every `next_step` entry as opening → a second turn
/// starts before the probe and the `started` assertion fails.
#[tokio::test]
async fn a_stranded_plain_inject_does_not_open_a_turn() {
    let model = ScriptedModel::new([text("one"), text("two")]);
    let (mut driver, env, recorder) = open_env(
        "stranded-inject",
        Arc::clone(&model),
        MemTools::new(),
        Some(Duration::from_secs(2)),
    )
    .await;
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox.followup("go", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    let context = env
        .inbox
        .inject("context", InputSource::System)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(20), sink.wait_finished(1))
        .await
        .expect("the first turn ends");
    assert_eq!(
        sink.started(),
        ["t-1"],
        "a stranded plain inject opens no turn on its own"
    );

    // A later followup opens the next turn and absorbs the inject at its first step.
    env.inbox
        .followup("probe", InputSource::User)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), sink.wait_finished(2))
        .await
        .expect("the probe's turn runs");
    handle.abort();
    let _ = handle.await;

    assert_eq!(sink.started(), ["t-1", "t-2"]);
    assert_eq!(
        history(&env.path),
        ["go", "one", "probe", "context", "two"],
        "the probe opens the turn; the inject joins after it, before its first call"
    );
    assert!(
        env.promotions.lock().unwrap().is_empty(),
        "no promotion fires for a plain inject"
    );
    let events = common::events(&env.path);
    let context_json = json!(context.0);
    let started = seq_of(&events, "TurnStarted", |d| d["turn_id"] == json!("t-2"));
    let claimed = seq_of(&events, "InputClaimed", |d| {
        d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&context_json))
    });
    let admitted = seq_of(&events, "InputAdmitted", |d| {
        d["message_id"] == context_json
    });
    assert!(
        started < claimed && claimed < admitted,
        "the inject is claimed and admitted inside the probe's turn"
    );
}

/// A pending-wake notice absorbed mid-turn never opens another turn: the running turn's
/// next step takes it, and the turn after is only the probe's.
/// Mutation: an absorbed-but-unadmitted entry is requeued with its flag (or the claim
/// ignores admission) → a second turn opens for the notice and `started` grows.
#[tokio::test]
async fn an_absorbed_pending_wake_does_not_open_another_turn() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "gate", json!({}))]),
        text("done"),
        text("two"),
    ]);
    let tools = MemTools::new();
    let (started, gate) = (Arc::clone(&tools.started), Arc::clone(&tools.gate));
    let (mut driver, env, _) = open_env("absorbed-pending", Arc::clone(&model), tools, None).await;
    let sink = TurnSink::install(&mut driver);
    let handle = tokio::spawn(async move { driver.run().await });

    env.inbox.followup("go", InputSource::User).await.unwrap();
    // The tool is now running; the notice arrives mid-turn and is absorbed by the next
    // step, exactly like an `inject`.
    started.notified().await;
    let notice = env
        .inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();
    gate.notify_one();

    // Without any other input the driver goes idle: no second turn for the notice.
    env.inbox
        .followup("probe", InputSource::User)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), sink.wait_finished(2))
        .await
        .expect("both turns run");
    handle.abort();
    let _ = handle.await;

    assert_eq!(sink.started(), ["t-1", "t-2"]);
    assert_eq!(
        history(&env.path),
        ["go", "", "released", "notice", "done", "probe", "two"],
        "the notice lands after the tool result, inside the first turn"
    );
    assert!(
        env.promotions.lock().unwrap().is_empty(),
        "an absorbed notice counts no wake"
    );
    let events = common::events(&env.path);
    let notice_json = json!(notice.0);
    let claimed_turn = events
        .iter()
        .find(|(name, _, data)| {
            name == "InputClaimed"
                && data["message_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.contains(&notice_json))
        })
        .map(|(_, _, data)| data["turn_id"].clone())
        .expect("the notice is claimed");
    assert_eq!(
        claimed_turn,
        json!("t-1"),
        "the notice is claimed inside the running turn, not for a new one"
    );
    // The host hears the merged claim too (ADR 0049): a host waiting on its own message must
    // learn which turn took it, and this one was taken mid-turn by `drain_steering`.
    assert_eq!(
        sink.claimed_turn(&notice.0),
        Some("t-1".to_owned()),
        "the merged claim reaches the runtime-event sink"
    );
    assert_eq!(
        count_of(&events, "InputClaimed", |d| d["message_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&notice_json))),
        1,
        "the notice is claimed exactly once"
    );
}

/// A journal that ends with a `pending_wake` `next_step` `InboxEnqueued` and no
/// `InputClaimed` (the writer produces it; no driver ever ran): opening the session
/// recovers the flag, and the driver starts work immediately with that entry as its
/// first input — the durable half of the stranding fix.
#[tokio::test]
async fn a_resumed_pending_wake_entry_opens_a_turn_on_open() {
    let dir = TempDir::new("resume-pending-wake");
    let path = dir.file("s.jsonl");
    let notice = {
        let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
        let inbox = Inbox::durable(16, Arc::clone(&journal));
        inbox
            .inject_pending_wake("notice", InputSource::System)
            .await
            .unwrap()
        // The journal and the inbox drop here, releasing the single-writer lock.
    };

    // The writer recorded the pending wake; the reader reads it back.
    let session = Session::open(&path).unwrap();
    let pending_wake = session
        .events()
        .iter()
        .find_map(|event| match &event.data {
            EventData::InboxEnqueued { pending_wake, .. } => Some(*pending_wake),
            _ => None,
        })
        .expect("the journal holds the pending-wake entry");
    assert!(pending_wake, "a pending-wake notice is marked on disk");

    let journal = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let promotions: Arc<Mutex<Vec<String>>> = Arc::default();
    let promotions_in = Arc::clone(&promotions);
    inbox.set_pending_wake_callback(Some(Arc::new(move |id: MessageId| {
        promotions_in.lock().unwrap().push(id.0.clone());
    })));
    let model = ScriptedModel::new([text("answer")]);
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), MemTools::new()));
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
        .expect("a resumed pending-wake entry must open a turn on open");
    handle.abort();
    let _ = handle.await;

    assert_eq!(sink.started(), ["t-1"]);
    assert_eq!(history(&path), ["notice", "answer"]);
    assert_eq!(
        *promotions.lock().unwrap(),
        vec![notice.0.clone()],
        "the resumed opening counts one wake"
    );
}

/// Lane sanity for the new operation: a pending-wake notice waits on the `next_step`
/// lane, like an `inject`.
#[tokio::test]
async fn a_pending_wake_notice_waits_on_the_next_step_lane() {
    let inbox = Inbox::new(8);
    inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();
    let snapshot = inbox.snapshot().await;
    assert_eq!(snapshot.len(), 1);
    let entry: &InboxEntry = &snapshot[0];
    assert_eq!(entry.lane, Lane::NextStep);
    assert!(!entry.wake);
    assert!(entry.pending_wake);
}

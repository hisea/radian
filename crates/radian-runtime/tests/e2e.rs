//! Test 11: a full turn through the real journal, a restart, and a second turn.

mod common;

use std::sync::Arc;

use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, events, text};
use radian_core::{Agent, InputSource, Message, TurnOptions};
use radian_runtime::{Driver, Inbox};
use radian_session::{Session, SessionJournal};
use serde_json::json;

#[tokio::test]
async fn a_full_turn_survives_a_restart_and_continues() {
    let dir = TempDir::new("e2e");
    let path = dir.file("s.jsonl");

    // ---- turn 1, then the driver "dies" ----
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({"n": 1}))]),
        text("first"),
    ]);
    let tools = MemTools::new();
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(model, tools.clone()));
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    inbox.followup("go", InputSource::User).await.unwrap();
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;
    // The durable inbox also holds the journal handle; drop it so the file lock is free.
    drop(inbox);

    // ---- restart: a fresh agent, tools and inbox over the same file ----
    let model2 = ScriptedModel::new([text("second")]);
    let tools2 = MemTools::new();
    let journal2 = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
    let inbox2 = Inbox::durable(16, Arc::clone(&journal2));
    let recorder2 = Arc::new(Recorder::default());
    let agent2 = Arc::new(Agent::new(Arc::clone(&model2), tools2.clone()));
    let mut driver2 = Driver::open(
        &agent2,
        journal2,
        Arc::clone(&inbox2),
        Arc::clone(&recorder2) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();

    // The first input is admitted: it is not replayed and not requeued.
    assert!(inbox2.is_empty().await);
    assert_eq!(
        Session::open(&path).unwrap().history().unwrap().len(),
        4,
        "turn 1's transcript survived the restart"
    );

    inbox2
        .followup("continue", InputSource::User)
        .await
        .unwrap();
    let handle2 = tokio::spawn(async move {
        let _ = driver2.run().await;
    });
    recorder2.wait_finished().await;
    handle2.abort();
    let _ = handle2.await;

    // ---- the history is exactly what the two turns produced ----
    let history = Session::open(&path).unwrap().history().unwrap();
    let texts: Vec<String> = history.iter().map(Message::text).collect();
    assert_eq!(
        texts,
        ["go", "", "{\"n\":1}", "first", "continue", "second"]
    );

    // Two turns are durably finished, with continuous seq.
    let durable = events(&path);
    let finished = durable
        .iter()
        .filter(|(name, _, _)| name == "TurnFinished")
        .count();
    assert_eq!(finished, 2);
    let seqs: Vec<u64> = durable.iter().map(|(_, seq, _)| *seq).collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());

    // The second turn's request carried the recovered history as a prefix.
    let request = &model2.requests()[0];
    assert_eq!(request.messages.len(), 5, "4 recovered + the new input");
    assert_eq!(request.messages[0].text(), "go");
    assert_eq!(request.messages.last().unwrap().text(), "continue");
    // The restart re-opened the tools; their in-process state did not survive.
    assert_eq!(tools2.started_ids(), Vec::<String>::new());
}

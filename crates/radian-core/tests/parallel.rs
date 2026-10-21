mod common;

use std::time::Duration;

use radian_core::*;
use common::*;
use serde_json::json;
use tokio::time::Instant;

fn position(list: &[String], id: &str) -> usize {
    list.iter()
        .position(|s| s == id)
        .unwrap_or_else(|| panic!("{id} missing from {list:?}"))
}

#[tokio::test(start_paused = true)]
async fn shared_calls_overlap_and_commit_in_order() {
    let model = ScriptedModel::new([
        calls(vec![
            call("s1", "slow300", json!({})),
            call("s2", "slow100", json!({})),
            call("s3", "slow200", json!({})),
        ]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![
        (shared("slow300"), Behavior::Delay(300)),
        (shared("slow100"), Behavior::Delay(100)),
        (shared("slow200"), Behavior::Delay(200)),
    ]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();
    let rec = Recorder::default();
    let start = Instant::now();

    chat.run_turn("go", TurnOptions::default(), &rec)
        .await
        .unwrap();

    assert_eq!(start.elapsed(), Duration::from_millis(300));
    assert_eq!(log.max_concurrent(), 3);
    assert_eq!(result_ids(chat.transcript()), ["s1", "s2", "s3"]);
    assert_eq!(rec.tool_finish_order(), ["s2", "s3", "s1"]);
}

#[tokio::test(start_paused = true)]
async fn exclusive_call_is_a_barrier() {
    let model = ScriptedModel::new([
        calls(vec![
            call("s1", "read", json!({})),
            call("s2", "read", json!({})),
            call("w", "write", json!({})),
            call("s3", "read", json!({})),
            call("s4", "read", json!({})),
        ]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![
        (shared("read"), Behavior::Delay(50)),
        (spec("write"), Behavior::Delay(50)),
    ]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let (started, finished) = (log.started(), log.finished());
    assert_eq!(log.max_concurrent(), 2);
    assert!(position(&started, "w") == 2);
    assert!(position(&finished, "s1") < position(&finished, "w"));
    assert!(position(&finished, "s2") < position(&finished, "w"));
    assert_eq!(position(&finished, "w"), 2);
    assert_eq!(result_ids(chat.transcript()), ["s1", "s2", "w", "s3", "s4"]);
}

#[tokio::test(start_paused = true)]
async fn max_parallel_tools_bounds_concurrency() {
    for (limit, expected_ms) in [(2, 300), (1, 600)] {
        let ids = ["a", "b", "c", "d", "e", "f"];
        let model = ScriptedModel::new([
            calls(ids.iter().map(|id| call(id, "read", json!({}))).collect()),
            text("ok"),
        ]);
        let tools = MemTools::new(vec![(shared("read"), Behavior::Delay(100))]);
        let log = tools.log.clone();
        let mut chat = agent(model, tools).open_session().await.unwrap();
        let options = TurnOptions {
            max_parallel_tools: limit,
            ..Default::default()
        };
        let start = Instant::now();

        chat.run_turn("go", options, &NoopSink).await.unwrap();

        assert_eq!(log.max_concurrent(), limit);
        assert_eq!(start.elapsed(), Duration::from_millis(expected_ms));
        assert_eq!(result_ids(chat.transcript()), ids);
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_during_parallel_batch_cancels_all_in_flight() {
    let model = ScriptedModel::new([calls(vec![
        call("h1", "hang", json!({})),
        call("h2", "hang", json!({})),
        call("quick", "quick", json!({})),
    ])]);
    let tools = MemTools::new(vec![
        (shared("hang"), Behavior::Hang),
        (shared("quick"), Behavior::Delay(10)),
    ]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&rec), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    let t = chat.transcript();
    assert_eq!(result_ids(t), ["h1", "h2", "quick"]);
    assert_eq!(code(result_for(t, "h1")), Some("cancelled"));
    assert_eq!(code(result_for(t, "h2")), Some("cancelled"));
    assert!(!result_for(t, "quick").is_error);
    // The hanging calls observed the cancellation and returned on their own.
    let mut finished = log.finished();
    finished.sort();
    assert_eq!(finished, ["h1", "h2", "quick"]);
    assert_eq!(rec.finished().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn drop_during_parallel_batch_poisons_and_fills_interrupted() {
    let model = ScriptedModel::new([calls(vec![
        call("quick", "quick", json!({})),
        call("h1", "hang", json!({})),
        call("h2", "hang", json!({})),
    ])]);
    let tools = MemTools::new(vec![
        (shared("hang"), Behavior::Hang),
        (shared("quick"), Behavior::Delay(10)),
    ]);
    let mut chat = agent(model, tools).open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let _ = tokio::time::timeout(Duration::from_millis(50), run.step(&rec)).await;

    assert_eq!(run.outcome().unwrap().status, RunStatus::Poisoned);
    drop(run);
    let resume = chat.history_for_resume();
    assert_eq!(result_ids(&resume.history), ["quick", "h1", "h2"]);
    assert!(!result_for(&resume.history, "quick").is_error);
    assert_eq!(code(result_for(&resume.history, "h1")), Some("interrupted"));
    assert_eq!(code(result_for(&resume.history, "h2")), Some("interrupted"));
    assert_eq!(rec.finished(), vec![RunStatus::Poisoned]);
}

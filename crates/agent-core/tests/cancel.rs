mod common;

use std::time::Duration;

use agent_core::*;
use common::*;
use serde_json::json;

async fn until_started(log: &ToolLog, id: &str) {
    while !log.started().iter().any(|s| s == id) {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn cancel_during_model_finishes_once() {
    let model = ScriptedModel::new([Reply::Hang]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&rec), async { cancel.cancel() });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    assert_eq!(
        rec.finished(),
        vec![RunStatus::Cancelled(CancelReason::Requested)]
    );
}

#[tokio::test]
async fn cancel_mid_tool_batch_keeps_completed_results() {
    let model = ScriptedModel::new([calls(vec![
        call("c1", "echo", json!({})),
        call("c2", "hang", json!({})),
        call("c3", "echo", json!({})),
    ])]);
    let tools = MemTools::new(vec![
        (spec("echo"), Behavior::Echo),
        (spec("hang"), Behavior::Hang),
    ]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&rec), async {
        until_started(&log, "c2").await;
        cancel.cancel();
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    let t = chat.transcript();
    assert_eq!(result_ids(t), ["c1", "c2", "c3"]);
    assert!(!result_for(t, "c1").is_error);
    assert_eq!(code(result_for(t, "c2")), Some("cancelled"));
    assert_eq!(code(result_for(t, "c3")), Some("cancelled"));
    assert_eq!(log.started(), ["c1", "c2"]);
    assert_eq!(log.finished(), ["c1"]);
    assert_eq!(rec.finished().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn turn_deadline_cancels_cleanly() {
    let model = ScriptedModel::new([Reply::Hang]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let rec = Recorder::default();
    let options = TurnOptions {
        deadline: Some(tokio::time::Instant::now() + Duration::from_secs(5)),
        ..Default::default()
    };

    let outcome = chat.run_turn("go", options, &rec).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::DeadlineExceeded)
    );
    assert_eq!(rec.finished().len(), 1);
    assert!(!chat.is_poisoned());
}

#[tokio::test(start_paused = true)]
async fn dropping_polled_step_poisons_run() {
    let model = ScriptedModel::new([Reply::Hang]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let timed_out = tokio::time::timeout(Duration::from_millis(10), run.step(&rec)).await;

    assert!(timed_out.is_err());
    assert_eq!(run.outcome().unwrap().status, RunStatus::Poisoned);
    assert_eq!(rec.finished(), vec![RunStatus::Poisoned]);
    assert!(matches!(run.step(&rec).await, Err(RunError::Poisoned)));
    drop(run);
    assert!(chat.is_poisoned());
    assert!(matches!(
        chat.start_turn("again").err(),
        Some(SessionError::Poisoned)
    ));
}

#[tokio::test]
async fn dropping_unpolled_step_has_no_effect() {
    let model = ScriptedModel::new([text("ok")]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let mut run = chat.start_turn("go").unwrap();

    drop(run.step(&NoopSink));

    assert!(run.outcome().is_none());
    assert_eq!(
        run.run(&NoopSink).await.unwrap().status,
        RunStatus::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn poisoned_session_history_is_resumable() {
    let model = ScriptedModel::new([calls(vec![
        call("c1", "hang", json!({})),
        call("c2", "echo", json!({})),
    ])]);
    let tools = MemTools::new(vec![
        (spec("echo"), Behavior::Echo),
        (spec("hang"), Behavior::Hang),
    ]);
    let mut chat = agent(model.clone(), tools.clone())
        .open_session()
        .await
        .unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let steering = run.steering_handle();

    let step = run.step(&NoopSink);
    let poll_until_hung = async {
        let _ = tokio::time::timeout(Duration::from_millis(10), step).await;
    };
    let ((), ()) = tokio::join!(poll_until_hung, async {
        until_started(&tools.log, "c1").await;
        steering.steer("also do this").unwrap();
    });
    drop(run);

    let resume = chat.history_for_resume();
    assert_eq!(code(result_for(&resume.history, "c1")), Some("interrupted"));
    assert_eq!(code(result_for(&resume.history, "c2")), Some("interrupted"));
    assert_eq!(
        resume.unapplied_steering,
        vec![Message::user("also do this")]
    );
    assert!(matches!(
        steering.steer("late"),
        Err(SteerError::RunFinished(_))
    ));

    model.push(text("resumed"));
    let mut chat = agent(model, tools)
        .open_session_with_history(resume.history)
        .await
        .unwrap();
    let outcome = chat
        .run_turn("continue", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
}

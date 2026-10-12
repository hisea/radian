mod common;

use std::time::Duration;

use agent_core::*;
use common::*;
use serde_json::json;
use tokio::time::sleep;

fn last_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|m| m.is_user())
        .map(Message::text)
        .unwrap_or_default()
}

#[tokio::test(start_paused = true)]
async fn steer_during_batch_injects_before_next_model_call() {
    let model = ScriptedModel::new([calls(vec![call("c1", "slow", json!({}))]), text("ok")]);
    let tools = MemTools::new(vec![(spec("slow"), Behavior::Delay(100))]);
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();
    let steering = run.steering_handle();

    let (outcome, ()) = tokio::join!(run.run(&rec), async {
        sleep(Duration::from_millis(50)).await;
        steering.steer("use metric units").unwrap();
    });

    assert_eq!(outcome.unwrap().status, RunStatus::Completed);
    let second = &model.requests()[1].messages;
    assert!(matches!(&second[2], Message::Tool { .. }));
    assert_eq!(second[3], Message::user("use metric units"));
    assert!(
        rec.events()
            .contains(&AgentEvent::SteeringApplied { count: 1 })
    );
}

#[tokio::test(start_paused = true)]
async fn steer_on_final_answer_continues_turn() {
    let model = ScriptedModel::new([slow_text(100, "first answer"), text("second answer")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let steering = run.steering_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        sleep(Duration::from_millis(10)).await;
        steering.steer("wait, one more thing").unwrap();
    });

    let outcome = outcome.unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.model_calls, 2);
    assert_eq!(outcome.final_message.unwrap().text(), "second answer");
    let requests = model.requests();
    // Steering never enters a request that is already in flight.
    assert_eq!(last_user_text(&requests[0]), "go");
    assert_eq!(last_user_text(&requests[1]), "wait, one more thing");
}

#[tokio::test]
async fn steer_before_first_step_joins_first_request() {
    let model = ScriptedModel::new([text("ok")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();
    let mut run = chat.start_turn("go").unwrap();
    run.steering_handle().steer("and be brief").unwrap();

    run.run(&NoopSink).await.unwrap();

    let first = &model.requests()[0].messages;
    assert_eq!(
        first,
        &vec![Message::user("go"), Message::user("and be brief")]
    );
}

#[tokio::test(start_paused = true)]
async fn steer_skip_remaining_skips_unstarted_calls() {
    let model = ScriptedModel::new([
        calls(vec![
            call("c1", "slow", json!({})),
            call("c2", "echo", json!({})),
            call("c3", "echo", json!({})),
        ]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![
        (spec("slow"), Behavior::Delay(100)),
        (spec("echo"), Behavior::Echo),
    ]);
    let log = tools.log.clone();
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();
    let options = TurnOptions {
        steer_mode: SteerMode::SkipRemaining,
        ..Default::default()
    };
    let mut run = chat.start_turn_with("go", options).unwrap();
    let steering = run.steering_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        sleep(Duration::from_millis(50)).await;
        steering.steer("stop, do something else").unwrap();
    });

    assert_eq!(outcome.unwrap().status, RunStatus::Completed);
    drop(run);
    let t = chat.transcript();
    assert_eq!(log.started(), ["c1"]);
    assert!(!result_for(t, "c1").is_error);
    assert_eq!(code(result_for(t, "c2")), Some("skipped_by_steering"));
    assert_eq!(code(result_for(t, "c3")), Some("skipped_by_steering"));
    assert_eq!(
        last_user_text(&model.requests()[1]),
        "stop, do something else"
    );
}

#[tokio::test]
async fn unapplied_steering_is_returned_on_cancel() {
    let model = ScriptedModel::new([Reply::Hang]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let steering = run.steering_handle();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        steering.steer("never applied").unwrap();
        cancel.cancel();
    });

    let outcome = outcome.unwrap();
    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    assert_eq!(
        outcome.unapplied_steering,
        vec![Message::user("never applied")]
    );
    match steering.steer("too late") {
        Err(SteerError::RunFinished(m)) => assert_eq!(m, Message::user("too late")),
        other => panic!("expected the message back, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn steering_without_budget_is_returned_not_lost() {
    let model = ScriptedModel::new([slow_text(100, "only answer")]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let options = TurnOptions {
        max_model_calls: Some(1),
        ..Default::default()
    };
    let mut run = chat.start_turn_with("go", options).unwrap();
    let steering = run.steering_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        sleep(Duration::from_millis(10)).await;
        steering.steer("follow up").unwrap();
    });

    let outcome = outcome.unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.unapplied_steering, vec![Message::user("follow up")]);
}

#[tokio::test]
async fn steering_must_be_a_user_message() {
    let mut chat = agent(ScriptedModel::new([]), NoTools)
        .open_session()
        .await
        .unwrap();
    let run = chat.start_turn("go").unwrap();
    let err = run
        .steering_handle()
        .steer(Message::assistant("nope"))
        .unwrap_err();
    assert!(matches!(err, SteerError::NotUserMessage(_)));
}

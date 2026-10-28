mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use radian_core::*;
use serde_json::json;

#[tokio::test]
async fn run_returns_final_answer() {
    let model = ScriptedModel::new([text("hi")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();
    let rec = Recorder::default();

    let outcome = chat
        .run_turn("hello", TurnOptions::default(), &rec)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_message.unwrap().text(), "hi");
    assert_eq!(outcome.model_calls, 1);
    assert_eq!(chat.transcript().len(), 2);
    assert_eq!(rec.finished(), vec![RunStatus::Completed]);
}

#[tokio::test]
async fn run_executes_tool_then_continues() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({"a": 1}))]),
        text("done"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    let t = chat.transcript();
    assert!(matches!(&t[0], Message::User { .. }));
    assert!(matches!(&t[1], Message::Assistant { tool_calls, .. } if tool_calls.len() == 1));
    assert_eq!(result_for(t, "c1").text(), r#"{"a":1}"#);
    assert_eq!(t[3].text(), "done");
    // The second request carried the tool result.
    assert_eq!(model.requests()[1].messages.len(), 3);
}

#[tokio::test]
async fn tool_error_is_fed_back() {
    let model = ScriptedModel::new([calls(vec![call("c1", "bad", json!({}))]), text("ok")]);
    let tools = MemTools::new(vec![(spec("bad"), Behavior::Fail)]);
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    let seen = &model.requests()[1].messages;
    let result = result_for(seen, "c1");
    assert!(result.is_error);
    assert_eq!(code(result), Some("boom"));
}

#[tokio::test]
async fn mixed_valid_invalid_tool_calls_preserve_order() {
    let model = ScriptedModel::new([
        calls(vec![
            call("c1", "nope", json!({})),
            call("c2", "typed", json!({"x": "not a number"})),
            call("c3", "typed", json!({"x": 3})),
            call("c4", "typed", json!("not an object")),
        ]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![(typed("typed"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let t = chat.transcript();
    assert_eq!(result_ids(t), ["c1", "c2", "c3", "c4"]);
    assert_eq!(code(result_for(t, "c1")), Some("unknown_tool"));
    assert_eq!(code(result_for(t, "c2")), Some("invalid_arguments"));
    assert!(!result_for(t, "c3").is_error);
    assert_eq!(code(result_for(t, "c4")), Some("invalid_arguments"));
    assert_eq!(log.started(), ["c3"]);
}

#[tokio::test]
async fn truncated_tool_calls_are_never_executed() {
    let model = ScriptedModel::new([
        calls_with(
            StopReason::MaxTokens,
            vec![call("c1", "echo", json!({})), call("c2", "echo", json!({}))],
        ),
        text("retry later"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert!(log.started().is_empty());
    assert_eq!(code(result_for(chat.transcript(), "c1")), Some("truncated"));
    assert_eq!(code(result_for(chat.transcript(), "c2")), Some("truncated"));
}

const MAX_TOKENS_NOTICE: &str = "Your last response hit the output token limit. Do not apologize, recap, or repeat yourself. Continue from where you stopped, and break the remaining work into smaller pieces.";

fn notice_count(transcript: &[Message]) -> usize {
    transcript
        .iter()
        .filter(|m| m.is_user() && m.text() == MAX_TOKENS_NOTICE)
        .count()
}

/// A truncated answer with no tool calls stays in the transcript, gains one notice, and the
/// next request carries both (ADR 0017 §2).
#[tokio::test]
async fn a_truncated_response_without_calls_continues_with_a_notice() {
    let model = ScriptedModel::new([stop(StopReason::MaxTokens, "half"), text("finished")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.model_calls, 2);
    let t = chat.transcript();
    assert_eq!(t.len(), 4, "input, truncated answer, notice, answer");
    assert_eq!(t[1].text(), "half");
    assert!(t[2].is_user());
    assert_eq!(t[2].meta().source, Some(InputSource::System));
    assert_eq!(t[2].text(), MAX_TOKENS_NOTICE);
    assert_eq!(t[3].text(), "finished");
    assert_eq!(notice_count(t), 1);
    // The second request carries the notice after the truncated assistant message.
    let second = &model.requests()[1].messages;
    assert_eq!(second.len(), 3);
    assert_eq!(second[1].text(), "half");
    assert_eq!(second[2].text(), MAX_TOKENS_NOTICE);
}

/// The third consecutive truncated answer with no tool calls ends the turn: two notices, then
/// `Incomplete(MaxTokens)` (ADR 0017 §2).
#[tokio::test]
async fn three_truncated_responses_without_calls_end_incomplete_with_two_notices() {
    let model = ScriptedModel::new([
        stop(StopReason::MaxTokens, "one"),
        stop(StopReason::MaxTokens, "two"),
        stop(StopReason::MaxTokens, "three"),
        text("never sent"),
    ]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Incomplete(IncompleteReason::MaxTokens)
    );
    assert_eq!(outcome.model_calls, 3);
    assert_eq!(
        model.requests().len(),
        3,
        "the third truncation ends the turn"
    );
    let t = chat.transcript();
    assert_eq!(notice_count(t), 2);
    assert_eq!(t.len(), 6, "input and three answers, with two notices");
    assert_eq!(t[5].text(), "three");
    assert!(matches!(&t[5], Message::Assistant { .. }));
}

/// Truncated responses with tool calls share the streak with truncated answers without them,
/// and a tool-call truncation continues the turn even after the streak passed two (ADR 0017 §2).
#[tokio::test]
async fn truncated_tool_call_responses_share_the_streak() {
    let model = ScriptedModel::new([
        calls_with(StopReason::MaxTokens, vec![call("c1", "echo", json!({}))]),
        stop(StopReason::MaxTokens, "second"),
        calls_with(StopReason::MaxTokens, vec![call("c2", "echo", json!({}))]),
        stop(StopReason::MaxTokens, "fourth"),
        text("never sent"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    // The first truncation (with calls) makes the second one the second in the streak, so that
    // one continues with a notice. The tool-call truncation after it is the third and still
    // continues without one; the fourth truncation, with no calls, ends the turn.
    assert_eq!(
        outcome.status,
        RunStatus::Incomplete(IncompleteReason::MaxTokens)
    );
    assert_eq!(outcome.model_calls, 4);
    assert_eq!(model.requests().len(), 4);
    assert!(log.started().is_empty(), "truncated calls never execute");
    assert_eq!(code(result_for(chat.transcript(), "c1")), Some("truncated"));
    assert_eq!(code(result_for(chat.transcript(), "c2")), Some("truncated"));
    assert_eq!(
        notice_count(chat.transcript()),
        1,
        "only the second truncation, with no calls, continued with a notice"
    );
}

/// A truncated response with tool calls counts toward the shared streak: the answer without
/// calls that follows it is only the second truncation, so it continues, and the next one ends
/// the turn (ADR 0017 §2).
#[tokio::test]
async fn a_truncated_tool_call_response_counts_toward_the_streak() {
    let model = ScriptedModel::new([
        calls_with(StopReason::MaxTokens, vec![call("c1", "echo", json!({}))]),
        stop(StopReason::MaxTokens, "second"),
        stop(StopReason::MaxTokens, "third"),
        text("never sent"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Incomplete(IncompleteReason::MaxTokens)
    );
    assert_eq!(outcome.model_calls, 3);
    assert_eq!(model.requests().len(), 3);
    assert_eq!(
        notice_count(chat.transcript()),
        1,
        "the second truncation was continued"
    );
    assert_eq!(code(result_for(chat.transcript(), "c1")), Some("truncated"));
}

/// Two truncated answers with no tool calls each gain a notice, and the normal answer after
/// them completes the turn (ADR 0017 §2).
#[tokio::test]
async fn two_truncated_responses_without_calls_then_a_normal_one_complete() {
    let model = ScriptedModel::new([
        stop(StopReason::MaxTokens, "one"),
        stop(StopReason::MaxTokens, "two"),
        text("done"),
    ]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.model_calls, 3);
    assert_eq!(notice_count(chat.transcript()), 2);
    let t = chat.transcript();
    assert_eq!(t.len(), 6, "input, two answers, two notices, the answer");
    assert_eq!(t[1].text(), "one");
    assert_eq!(t[2].text(), MAX_TOKENS_NOTICE);
    assert_eq!(t[3].text(), "two");
    assert_eq!(t[4].text(), MAX_TOKENS_NOTICE);
    assert_eq!(t[5].text(), "done");
}

/// Any response that is not `MaxTokens` resets the streak (ADR 0017 §2).
#[tokio::test]
async fn an_untruncated_response_resets_the_streak() {
    let model = ScriptedModel::new([
        stop(StopReason::MaxTokens, "first"),
        calls(vec![call("c1", "echo", json!({}))]),
        stop(StopReason::MaxTokens, "third"),
        stop(StopReason::MaxTokens, "fourth"),
        text("done"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.model_calls, 5);
    assert_eq!(model.requests().len(), 5);
    assert_eq!(notice_count(chat.transcript()), 3);
}

/// The call cap wins over a continuation: the turn ends `BudgetExceeded`, not
/// `Incomplete(MaxTokens)`, and no notice is written for a call that cannot happen.
#[tokio::test]
async fn the_call_cap_beats_a_truncation_continuation() {
    let model = ScriptedModel::new([stop(StopReason::MaxTokens, "half"), text("never sent")]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();
    let options = TurnOptions {
        max_model_calls: Some(1),
        ..Default::default()
    };

    let outcome = chat.run_turn("go", options, &NoopSink).await.unwrap();

    assert_eq!(outcome.status, RunStatus::BudgetExceeded);
    assert_eq!(outcome.model_calls, 1);
    assert_eq!(model.requests().len(), 1);
    assert_eq!(notice_count(chat.transcript()), 0);
}

/// A deadline that has already passed by the time the truncated answer is in the transcript
/// ends the turn on that deadline and writes no continuation notice: the notice's model call
/// can never happen (ADR 0017 §2).
#[tokio::test]
async fn a_passed_deadline_beats_a_truncation_continuation() {
    /// Blocks long enough that the turn deadline passes between the response and the notice.
    struct SlowAfterResponse;

    impl Hooks for SlowAfterResponse {
        async fn after_model_response(
            &self,
            _cx: &HookContext,
            _response: &ModelResponse,
        ) -> Result<(), HookError> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(())
        }
    }

    let model = ScriptedModel::new([stop(StopReason::MaxTokens, "half"), text("never sent")]);
    let agent = Arc::new(Agent::new(model.clone(), NoTools).with_hooks(SlowAfterResponse));
    let mut chat = agent.open_session().await.unwrap();
    let options = TurnOptions {
        deadline: Some(tokio::time::Instant::now() + Duration::from_millis(20)),
        ..Default::default()
    };

    let outcome = chat.run_turn("go", options, &NoopSink).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::DeadlineExceeded)
    );
    assert_eq!(outcome.model_calls, 1);
    assert_eq!(model.requests().len(), 1, "the continuation never ran");
    assert_eq!(notice_count(chat.transcript()), 0);
    assert_eq!(chat.transcript().len(), 2, "input and the truncated answer");
}

#[tokio::test]
async fn content_filtered_tool_calls_are_never_executed() {
    let model = ScriptedModel::new([calls_with(
        StopReason::ContentFiltered,
        vec![call("c1", "echo", json!({}))],
    )]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Incomplete(IncompleteReason::ContentFiltered)
    );
    assert!(log.started().is_empty());
    assert_eq!(
        code(result_for(chat.transcript(), "c1")),
        Some("content_filtered")
    );
}

#[tokio::test]
async fn max_calls_final_answer_is_allowed() {
    let model = ScriptedModel::new([text("one shot")]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let options = TurnOptions {
        max_model_calls: Some(1),
        ..Default::default()
    };

    let outcome = chat.run_turn("go", options, &NoopSink).await.unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
}

#[tokio::test]
async fn max_calls_tool_request_is_not_executed() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({}))]),
        calls(vec![call("c2", "echo", json!({}))]),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();
    let rec = Recorder::default();
    let options = TurnOptions {
        max_model_calls: Some(2),
        ..Default::default()
    };

    let outcome = chat.run_turn("go", options, &rec).await.unwrap();

    assert_eq!(outcome.status, RunStatus::BudgetExceeded);
    assert_eq!(outcome.model_calls, 2);
    assert_eq!(log.started(), ["c1"]);
    assert_eq!(
        code(result_for(chat.transcript(), "c2")),
        Some("budget_exhausted")
    );
    assert_eq!(rec.finished(), vec![RunStatus::BudgetExceeded]);
}

#[tokio::test]
async fn model_error_finishes_once() {
    let model = ScriptedModel::new([Reply::Fail("network down".into())]);
    // Retries are off (ADR 0017): this pins the "one failure ends the turn" path.
    let chat_agent = Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        retries: 0,
        ..Default::default()
    }));
    let mut chat = chat_agent.open_session().await.unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    assert!(matches!(err, RunError::Model(ModelError::Transport(_))));
    assert_eq!(run.outcome().unwrap().status, RunStatus::Failed);
    assert_eq!(rec.finished(), vec![RunStatus::Failed]);
    // The session itself is fine; the next turn can run.
    drop(run);
    assert!(chat.start_turn("again").is_ok());
}

#[tokio::test]
async fn stop_reasons_map_to_outcomes() {
    let cases = [
        (StopReason::Refusal, RunStatus::Refused),
        (
            StopReason::ContentFiltered,
            RunStatus::Incomplete(IncompleteReason::ContentFiltered),
        ),
        (
            StopReason::ToolUse,
            RunStatus::Incomplete(IncompleteReason::ToolUseWithoutCalls),
        ),
        (
            StopReason::Other("weird".into()),
            RunStatus::Incomplete(IncompleteReason::UnknownStopReason("weird".into())),
        ),
    ];
    for (reason, expected) in cases {
        let model = ScriptedModel::new([stop(reason, "partial")]);
        let mut chat = agent(model, NoTools).open_session().await.unwrap();
        let outcome = chat
            .run_turn("go", TurnOptions::default(), &NoopSink)
            .await
            .unwrap();
        assert_eq!(outcome.status, expected);
    }
}

#[tokio::test]
async fn tool_calls_run_whatever_the_stop_reason() {
    // Some providers report a plain stop with tool calls attached.
    let model = ScriptedModel::new([
        calls_with(StopReason::EndTurn, vec![call("c1", "echo", json!({}))]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model, tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(log.started(), ["c1"]);
}

#[tokio::test]
async fn pause_turn_continues_with_same_budget_and_keeps_opaque_blocks() {
    let server_block = ContentPart::Opaque {
        raw: OpaqueJson::parse(r#"{"type":"server_tool_use","id":"s1"}"#).unwrap(),
    };
    let model = ScriptedModel::new([
        Reply::Respond(ModelResponse {
            limits: None,
            reasoning: Vec::new(),
            content: vec![server_block.clone()],
            tool_calls: vec![],
            provider_payload: None,
            stop_reason: StopReason::PauseTurn,
            usage: None,
        }),
        text("finished"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let mut chat = agent(model.clone(), tools).open_session().await.unwrap();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.model_calls, 2);
    assert!(log.started().is_empty());
    let second = &model.requests()[1].messages;
    assert!(
        matches!(&second[1], Message::Assistant { content, .. } if content == &vec![server_block.clone()])
    );

    // With a budget of one, the pause cannot continue.
    let model = ScriptedModel::new([stop(StopReason::PauseTurn, "")]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let options = TurnOptions {
        max_model_calls: Some(1),
        ..Default::default()
    };
    let outcome = chat.run_turn("go", options, &NoopSink).await.unwrap();
    assert_eq!(outcome.status, RunStatus::BudgetExceeded);
}

#[tokio::test]
async fn session_history_continues_across_turns() {
    let model = ScriptedModel::new([text("second answer"), text("third answer")]);
    let history = vec![Message::user("first"), Message::assistant("first answer")];
    let mut chat = agent(model.clone(), NoTools)
        .open_session_with_history(history)
        .await
        .unwrap();

    chat.run_turn("second", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    chat.run_turn("third", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let requests = model.requests();
    assert_eq!(requests[0].messages.len(), 3);
    assert_eq!(requests[1].messages.len(), 5);
    assert_eq!(chat.transcript().len(), 6);
}

#[tokio::test]
async fn history_with_unpaired_tool_call_is_rejected() {
    let dangling = vec![
        Message::user("hi"),
        Message::Assistant {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![call("c1", "echo", json!({}))],
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        },
        Message::user("next"),
    ];
    let a = agent(ScriptedModel::new([]), NoTools);
    let err = a.open_session_with_history(dangling).await.err().unwrap();
    assert!(matches!(err, SessionError::InvalidHistory(_)));

    let orphan = vec![Message::Tool {
        result: ToolResult::success("ghost", vec![]),
        meta: MessageMeta::default(),
    }];
    let err = a.open_session_with_history(orphan).await.err().unwrap();
    assert!(matches!(err, SessionError::InvalidHistory(_)));
}

#[tokio::test]
async fn provider_payload_round_trips() {
    let raw = r#"{ "signature" : "abc",  "blocks":[1, 2] }"#;
    let content = vec![
        ContentPart::Image {
            mime_type: "image/png".into(),
            data: "AAAA".into(),
            annotations: Some(json!({"audience": ["user"]})),
            meta: Some(json!({"trace": "abc"})),
        },
        ContentPart::ResourceLink {
            uri: "file:///a".into(),
            name: Some("a".into()),
            mime_type: None,
            annotations: None,
            meta: Some(json!({"k": 1})),
        },
    ];
    let model = ScriptedModel::new([
        Reply::Respond(ModelResponse {
            limits: None,
            reasoning: Vec::new(),
            content: content.clone(),
            tool_calls: vec![],
            provider_payload: Some(OpaqueJson::parse(raw).unwrap()),
            stop_reason: StopReason::EndTurn,
            usage: None,
        }),
        text("next"),
    ]);
    let mut chat = agent(model.clone(), NoTools).open_session().await.unwrap();
    chat.run_turn("one", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    // Serialize and restore the transcript, then send it back to the adapter.
    let json = serde_json::to_string(chat.transcript()).unwrap();
    let restored: Vec<Message> = serde_json::from_str(&json).unwrap();
    let mut chat = agent(model.clone(), NoTools)
        .open_session_with_history(restored)
        .await
        .unwrap();
    chat.run_turn("two", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let sent = &model.requests()[1].messages[1];
    let Message::Assistant {
        content: sent_content,
        provider_payload: Some(payload),
        ..
    } = sent
    else {
        panic!("expected the assistant message with its payload");
    };
    assert_eq!(payload.get(), raw);
    assert_eq!(sent_content, &content);
}

#[tokio::test]
async fn step_and_run_share_semantics() {
    let script = || {
        [
            calls(vec![call("c1", "echo", json!({"n": 1}))]),
            calls(vec![call("c2", "echo", json!({"n": 2}))]),
            text("done"),
        ]
    };
    let tools = || MemTools::new(vec![(spec("echo"), Behavior::Echo)]);

    let rec_run = Recorder::default();
    let mut chat_a = agent(ScriptedModel::new(script()), tools())
        .open_session()
        .await
        .unwrap();
    let outcome_a = chat_a
        .run_turn("go", TurnOptions::default(), &rec_run)
        .await
        .unwrap();

    let rec_step = Recorder::default();
    let mut chat_b = agent(ScriptedModel::new(script()), tools())
        .open_session()
        .await
        .unwrap();
    let mut run = chat_b.start_turn("go").unwrap();
    let mut steps = 0;
    let outcome_b = loop {
        steps += 1;
        if let Step::Finished(o) = run.step(&rec_step).await.unwrap() {
            break o;
        }
    };
    drop(run);

    assert_eq!(steps, 3);
    assert_eq!(outcome_a, outcome_b);
    assert_eq!(chat_a.transcript(), chat_b.transcript());
    assert_eq!(rec_run.events(), rec_step.events());
}

#[tokio::test]
async fn terminal_run_cannot_step_again() {
    let model = ScriptedModel::new([text("done")]);
    let mut chat = agent(model, NoTools).open_session().await.unwrap();
    let mut run = chat.start_turn("go").unwrap();
    run.run(&NoopSink).await.unwrap();

    assert!(matches!(run.step(&NoopSink).await, Err(RunError::Finished)));
}

#[tokio::test]
async fn schema_external_refs_are_disabled() {
    let remote = ToolSpec {
        input_schema: json!({"$ref": "https://example.com/schema.json"}),
        ..spec("remote")
    };
    let a = agent(
        ScriptedModel::new([]),
        MemTools::new(vec![(remote, Behavior::Echo)]),
    );

    let err = a.open_session().await.err().unwrap();

    assert!(matches!(err, SessionError::ToolSpec { tool, .. } if tool == "remote"));
}

#[tokio::test]
async fn independent_sessions_do_not_share_state() {
    let model = ScriptedModel::new([text("a"), text("b")]);
    let a = agent(model, NoTools);
    let mut one = a.open_session().await.unwrap();
    let mut two = a.open_session().await.unwrap();

    one.run_turn("to one", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let mut run = two.start_turn("to two").unwrap();
    run.cancellation_handle().cancel();
    let outcome = run.run(&NoopSink).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    assert_eq!(one.transcript().len(), 2);
}

#[test]
fn run_turn_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut chat = rt
        .block_on(agent(ScriptedModel::new([]), MemTools::new(vec![])).open_session())
        .unwrap();
    let fut = chat.run_turn("x", TurnOptions::default(), &NoopSink);
    assert_send(&fut);
}

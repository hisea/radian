mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::*;
use common::*;
use serde_json::{Value, json};

/// Decides per call id.
struct Policy;

impl Hooks for Policy {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        Ok(match call.id.as_str() {
            "deny" => ToolDecision::Deny {
                reason: "not allowed here".into(),
            },
            "rewrite" => ToolDecision::Rewrite {
                arguments: json!({"x": 5}),
            },
            "respond" => ToolDecision::Respond(ToolResult::success(
                "ignored",
                vec![ContentPart::text("cached")],
            )),
            _ => ToolDecision::Allow,
        })
    }
}

#[tokio::test]
async fn before_tool_decisions() {
    let model = ScriptedModel::new([
        calls(vec![
            call("allow", "typed", json!({"x": 1})),
            call("deny", "typed", json!({"x": 2})),
            call("rewrite", "typed", json!({"x": 3})),
            call("respond", "typed", json!({"x": 4})),
        ]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![(typed("typed"), Behavior::Echo)]);
    let log = tools.log.clone();
    let a = Arc::new(Agent::new(model, tools).with_hooks(Policy));
    let mut chat = a.open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let t = chat.transcript();
    assert_eq!(log.started(), ["allow", "rewrite"]);
    assert_eq!(result_for(t, "allow").text(), r#"{"x":1}"#);
    assert_eq!(code(result_for(t, "deny")), Some("denied"));
    assert_eq!(result_for(t, "deny").text(), "not allowed here");
    assert_eq!(result_for(t, "rewrite").text(), r#"{"x":5}"#);
    assert_eq!(result_for(t, "respond").text(), "cached");
    assert!(!result_for(t, "respond").is_error);
}

#[derive(Default)]
struct BadRewrite(AtomicUsize);

impl Hooks for BadRewrite {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolDecision::Rewrite {
            arguments: json!({"x": "not a number"}),
        })
    }
}

#[tokio::test]
async fn rewritten_args_are_revalidated_without_rehooking() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "typed", json!({"x": 1}))]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![(typed("typed"), Behavior::Echo)]);
    let log = tools.log.clone();
    let hooks = Arc::new(BadRewrite::default());
    let a = Arc::new(Agent::new(model, tools).with_hooks(hooks.clone()));
    let mut chat = a.open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(
        code(result_for(chat.transcript(), "c1")),
        Some("invalid_arguments")
    );
    assert!(log.started().is_empty());
    assert_eq!(hooks.0.load(Ordering::SeqCst), 1);
}

/// Fails at one hook point.
struct FailAt(HookPoint);

impl Hooks for FailAt {
    async fn before_model_call(
        &self,
        _cx: &HookContext,
        _r: &mut ModelRequest,
    ) -> Result<(), HookError> {
        self.fail_if(HookPoint::BeforeModelCall)
    }
    async fn after_model_response(
        &self,
        _cx: &HookContext,
        _r: &ModelResponse,
    ) -> Result<(), HookError> {
        self.fail_if(HookPoint::AfterModelResponse)
    }
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _c: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        self.fail_if(HookPoint::BeforeToolCall)
            .map(|()| ToolDecision::Allow)
    }
    async fn after_tool_call(
        &self,
        _cx: &HookContext,
        _c: &ToolCall,
        _r: &mut ToolResult,
    ) -> Result<(), HookError> {
        self.fail_if(HookPoint::AfterToolCall)
    }
}

impl FailAt {
    fn fail_if(&self, point: HookPoint) -> Result<(), HookError> {
        if self.0 == point {
            Err(HookError::new("policy service down"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn hook_errors_fail_closed() {
    for point in [HookPoint::BeforeModelCall, HookPoint::AfterModelResponse] {
        let model = ScriptedModel::new([text("secret answer")]);
        let a = Arc::new(Agent::new(model, NoTools).with_hooks(FailAt(point)));
        let mut chat = a.open_session().await.unwrap();
        let rec = Recorder::default();

        let err = chat
            .run_turn("go", TurnOptions::default(), &rec)
            .await
            .unwrap_err();

        assert!(matches!(err, RunError::Hook { point: p, .. } if p == point));
        assert_eq!(rec.finished(), vec![RunStatus::Failed]);
        assert!(
            rec.events()
                .iter()
                .any(|e| matches!(e, AgentEvent::HookFailed { .. }))
        );
        // The response never reaches the transcript.
        assert_eq!(chat.transcript().len(), 1);
    }

    for point in [HookPoint::BeforeToolCall, HookPoint::AfterToolCall] {
        let model = ScriptedModel::new([
            calls(vec![call("c1", "echo", json!({"secret": true}))]),
            text("ok"),
        ]);
        let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
        let log = tools.log.clone();
        let a = Arc::new(Agent::new(model, tools).with_hooks(FailAt(point)));
        let mut chat = a.open_session().await.unwrap();

        let outcome = chat
            .run_turn("go", TurnOptions::default(), &NoopSink)
            .await
            .unwrap();

        assert_eq!(outcome.status, RunStatus::Completed);
        let result = result_for(chat.transcript(), "c1");
        assert_eq!(code(result), Some("hook_error"));
        assert!(!result.text().contains("secret"));
        let ran = !log.started().is_empty();
        assert_eq!(ran, point == HookPoint::AfterToolCall);
    }
}

/// Records what it saw, then applies its own decision.
struct Step(&'static str, Arc<Mutex<Vec<String>>>, Option<ToolDecision>);

impl Hooks for Step {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        self.1
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.0, call.arguments));
        Ok(self.2.clone().unwrap_or(ToolDecision::Allow))
    }
    async fn after_tool_call(
        &self,
        _cx: &HookContext,
        _c: &ToolCall,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        result
            .content
            .push(ContentPart::text(format!("+{}", self.0)));
        Ok(())
    }
}

#[tokio::test]
async fn hook_composition_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let first = Step(
        "a",
        seen.clone(),
        Some(ToolDecision::Rewrite {
            arguments: json!({"v": 2}),
        }),
    );
    let second = Step("b", seen.clone(), None);
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", json!({"v": 1}))]), text("ok")]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let a = Arc::new(Agent::new(model, tools).with_hooks((first, second)));
    let mut chat = a.open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(*seen.lock().unwrap(), [r#"a:{"v":1}"#, r#"b:{"v":2}"#]);
    assert_eq!(result_for(chat.transcript(), "c1").text(), r#"{"v":2}+a+b"#);

    // A denial in the first hook short-circuits the second.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let deny = Step(
        "a",
        seen.clone(),
        Some(ToolDecision::Deny {
            reason: "no".into(),
        }),
    );
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", json!({}))]), text("ok")]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let a = Arc::new(Agent::new(model, tools).with_hooks((deny, Step("b", seen.clone(), None))));
    a.open_session()
        .await
        .unwrap()
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Waits for an approval that never comes.
#[derive(Default)]
struct WaitsForApproval(AtomicUsize);

impl Hooks for WaitsForApproval {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _c: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test]
async fn approval_hook_races_cancellation() {
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", json!({}))])]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let log = tools.log.clone();
    let hooks = Arc::new(WaitsForApproval::default());
    let a = Arc::new(Agent::new(model, tools).with_hooks(hooks.clone()));
    let mut chat = a.open_session().await.unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        while hooks.0.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    assert!(log.started().is_empty());
    assert_eq!(code(result_for(chat.transcript(), "c1")), Some("cancelled"));
}

/// Adds context to each request and counts turn ends.
#[derive(Default)]
struct AddsContext(AtomicUsize);

impl Hooks for AddsContext {
    async fn before_model_call(
        &self,
        _cx: &HookContext,
        request: &mut ModelRequest,
    ) -> Result<(), HookError> {
        request
            .messages
            .push(Message::user("context: today is Monday"));
        request.settings = Value::String("tuned".into());
        Ok(())
    }
    fn on_turn_end(&self, _cx: &HookContext, _outcome: &RunOutcome) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn before_model_call_changes_request_not_transcript() {
    let model = ScriptedModel::new([text("ok")]);
    let hooks = Arc::new(AddsContext::default());
    let a = Arc::new(Agent::new(model.clone(), NoTools).with_hooks(hooks.clone()));
    let mut chat = a.open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    let sent = &model.requests()[0];
    assert_eq!(sent.messages.len(), 2);
    assert_eq!(sent.settings, Value::String("tuned".into()));
    assert_eq!(chat.transcript().len(), 2);
    assert_eq!(hooks.0.load(Ordering::SeqCst), 1);
}

/// Tries to make the result answer a different call.
struct StealsCallId;

impl Hooks for StealsCallId {
    async fn after_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        result.call_id = "someone-else".into();
        result.content.push(ContentPart::text(" (reviewed)"));
        Ok(())
    }
}

#[tokio::test]
async fn after_tool_call_cannot_change_call_id() {
    let model = ScriptedModel::new([
        calls(vec![
            call("c1", "echo", json!({})),
            call("c2", "echo", json!({})),
        ]),
        text("ok"),
    ]);
    let tools = MemTools::new(vec![(shared("echo"), Behavior::Echo)]);
    let a = Arc::new(Agent::new(model, tools.clone()).with_hooks(StealsCallId));
    let mut chat = a.open_session().await.unwrap();

    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    // Content edits are kept; the pairing with the model's calls is not changed.
    assert_eq!(result_ids(chat.transcript()), ["c1", "c2"]);
    assert_eq!(result_for(chat.transcript(), "c1").text(), "{} (reviewed)");
    let history = chat.history_for_resume().history;
    let reopened = agent(ScriptedModel::new([]), tools)
        .open_session_with_history(history)
        .await;
    assert!(reopened.is_ok());
}

// ---------- hook error investigation (ADR 0017, item 5) ----------

/// A `before_tool_call` hook that always fails, so the turn ends as `RunError::Hook`.
struct FailBeforeTool;

impl Hooks for FailBeforeTool {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        Err(HookError::new("the hook refused to run the tool"))
    }
}

/// Investigation only (ADR 0017 item 5): the model already produced an assistant message with
/// `tool_calls`, then `before_tool_call` fails. `fail_hook` ends the turn as `Failed` without
/// repairing the transcript, so the assistant tool call stays with **no** result. The next
/// turn would send that unpaired call, which both providers reject as an invalid request.
///
/// The desired behavior is asserted here (every declared call has a result); it fails today,
/// hence `#[ignore]`. Run with `--ignored` to reproduce. Hook errors stay fatal until this is
/// decided; this test exists so the decision has a repro.
#[tokio::test]
#[ignore = "known gap: a failed before_tool_call hook leaves an unpaired tool call (ADR 0017 item 5)"]
async fn a_before_tool_hook_error_does_not_leave_unpaired_tool_calls() {
    let model = ScriptedModel::new([
        calls(vec![call("c1", "echo", json!({}))]),
        text("never reached"),
    ]);
    let tools = MemTools::new(vec![(spec("echo"), Behavior::Echo)]);
    let a = Arc::new(Agent::new(model, tools).with_hooks(FailBeforeTool));
    let mut chat = a.open_session().await.unwrap();

    let error = chat
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            RunError::Hook {
                point: HookPoint::BeforeToolCall,
                ..
            }
        ),
        "{error}"
    );

    let transcript = chat.transcript();
    let declared: Vec<String> = transcript
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        })
        .collect();
    let answered: Vec<String> = transcript
        .iter()
        .filter_map(|message| match message {
            Message::Tool { result, .. } => Some(result.call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        declared, answered,
        "every tool call must have a result before the next request: {transcript:?}"
    );
}

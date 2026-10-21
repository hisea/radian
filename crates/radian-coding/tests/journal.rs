//! ADR 0024 Q3: the question and the answer are durable without a new record — the question
//! is the call's `ToolExecutionStarted.arguments` and the answer its `ToolExecutionSettled`
//! result (ADR 0002 E1). This runs a real turn through `radian-core` with the recording journal.

mod common;

use std::sync::{Arc, Mutex};

use common::TempDir;
use futures_util::future::BoxFuture;
use radian_coding::{PromptError, Prompter, QuestionAnswer, QuestionReply, QuestionRequest};
use radian_core::testing::RecordingJournal;
use radian_core::{
    Agent, ContentPart, JournalRecord, Message, Model, ModelContext, ModelError, ModelRequest,
    ModelResponse, NoopSink, StopReason, ToolCall,
};
use serde_json::json;

/// Asks one question on the first model call, then answers the user.
#[derive(Default)]
struct AskingModel {
    calls: Mutex<usize>,
}

impl Model for AskingModel {
    async fn complete(
        &self,
        _request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            Ok(ModelResponse {
                limits: None,
                content: Vec::new(),
                reasoning: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "ask_user".into(),
                    arguments: json!({"questions": [{
                        "id": "loader",
                        "prompt": "How should it load?",
                        "kind": "single",
                        "options": [
                            {"id": "builtin", "label": "Built-in"},
                            {"id": "process", "label": "Process plugins"}
                        ]
                    }]}),
                }],
                provider_payload: None,
                stop_reason: StopReason::ToolUse,
                usage: None,
            })
        } else {
            Ok(ModelResponse {
                limits: None,
                content: vec![ContentPart::text("done")],
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
                provider_payload: None,
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
        }
    }
}

struct FixedPrompter;

impl Prompter for FixedPrompter {
    fn ask(&self, _request: QuestionRequest) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>> {
        Box::pin(async {
            Ok(QuestionAnswer::new(
                "q-7",
                vec![QuestionReply {
                    id: "loader".into(),
                    selected: vec!["builtin".into()],
                    ..QuestionReply::default()
                }],
            ))
        })
    }
}

#[tokio::test]
async fn the_question_and_the_answer_are_journaled_as_the_call_arguments_and_result() {
    let dir = TempDir::new("ask-journal");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = radian_coding::AskTools::new(Arc::new(FixedPrompter));
    let journal = Arc::new(RecordingJournal::new());
    let agent = Arc::new(Agent::new(AskingModel::default(), tools));
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal.clone())
        .await
        .unwrap();
    chat.run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();
    drop(chat);

    let records = journal.records();
    // The question: the call's arguments, exactly as the model wrote them.
    let (call_id, arguments) = records
        .iter()
        .find_map(|record| match record {
            JournalRecord::ToolExecutionStarted {
                call_id,
                name,
                arguments,
            } if name == "ask_user" => Some((call_id.clone(), arguments.clone())),
            _ => None,
        })
        .expect("a ToolExecutionStarted for ask_user");
    assert_eq!(arguments["questions"][0]["id"], json!("loader"));
    assert_eq!(arguments["questions"][0]["kind"], json!("single"));
    assert_eq!(
        arguments["questions"][0]["options"][0]["label"],
        json!("Built-in")
    );

    // The answer: the settled result, both the readable content and the structured answers.
    let result = records
        .iter()
        .find_map(|record| match record {
            JournalRecord::ToolExecutionSettled {
                call_id: id,
                result,
            } if id == &call_id => Some(result.clone()),
            _ => None,
        })
        .expect("a ToolExecutionSettled for the ask_user call");
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        result.text(),
        "How should it load? → Built-in",
        "the human-readable answer"
    );
    let structured = result.structured.expect("the structured answers");
    assert_eq!(structured["request_id"], json!("q-7"));
    assert_eq!(structured["answers"][0]["id"], json!("loader"));
    assert_eq!(structured["answers"][0]["selected"], json!(["builtin"]));

    // The answer is a tool result, never user input: the transcript's user turns are the
    // prompt only.
    let transcript = chat_transcript(&records);
    let user_messages: Vec<&Message> = transcript
        .iter()
        .filter(|message| matches!(message, Message::User { .. }))
        .collect();
    assert_eq!(user_messages.len(), 1);
    assert_eq!(user_messages[0].text(), "go");
}

/// The transcript messages the journal recorded, in order (ADR 0002 E1 pairs each with its
/// call).
fn chat_transcript(records: &[JournalRecord]) -> Vec<Message> {
    records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

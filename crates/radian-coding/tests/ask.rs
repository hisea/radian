//! `ask_user` (ADR 0024): the Q2 limits, the Q4 outcomes, and the Q3 answer shape.
//!
//! Every test drives the tool through `ToolSession::execute`, the same path the core uses.

mod common;

use std::sync::{Arc, Mutex};

use agent_core::{
    CancellationToken, NoopSink, ToolCall, ToolConcurrency, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
use common::{TempDir, text};
use futures_util::future::BoxFuture;
use radian_coding::{
    AskTools, CodingTools, PromptError, Prompter, QuestionAnswer, QuestionReply, QuestionRequest,
};
use serde_json::{Value, json};

/// A prompter with one scripted reply (or none, so it never answers).
struct ScriptedPrompter {
    reply: Mutex<Option<Result<QuestionAnswer, PromptError>>>,
    seen: Mutex<Vec<QuestionRequest>>,
}

impl ScriptedPrompter {
    fn answering(answer: QuestionAnswer) -> Arc<Self> {
        Self::scripted(Some(Ok(answer)))
    }
    fn failing(error: PromptError) -> Arc<Self> {
        Self::scripted(Some(Err(error)))
    }
    /// Never returns: the ask hangs until it is cancelled.
    fn hanging() -> Arc<Self> {
        Self::scripted(None)
    }
    fn scripted(reply: Option<Result<QuestionAnswer, PromptError>>) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(reply),
            seen: Mutex::new(Vec::new()),
        })
    }
    fn seen(&self) -> Vec<QuestionRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Prompter for ScriptedPrompter {
    fn ask(&self, request: QuestionRequest) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>> {
        self.seen.lock().unwrap().push(request);
        let reply = self.reply.lock().unwrap().take();
        Box::pin(async move {
            match reply {
                Some(result) => result,
                None => {
                    std::future::pending::<()>().await;
                    unreachable!("the hanging prompter never returns")
                }
            }
        })
    }
}

fn workspace(label: &str) -> (TempDir, CodingTools) {
    let dir = TempDir::new(label);
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    (dir, CodingTools::new(&ws).unwrap())
}

async fn ask_with(
    tools: &AskTools,
    arguments: Value,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolFailure> {
    let session = tools.open_session().await.unwrap();
    let call = ToolCall {
        id: "ask-1".into(),
        name: "ask_user".into(),
        arguments,
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation,
        events: &sink,
        index: 0,
    };
    session.execute(&call, cx).await
}

async fn ask(tools: &AskTools, arguments: Value) -> Result<ToolOutput, ToolFailure> {
    ask_with(tools, arguments, CancellationToken::new()).await
}

/// A valid single-choice question under `id`.
fn question(id: &str) -> Value {
    json!({
        "id": id,
        "prompt": "Pick one",
        "kind": "single",
        "options": [
            {"id": "a", "label": "Option A"},
            {"id": "b", "label": "Option B", "description": "the second one"}
        ]
    })
}

fn args(questions: Vec<Value>) -> Value {
    json!({"questions": questions})
}

fn code(failure: &ToolFailure) -> &str {
    failure.code.as_str()
}

/// ADR 0024 Q5: a host with no interactive user does not mount the provider at all, so the
/// model never sees `ask_user`. The coding tools never advertise it either: it is its own
/// provider, so a host without a workspace still has it.
#[tokio::test]
async fn without_the_provider_the_tool_is_not_advertised() {
    let (_dir, coding) = workspace("ask-off");
    let session = coding.open_session().await.unwrap();
    let names: Vec<&str> = session
        .tool_specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(
        !names.contains(&"ask_user"),
        "the coding tools must not advertise ask_user: {names:?}"
    );
    // A hand-built call is a refusal, never a hang.
    let failure = session
        .execute(
            &ToolCall {
                id: "ask-1".into(),
                name: "ask_user".into(),
                arguments: args(vec![question("q")]),
            },
            ToolContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
                index: 0,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(&failure), "unknown_tool");
}

/// ADR 0024 Q1: with a prompter the tool appears, and it runs alone in its batch.
#[tokio::test]
async fn with_a_prompter_the_tool_is_advertised_and_exclusive() {
    let prompter = ScriptedPrompter::answering(QuestionAnswer::new("q-1", Vec::new()));
    let tools = AskTools::new(prompter);
    let session = tools.open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "ask_user")
        .expect("ask_user is advertised when a prompter is set");
    assert_eq!(spec.concurrency, ToolConcurrency::Exclusive);
}

/// ADR 0024 Q3: the answers are structured JSON, and the content is a readable line per
/// question.
#[tokio::test]
async fn the_answer_is_structured_json_and_readable_text() {
    let prompter = ScriptedPrompter::answering(QuestionAnswer::new(
        "q-17",
        vec![
            QuestionReply {
                id: "loader".into(),
                selected: vec!["builtin".into()],
                ..QuestionReply::default()
            },
            QuestionReply {
                id: "name".into(),
                text: Some("radian".into()),
                ..QuestionReply::default()
            },
            QuestionReply {
                id: "tiers".into(),
                selected: vec!["safe".into(), "workspace".into()],
                other: Some("a custom tier".into()),
                ..QuestionReply::default()
            },
        ],
    ));
    let tools = AskTools::new(prompter);
    let arguments = args(vec![
        json!({
            "id": "loader",
            "prompt": "How should the UI extension be loaded?",
            "kind": "single",
            "options": [
                {"id": "builtin", "label": "Built-in Rust components"},
                {"id": "process", "label": "External process plugins"}
            ],
            "allow_other": true,
            "required": true
        }),
        json!({"id": "name", "prompt": "What should it be called?", "kind": "text", "required": true}),
        json!({
            "id": "tiers",
            "prompt": "Which tiers ship?",
            "kind": "multi",
            "options": [
                {"id": "safe", "label": "Safe"},
                {"id": "workspace", "label": "Workspace"}
            ],
            "allow_other": true
        }),
    ]);
    let output = ask(&tools, arguments).await.unwrap();
    let structured = output.structured.clone().unwrap();
    assert_eq!(structured["request_id"], json!("q-17"));
    assert_eq!(structured["answers"][0]["id"], json!("loader"));
    assert_eq!(structured["answers"][0]["selected"], json!(["builtin"]));
    assert_eq!(structured["answers"][1]["text"], json!("radian"));
    assert_eq!(
        structured["answers"][2]["selected"],
        json!(["safe", "workspace"])
    );
    assert_eq!(structured["answers"][2]["other"], json!("a custom tier"));
    // A `text` reply carries no `selected`; an empty field is absent, not `[]`.
    assert!(structured["answers"][1].get("selected").is_none());

    // The human-readable content uses labels, not option ids, one line per question.
    let rendered = text(&output);
    assert_eq!(
        rendered,
        "How should the UI extension be loaded? → Built-in Rust components\n\
         What should it be called? → radian\n\
         Which tiers ship? → Safe, Workspace, other: a custom tier"
    );
}

/// ADR 0024 Q2: every limit is `invalid_arguments`, never a truncation.
#[tokio::test]
async fn every_question_limit_is_an_invalid_arguments_failure() {
    let cases: Vec<(&str, Value)> = vec![
        ("no questions at all", args(Vec::new())),
        (
            "9 questions",
            args((0..9).map(|i| question(&format!("q{i}"))).collect()),
        ),
        (
            "13 options",
            args(vec![json!({
                "id": "q",
                "prompt": "Pick one",
                "kind": "single",
                "options": (0..13)
                    .map(|i| json!({"id": format!("o{i}"), "label": format!("Option {i}")}))
                    .collect::<Vec<_>>()
            })]),
        ),
        ("an uppercase question id", args(vec![question("Loader")])),
        (
            "a 33-character question id",
            args(vec![question(&"a".repeat(33))]),
        ),
        (
            "a duplicate question id",
            args(vec![question("q"), question("q")]),
        ),
        (
            "a 501-character prompt",
            args(vec![json!({
                "id": "q", "prompt": "x".repeat(501), "kind": "text"
            })]),
        ),
        (
            "an 81-character label",
            args(vec![json!({
                "id": "q", "prompt": "Pick one", "kind": "single",
                "options": [{"id": "a", "label": "x".repeat(81)}]
            })]),
        ),
        (
            "a 201-character description",
            args(vec![json!({
                "id": "q", "prompt": "Pick one", "kind": "single",
                "options": [{"id": "a", "label": "A", "description": "x".repeat(201)}]
            })]),
        ),
        (
            "a text question with options",
            args(vec![json!({
                "id": "q", "prompt": "Say something", "kind": "text",
                "options": [{"id": "a", "label": "A"}]
            })]),
        ),
        (
            "an uppercase option id",
            args(vec![json!({
                "id": "q", "prompt": "Pick one", "kind": "single",
                "options": [{"id": "A", "label": "A"}]
            })]),
        ),
        (
            "a duplicate option id",
            args(vec![json!({
                "id": "q", "prompt": "Pick one", "kind": "single",
                "options": [{"id": "a", "label": "A"}, {"id": "a", "label": "B"}]
            })]),
        ),
        (
            "an unknown field",
            args(vec![json!({
                "id": "q", "prompt": "Pick one", "kind": "text", "extra": true
            })]),
        ),
    ];
    let dir = TempDir::new("ask-limits");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let prompter = ScriptedPrompter::answering(QuestionAnswer::new("q-1", Vec::new()));
    for (what, arguments) in cases {
        let prompter_dyn: Arc<dyn Prompter> = prompter.clone();
        let tools = AskTools::new(prompter_dyn);
        let failure = ask(&tools, arguments)
            .await
            .expect_err(&format!("{what} must be refused"));
        assert_eq!(code(&failure), "invalid_arguments", "{what}: {failure:?}");
    }
    // The prompter is never reached: a bad request is refused before asking.
    assert!(
        prompter.seen().is_empty(),
        "an invalid request must not reach the prompter"
    );
}

/// ADR 0024 Q3: the answer's ids must match the questions one-to-one.
#[tokio::test]
async fn an_answer_that_does_not_match_the_questions_is_rejected() {
    fn reply(id: &str) -> QuestionReply {
        QuestionReply {
            id: id.into(),
            text: Some("hi".into()),
            ..QuestionReply::default()
        }
    }
    let cases: Vec<(&str, Value, Vec<QuestionReply>)> = vec![
        (
            "a missing question",
            args(vec![question("q"), question("r")]),
            vec![reply("q")],
        ),
        (
            "an unknown question",
            args(vec![question("q")]),
            vec![reply("q"), reply("other")],
        ),
        (
            "a duplicated question",
            args(vec![question("q")]),
            vec![reply("q"), reply("q")],
        ),
    ];
    let dir = TempDir::new("ask-mismatch");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    for (what, request, answers) in cases {
        let prompter = ScriptedPrompter::answering(QuestionAnswer::new("q-1", answers));
        let tools = AskTools::new(prompter);
        let failure = ask(&tools, request).await.expect_err(what);
        assert_eq!(code(&failure), "invalid_answer", "{what}: {failure:?}");
    }
}

/// ADR 0024 Q4: cancellation wins the race and **no** answer is built.
#[tokio::test]
async fn a_cancelled_ask_is_cancelled_and_never_answers() {
    let prompter = ScriptedPrompter::hanging();
    let tools = AskTools::new(prompter);
    let token = CancellationToken::new();
    token.cancel();
    let failure = ask_with(&tools, args(vec![question("q")]), token)
        .await
        .expect_err("a cancelled ask is a failure, never a fabricated answer");
    assert_eq!(code(&failure), "cancelled");
}

/// ADR 0024 Q4: an explicit decline is `user_declined`, not an internal error.
#[tokio::test]
async fn a_declined_ask_is_user_declined() {
    let tools = AskTools::new(ScriptedPrompter::failing(PromptError::Declined));
    let failure = ask(&tools, args(vec![question("q")])).await.unwrap_err();
    assert_eq!(code(&failure), "user_declined");
}

/// ADR 0024 Q4: a dropped prompter (the UI exited, the session closed) is `prompter_gone`.
#[tokio::test]
async fn a_gone_prompter_is_prompter_gone() {
    let tools = AskTools::new(ScriptedPrompter::failing(PromptError::Gone));
    let failure = ask(&tools, args(vec![question("q")])).await.unwrap_err();
    assert_eq!(code(&failure), "prompter_gone");
}

/// ADR 0024 Q4: a prompter that itself fails is a distinct, visible failure.
#[tokio::test]
async fn a_failed_prompter_is_prompter_error() {
    let tools = AskTools::new(ScriptedPrompter::failing(PromptError::Failed(
        "the channel closed".into(),
    )));
    let failure = ask(&tools, args(vec![question("q")])).await.unwrap_err();
    assert_eq!(code(&failure), "prompter_error");
    assert!(failure.message.contains("the channel closed"));
}

/// ADR 0024 Q3: a `text` question renders its free text, and an unanswered question renders
/// `(no answer)` rather than an empty line.
#[tokio::test]
async fn a_text_answer_renders_its_text() {
    let prompter = ScriptedPrompter::answering(QuestionAnswer::new(
        "q-2",
        vec![
            QuestionReply {
                id: "name".into(),
                text: Some("radian".into()),
                ..QuestionReply::default()
            },
            QuestionReply {
                id: "note".into(),
                ..QuestionReply::default()
            },
        ],
    ));
    let tools = AskTools::new(prompter);
    let output = ask(
        &tools,
        args(vec![
            json!({"id": "name", "prompt": "What should it be called?", "kind": "text"}),
            json!({"id": "note", "prompt": "Anything else?", "kind": "text"}),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&output),
        "What should it be called? → radian\nAnything else? → (no answer)"
    );
}

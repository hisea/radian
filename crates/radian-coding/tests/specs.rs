//! The tool specs must be accepted by `agent-core`'s preflight (`ToolRegistry::new` compiles
//! every `input_schema` and rejects duplicate names).

mod common;

use std::sync::Arc;

use agent_core::{
    Agent, Model, ModelContext, ModelError, ModelRequest, ModelResponse, ToolProvider, ToolSession,
};
use common::{TempDir, tools};

/// Never called: opening a session only needs the trait to be implemented.
struct NeverModel;

impl Model for NeverModel {
    async fn complete(
        &self,
        _request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        Err(ModelError::Protocol("not used".into()))
    }
}

#[tokio::test]
async fn agent_core_accepts_the_built_in_tool_specs() {
    let dir = TempDir::new("specs");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let coding = tools(&root);

    // Directly: the specs exist with the expected names and valid schemas.
    let session = coding.open_session().await.unwrap();
    let names: Vec<&str> = session
        .tool_specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["read", "write", "patch", "list", "git_status", "git_diff"]
    );

    // Through the core: `open_session` compiles every schema and refuses duplicates.
    let agent = Arc::new(Agent::new(NeverModel, coding));
    let mut chat = agent.open_session().await.unwrap();
    chat.close().await.unwrap();
}

/// D25: the `read` spec offers `offset`/`limit` so a large file can be read in sections, and
/// its description says not to raise `max_bytes` to reach a later part.
#[tokio::test]
async fn the_read_spec_offers_offset_and_limit() {
    let dir = TempDir::new("read-spec");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let coding = tools(&root);
    let session = coding.open_session().await.unwrap();
    let read = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "read")
        .expect("the read spec");
    let properties = &read.input_schema["properties"];
    for field in ["path", "max_bytes", "offset", "limit"] {
        assert!(properties.get(field).is_some(), "missing {field}");
    }
    assert_eq!(properties["offset"]["minimum"], 1);
    assert_eq!(properties["limit"]["minimum"], 1);
    assert!(read.description.contains("offset"), "{}", read.description);
    assert!(
        read.description.contains("do NOT raise max_bytes"),
        "{}",
        read.description
    );
}

/// ADR 0024 Q1: with a prompter, `ask_user` is appended and its schema passes the same
/// preflight (`ToolRegistry::new` compiles every `input_schema` and rejects duplicates).
#[tokio::test]
async fn agent_core_accepts_the_ask_user_spec() {
    use futures_util::future::BoxFuture;
    use radian_coding::{PromptError, Prompter, QuestionAnswer, QuestionRequest};

    struct NeverPrompter;

    impl Prompter for NeverPrompter {
        fn ask(
            &self,
            _request: QuestionRequest,
        ) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>> {
            Box::pin(async { Err(PromptError::Gone) })
        }
    }

    // `ask_user` is its own provider (ADR 0024 Q5), so it is accepted on its own too — that is
    // what lets a host with no workspace mount it.
    let ask = radian_coding::AskTools::new(Arc::new(NeverPrompter));

    let session = ask.open_session().await.unwrap();
    let names: Vec<&str> = session
        .tool_specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["ask_user"]);

    let agent = Arc::new(Agent::new(NeverModel, ask));
    let mut chat = agent.open_session().await.unwrap();
    chat.close().await.unwrap();
}

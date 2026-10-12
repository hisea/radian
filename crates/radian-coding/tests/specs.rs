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
    assert_eq!(names, ["read", "write", "patch", "list"]);

    // Through the core: `open_session` compiles every schema and refuses duplicates.
    let agent = Arc::new(Agent::new(NeverModel, coding));
    let mut chat = agent.open_session().await.unwrap();
    chat.close().await.unwrap();
}

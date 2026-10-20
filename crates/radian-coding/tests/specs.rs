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
        [
            "read",
            "write",
            "patch",
            "list",
            "glob",
            "grep",
            "git_diff",
            // The one-version aliases (ADR 0043 decision 3): `list` for `read` on a
            // directory, `git_status` for `git_diff { stat: true }`. Both are registered
            // so a call to an old name passes Core's preflight, and the host strips
            // them from every model request, so the model never sees them.
            "git_status"
        ]
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

/// ADR 0043 decision 1: `read` on a directory lists it. The description says so in one
/// sentence (decision 5's budget: the serialized spec stays within 150 bytes of the
/// file-only definition it replaced).
#[tokio::test]
async fn the_read_spec_names_directory_listing() {
    let dir = TempDir::new("read-dir-spec");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let coding = tools(&root);
    let session = coding.open_session().await.unwrap();
    let specs: std::collections::HashMap<&str, &agent_core::ToolSpec> = session
        .tool_specs()
        .iter()
        .map(|spec| (spec.name.as_str(), spec))
        .collect();
    let read = specs["read"];
    assert!(
        read.description.contains("A directory path lists it"),
        "{}",
        read.description
    );
    // Decision 5's size budget: the file-only `read` serialized to 1601 bytes (measured
    // 2026-10-20); the directory sentence fits in 150 bytes on top of that, and the `limit`
    // directory-entry note (item 10) in 67 more; the pin leaves a little room above the
    // measured 1778.
    let bytes = serde_json::to_vec(read).unwrap().len();
    assert!(
        bytes <= 1790,
        "the read definition grew to {bytes} bytes, past the 1790 budget"
    );
    // The `list` alias keeps its old schema (so a stale call still parses) and its
    // description names the rename.
    let list = specs["list"];
    assert!(
        list.description.contains("Deprecated alias for `read`"),
        "{}",
        list.description
    );
    assert!(
        list.input_schema["properties"].get("max_entries").is_some(),
        "the alias keeps the old arguments: {}",
        list.input_schema
    );
}

/// Opens an `AskTools` session (no workspace needed, ADR 0024 Q5) and returns the
/// advertised `ask_user` spec. Small helper for the description pins below.
async fn ask_user_spec() -> agent_core::ToolSpec {
    use futures_util::future::BoxFuture;
    use radian_coding::{PromptError, Prompter, QuestionAnswer, QuestionRequest};

    struct SilentPrompter;

    impl Prompter for SilentPrompter {
        fn ask(
            &self,
            _request: QuestionRequest,
        ) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>> {
            Box::pin(async { Err(PromptError::Gone) })
        }
    }

    let ask = radian_coding::AskTools::new(Arc::new(SilentPrompter));
    let session = ask.open_session().await.unwrap();
    session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "ask_user")
        .expect("ask_user is advertised when a prompter is set")
        .clone()
}

/// ADR 0024 decision 2 (`ask-user-pass`): the description itself states when to ask —
/// only a decision that belongs to the user, or a real ambiguity looking cannot
/// resolve — and tells the model to mark its recommendation.
#[tokio::test]
async fn the_ask_user_description_states_when_to_ask() {
    let spec = ask_user_spec().await;
    assert!(
        spec.description.contains("belongs to the user"),
        "ask only for the user's own decisions: {}",
        spec.description
    );
    assert!(
        spec.description.contains("do not ask what you can find"),
        "look before you ask (ADR 0036): {}",
        spec.description
    );
    assert!(
        spec.description.contains("recommend"),
        "give a recommendation when asking: {}",
        spec.description
    );
}

/// `ask-user-pass` decision 3: the slimmed definition serializes to 1618 bytes
/// (measured 2026-10-20: 1584 after slimming, +49 for the header width note restored in
/// fix round 1, -15 for dropping the `header` `maxLength` that counted characters instead
/// of display cells); the pin leaves a little room above that. The `header` field names
/// the width limit so a CJK-writing user does not pay an error round trip for it.
#[tokio::test]
async fn the_ask_user_definition_stays_small() {
    let spec = ask_user_spec().await;
    let bytes = serde_json::to_vec(&spec).unwrap().len();
    assert!(
        bytes <= 1630,
        "the ask_user definition grew to {bytes} bytes, past the 1630 budget"
    );
    let header = &spec.input_schema["properties"]["questions"]["items"]["properties"]["header"];
    assert!(
        header["description"]
            .as_str()
            .is_some_and(|text| text.contains("16 display cells")),
        "the header description must name the width limit: {header}"
    );
}

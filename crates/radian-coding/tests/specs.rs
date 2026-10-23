//! The tool specs must be accepted by `radian-core`'s preflight (`ToolRegistry::new` compiles
//! every `input_schema` and rejects duplicate names).

mod common;

use std::sync::Arc;

use common::{TempDir, call, tools};
use radian_coding::CodingTools;
use radian_core::{
    Agent, Model, ModelContext, ModelError, ModelRequest, ModelResponse, ToolProvider, ToolSession,
};

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
async fn radian_core_accepts_the_built_in_tool_specs() {
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
    assert_eq!(names, ["read", "write", "patch", "glob", "grep"]);

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
    assert!(properties["offset"].get("minimum").is_none());
    assert_eq!(properties["limit"]["minimum"], 1);
    assert!(
        properties["offset"]["description"]
            .as_str()
            .is_some_and(|text| text.contains("from the end")),
        "offset names the from-the-end read: {}",
        properties["offset"]
    );
    assert!(read.description.contains("offset"), "{}", read.description);
    assert!(
        read.description.contains("Do not increase `max_bytes`"),
        "{}",
        read.description
    );
}

/// `write` creates a missing parent directory, and its description says so instead of the old
/// "parent directory must exist" rule (ADR 0039 decision 2).
#[tokio::test]
async fn the_write_spec_says_parents_are_created() {
    let dir = TempDir::new("write-spec");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let coding = tools(&root);
    let session = coding.open_session().await.unwrap();
    let write = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "write")
        .expect("the write spec");
    assert!(
        write
            .description
            .contains("Missing parent directories are created"),
        "{}",
        write.description
    );
    assert!(
        !write.description.contains("parent directory must exist"),
        "{}",
        write.description
    );
}

/// ADR 0024 Q1: with a prompter, `ask_user` is appended and its schema passes the same
/// preflight (`ToolRegistry::new` compiles every `input_schema` and rejects duplicates).
#[tokio::test]
async fn radian_core_accepts_the_ask_user_spec() {
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

/// ADR 0043 decision 1: `read` on a directory lists it; the image branch says what it does.
/// The description says both in one sentence each (decision 5's budget: the serialized spec
/// stays within 1500 bytes).
#[tokio::test]
async fn the_read_spec_names_directory_listing() {
    let dir = TempDir::new("read-dir-spec");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let coding = tools(&root);
    let session = coding.open_session().await.unwrap();
    let specs: std::collections::HashMap<&str, &radian_core::ToolSpec> = session
        .tool_specs()
        .iter()
        .map(|spec| (spec.name.as_str(), spec))
        .collect();
    let read = specs["read"];
    assert!(
        read.description.contains("a directory") && read.description.contains("image"),
        "{}",
        read.description
    );
    // The size budget: the directory description fit 1078 bytes (measured 2026-10-20); the
    // image branch adds its format, limits and error code, which cannot fit under the old
    // 1100. The pin leaves a little room above the measured 1430.
    let bytes = serde_json::to_vec(read).unwrap().len();
    assert!(
        bytes <= 1500,
        "the read definition grew to {bytes} bytes, past the 1500 budget"
    );
    // No removed name is advertised: a call to one is an unknown tool.
    for name in [
        "git_diff",
        "list",
        "git_status",
        "job_output",
        "job_list",
        "job_kill",
        "shell_output",
    ] {
        assert!(
            session.tool_specs().iter().all(|spec| spec.name != name),
            "{name} must not be advertised"
        );
    }
}

/// Opens an `AskTools` session (no workspace needed, ADR 0024 Q5) and returns the
/// advertised `ask_user` spec. Small helper for the description pins below.
async fn ask_user_spec() -> radian_core::ToolSpec {
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

/// ADR 0024 Q2 (`ask-user-pass`): the description itself states when to ask —
/// only a decision that belongs to the user, or a real ambiguity looking cannot
/// resolve — and tells the model to mark its recommendation.
#[tokio::test]
async fn the_ask_user_description_states_when_to_ask() {
    let spec = ask_user_spec().await;
    assert!(
        spec.description.contains("belong to the user"),
        "ask only for the user's own decisions: {}",
        spec.description
    );
    assert!(
        spec.description.contains("Do not ask what you can find"),
        "look before you ask (ADR 0036): {}",
        spec.description
    );
    assert!(
        spec.description.contains("recommend"),
        "give a recommendation when asking: {}",
        spec.description
    );
}

/// `ask-user-pass` decision 3: the STE definition serializes to 1663 bytes
/// (measured 2026-10-20); the pin leaves a little room above that. The `header` field names
/// the width limit so a CJK-writing user does not pay an error round trip for it.
#[tokio::test]
async fn the_ask_user_definition_stays_small() {
    let spec = ask_user_spec().await;
    let bytes = serde_json::to_vec(&spec).unwrap().len();
    assert!(
        bytes <= 1700,
        "the ask_user definition grew to {bytes} bytes, past the 1700 budget"
    );
    let header = &spec.input_schema["properties"]["questions"]["items"]["properties"]["header"];
    assert!(
        header["description"]
            .as_str()
            .is_some_and(|text| text.contains("16 display cells")),
        "the header description must name the width limit: {header}"
    );
}

/// The seven removed names are unknown tools in every session shape: `git_diff` outright,
/// and the six old names (`list`, `git_status`, `job_output`, `job_list`, `job_kill`,
/// `shell_output`). Neither the plain session nor the one with `shell`+`job` advertises
/// them, and calling one returns the normal unknown-tool error.
#[tokio::test]
async fn removed_tool_names_are_unknown_in_every_session_shape() {
    use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};

    let dir = TempDir::new("removed-names");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let jobs_dir = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(jobs_dir.parent().unwrap()).unwrap();

    let plain = tools(&root);
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, &root).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full-access needs no backend");
    let with_shell = CodingTools::with_sandbox(&root, sandbox)
        .expect("matching workspaces")
        .with_jobs(std::sync::Arc::new(radian_coding::Jobs::new(&jobs_dir)));

    for name in [
        "git_diff",
        "list",
        "git_status",
        "job_output",
        "job_list",
        "job_kill",
        "shell_output",
    ] {
        for coding in [&plain, &with_shell] {
            let session = coding.open_session().await.unwrap();
            assert!(
                session.tool_specs().iter().all(|spec| spec.name != name),
                "{name} must not be advertised"
            );
            let failure = call(coding, name, serde_json::json!({}))
                .await
                .expect_err("a removed name is unknown");
            assert_eq!(failure.code, "unknown_tool", "{name}: {failure:?}");
        }
    }
}

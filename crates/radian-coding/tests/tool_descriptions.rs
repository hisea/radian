//! Say-each-fact-once (part A): no parameter description repeats a sentence of its
//! tool description, for `shell`, `job`, `read` and `patch` (`skill` is pinned in
//! `radian-host`). Plus the `full`-tier `outside_sandbox` parse path (part A1).

mod common;

use std::ffi::{OsStr, OsString};
use std::process::Command;

use agent_core::{ToolProvider, ToolSession, ToolSpec};
use common::{TempDir, call, text};
use radian_coding::CodingTools;
use radian_sandbox::{
    ProbeReport, Sandbox, SandboxBackend, SandboxCommand, SandboxError, SandboxMode, SandboxPolicy,
};
use serde_json::json;

/// A backend with a fixed git answer, so both `workspace` wordings are testable on any
/// platform (the same seam `shell.rs`'s `GitBackend` uses).
struct GitBackend(bool);

impl SandboxBackend for GitBackend {
    fn name(&self) -> &'static str {
        "test-desc-git"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        Ok(ProbeReport {
            backend: self.name(),
            detail: "test".into(),
        })
    }

    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(&policy.workspace);
        Ok(SandboxCommand::new(command))
    }

    fn git_writable_except_exec(&self, _policy: &SandboxPolicy) -> bool {
        self.0
    }
}

static GIT_WRITABLE: GitBackend = GitBackend(true);
static GIT_READ_ONLY: GitBackend = GitBackend(false);

/// `FullAccess` needs no backend, so it runs everywhere.
fn transparent_tools(root: &std::path::Path) -> CodingTools {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, root).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full-access needs no backend");
    CodingTools::with_sandbox(root, sandbox).expect("matching workspaces")
}

fn workspace_tools(root: &std::path::Path, backend: &'static dyn SandboxBackend) -> CodingTools {
    let sandbox =
        Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, root).expect("sandbox");
    CodingTools::with_sandbox(root, sandbox).expect("matching workspaces")
}

/// The tool description's sentences: split on `. `, trimmed. Fragments under 24 characters
/// are too short to be a repeated fact (subjects like "Pass its job id.").
fn sentences(description: &str) -> Vec<&str> {
    description
        .split(". ")
        .map(str::trim)
        .filter(|sentence| sentence.len() >= 24)
        .collect()
}

/// No top-level parameter description repeats one of the tool description's sentences. All
/// four specs here are flat (their only nesting is the `enum`/`type` keywords, never a
/// sub-description), so the top-level `properties` are the whole surface.
fn assert_no_param_repeats_description(spec: &ToolSpec) {
    let properties = spec.input_schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{} has no properties", spec.name));
    for sentence in sentences(&spec.description) {
        for (name, property) in properties {
            if let Some(text) = property["description"].as_str() {
                assert!(
                    !text.contains(sentence),
                    "`{}` repeats the description sentence {sentence:?} in `{name}`: {text:?}",
                    spec.name,
                );
            }
        }
    }
}

async fn specs(tools: &CodingTools) -> Vec<ToolSpec> {
    tools.open_session().await.unwrap().tool_specs().to_vec()
}

fn named(specs: &[ToolSpec], name: &str) -> ToolSpec {
    specs
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("the {name} spec"))
        .clone()
}

#[tokio::test]
async fn shell_params_repeat_no_description_sentence() {
    let dir = TempDir::new("desc-shell");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    assert_no_param_repeats_description(&named(&specs(&transparent_tools(&root)).await, "shell"));
    for backend in [&GIT_WRITABLE as &dyn SandboxBackend, &GIT_READ_ONLY] {
        let spec = named(&specs(&workspace_tools(&root, backend)).await, "shell");
        assert_no_param_repeats_description(&spec);
    }
}

#[tokio::test]
async fn job_params_repeat_no_description_sentence() {
    let dir = TempDir::new("desc-job");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    assert_no_param_repeats_description(&named(&specs(&transparent_tools(&root)).await, "job"));
}

#[tokio::test]
async fn read_and_patch_params_repeat_no_description_sentence() {
    let dir = TempDir::new("desc-read-patch");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let specs = specs(&common::tools(&root)).await;
    assert_no_param_repeats_description(&named(&specs, "read"));
    assert_no_param_repeats_description(&named(&specs, "patch"));
}

/// Part A1: `run` still parses with the full schema, so a `full`-tier call that sends
/// `outside_sandbox` behaves exactly as before (the advertised schema simply omits the
/// fields). The reason/background refusals under `transparent_tools` already prove the parse
/// path (`outside_sandbox_without_a_reason_is_refused`, `outside_sandbox_refuses_background`);
/// this is the success leg.
#[tokio::test]
async fn a_full_tier_outside_call_still_runs() {
    let dir = TempDir::new("desc-full-outside");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let output = call(
        &tools,
        "shell",
        json!({
            "command": "echo FULL-OUTSIDE",
            "outside_sandbox": true,
            "outside_sandbox_reason": "the test leaves the sandbox on purpose",
        }),
    )
    .await
    .expect("a full-tier outside call still parses and runs");
    let body = text(&output);
    assert!(body.contains("FULL-OUTSIDE"), "{body}");
    assert!(
        body.contains("ran outside the sandbox")
            && body.contains("the test leaves the sandbox on purpose"),
        "the result must name the unsandboxed run and its reason: {body}"
    );
}

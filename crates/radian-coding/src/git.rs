use std::process::Stdio;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::workspace::WorkspaceRoot;

pub(crate) fn status_spec() -> ToolSpec {
    ToolSpec {
        name: "git_status".into(),
        description: "Run `git status --porcelain=v1 --branch` in the workspace (read-only). \
                      Output is capped and never touches .git beyond reading."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Shared,
    }
}

pub(crate) fn diff_spec() -> ToolSpec {
    ToolSpec {
        name: "git_diff".into(),
        description: "Run `git diff` (optionally `--cached`) in the workspace for the whole \
                      tree or one workspace-relative path (read-only)."
            .into(),
        input_schema: diff_schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

pub(crate) fn diff_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "staged": {
                "type": "boolean",
                "description": "Diff the index against HEAD instead of the worktree."
            },
            "path": {
                "type": "string",
                "description": "Workspace-relative path to limit the diff to."
            }
        },
        "additionalProperties": false
    })
}

#[derive(Deserialize, Default)]
struct DiffArgs {
    staged: Option<bool>,
    path: Option<String>,
}

pub(crate) async fn status(
    root: &WorkspaceRoot,
    limits: &Limits,
    _arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let output = run_git(
        root,
        limits,
        &[
            "status".into(),
            "--porcelain=v1".into(),
            "--branch".into(),
            "--untracked-files=all".into(),
        ],
    )
    .await?;
    Ok(ToolOutput {
        content: vec![ContentPart::text(output)],
        structured: None,
    })
}

pub(crate) async fn diff(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: DiffArgs = args::parse("git_diff", &diff_schema(), arguments)?;
    let mut argv: Vec<String> = vec!["diff".into(), "--no-color".into()];
    if args.staged.unwrap_or(false) {
        argv.push("--cached".into());
    }
    if let Some(path) = &args.path
        && path != "."
        && path != "./"
    {
        // The pathspec is checked through the root's directory fd (ADR 0039 decision 2: the
        // parents are walked with `O_NOFOLLOW`, the leaf is `lstat`ed and must not be a
        // link) and passed to git relative to the repository, after `--`. git opens the
        // tracked content itself.
        let relative = root.relative_checked(path)?;
        if relative.as_os_str().is_empty() {
            return Err(fail(
                "path_outside_workspace",
                format!("`{path}` does not name a file or directory"),
            ));
        }
        argv.push("--".into());
        argv.push(relative.to_string_lossy().into_owned());
    }
    let output = run_git(root, limits, &argv).await?;
    Ok(ToolOutput {
        content: vec![ContentPart::text(output)],
        structured: None,
    })
}

/// Runs one `git` invocation. `--no-optional-locks` (and `GIT_OPTIONAL_LOCKS=0`) keeps git
/// from refreshing the index, so a read-only tool never writes `.git`.
async fn run_git(
    root: &WorkspaceRoot,
    limits: &Limits,
    argv: &[String],
) -> Result<String, ToolFailure> {
    // ADR 0039 decision 3: a host git call carries the shared no-filter overrides.
    let mut command = tokio::process::Command::from(
        crate::host_git::host_git_command(root.root())
            .map_err(|error| fail("git_unavailable", error))?,
    );
    command.arg("--no-optional-locks").arg("--no-pager");
    command.args(argv);
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = tokio::time::timeout(limits.git_timeout, command.output())
        .await
        .map_err(|_| {
            fail(
                "git_timeout",
                format!("git did not finish within {:?}", limits.git_timeout),
            )
        })?;
    let output = child.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            fail("git_unavailable", "the `git` executable was not found")
        } else {
            fail("git_failed", format!("could not run git: {e}"))
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(fail(
            "git_failed",
            format!(
                "git {} exited with {}: {}",
                argv.join(" "),
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ),
        ));
    }
    Ok(truncate_utf8(
        String::from_utf8_lossy(&output.stdout).into_owned(),
        limits.max_git_output_bytes,
    ))
}

/// Truncates on a character boundary and marks the cut, so the result is still text.
fn truncate_utf8(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[git output truncated]\n");
    text
}

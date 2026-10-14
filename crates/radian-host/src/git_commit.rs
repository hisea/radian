//! `git_commit`: the host-side commit tool (ADR 0030 R10).
//!
//! The `workspace` tier keeps `.git` read-only (ADR 0016), so a model cannot `git commit` from
//! the sandbox. This tool runs in the **host process**, outside the sandbox: it is the one
//! write path for a commit.
//!
//! What it refuses, by construction:
//!
//! - It never runs a shell: every argument is argv.
//! - It never runs a workspace hook or filter: `--no-verify` and `-c core.hooksPath=/dev/null`
//!   are on every call. A project's `core.hooksPath` (e.g. husky's `.husky/`) lives in the
//!   worktree the model can write, so running it in the host would execute agent-written
//!   scripts as the user (ADR 0030).
//! - `paths` must resolve inside the workspace (the file tools' own boundary: absolute paths,
//!   `..` and symlinks out are refused).
//! - It refuses `main`/`master`, a configured protected branch, a detached HEAD, and — when a
//!   session is restricted to one branch (`radian/<id>`) — any other branch.
//! - It only adds and commits: no push, no branch, no amend, no rebase.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{
    ContentPart, ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure, ToolOutput,
    ToolProvider, ToolSession, ToolSpec,
};
use radian_coding::WorkspaceRoot;
use radian_session::{RuntimeEvent, SessionJournal};
use serde::Deserialize;
use serde_json::json;

fn fail(code: &str, message: impl Into<String>) -> ToolFailure {
    ToolFailure::new(code, message)
}

/// The `git_commit` spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "git_commit".into(),
        description: "Commit the workspace's changes on the current branch. Runs in the host, \
                      outside the sandbox (the sandbox keeps `.git` read-only, so `git commit` \
                      there fails by design). It stages with `git add` and commits with \
                      `git commit --no-verify`; it never runs workspace hooks (a project's \
                      `core.hooksPath` lives in the worktree you can write, so the host would \
                      be running your scripts as the user). It refuses a detached HEAD, \
                      `main`/`master` and configured protected branches, and never pushes, \
                      branches, amends or rebases. `paths` are workspace-relative; with none, \
                      every change is staged (`git add -A`, so `.gitignore` still applies)."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The commit message."
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Workspace-relative paths to stage. Omit to stage every \
                                    change."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Exclusive,
    }
}

#[derive(Deserialize)]
struct CommitArgs {
    message: String,
    paths: Option<Vec<String>>,
}

/// One successful commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub commit: String,
    pub branch: String,
    pub files: usize,
}

/// The `git_commit` tool. `workspace` is the session's root; `journal` receives the audit event.
#[derive(Clone)]
pub struct GitCommitTools {
    workspace: PathBuf,
    journal: Arc<SessionJournal>,
    /// When set, only this branch may be committed to (a task agent's `radian/<id>`).
    allowed_branch: Option<String>,
    /// Branch names that are refused besides `main`/`master` (`[git] protected_branches`).
    protected_branches: Vec<String>,
}

impl GitCommitTools {
    pub fn new(workspace: impl Into<PathBuf>, journal: Arc<SessionJournal>) -> Self {
        Self {
            workspace: workspace.into(),
            journal,
            allowed_branch: None,
            protected_branches: Vec::new(),
        }
    }

    /// Restricts commits to one branch (the orchestration slices pass `radian/<id>`; the
    /// interactive session passes `None`).
    pub fn with_allowed_branch(mut self, branch: Option<String>) -> Self {
        self.allowed_branch = branch;
        self
    }

    pub fn with_protected_branches(mut self, branches: Vec<String>) -> Self {
        self.protected_branches = branches;
        self
    }
}

impl ToolProvider for GitCommitTools {
    type Session = GitCommitSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(GitCommitSession {
            specs: vec![spec()],
        })
    }
}

pub struct GitCommitSession {
    specs: Vec<ToolSpec>,
}

impl ToolSession for GitCommitSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        _call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        Err(fail(
            "unknown_tool",
            "`git_commit` is executed by the host tool provider, not this session",
        ))
    }
}

/// Runs the tool for one call: the session variant is never used; the host provider calls this
/// directly (it needs the workspace, the journal and the branch rules, which the plain
/// `ToolSession` trait cannot carry).
pub async fn run(tools: &GitCommitTools, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
    if call.name != "git_commit" {
        return Err(fail("unknown_tool", "`git_commit` is the only tool here"));
    }
    let args: CommitArgs = serde_json::from_value(call.arguments.clone())
        .map_err(|error| fail("invalid_arguments", error.to_string()))?;
    if args.message.trim().is_empty() {
        return Err(fail("invalid_arguments", "`message` must not be empty"));
    }
    let workspace = tools.workspace.clone();
    let allowed = tools.allowed_branch.clone();
    let protected = tools.protected_branches.clone();
    let message = args.message;
    let paths = args.paths.unwrap_or_default();
    let committed = tokio::task::spawn_blocking(move || {
        commit(&workspace, &message, &paths, allowed.as_deref(), &protected)
    })
    .await
    .map_err(|error| {
        fail(
            "git_commit_failed",
            format!("the commit task failed: {error}"),
        )
    })??;

    // Audit (ADR 0030 R10): a non-critical record of who committed what. Best-effort: the
    // commit already happened, so a write failure is not the caller's problem.
    let _ = tools
        .journal
        .append_runtime_event(RuntimeEvent::GitCommitted {
            commit: committed.commit.clone(),
            branch: committed.branch.clone(),
            files: committed.files,
        })
        .await;

    let short = committed.commit.get(..8).unwrap_or(&committed.commit);
    Ok(ToolOutput {
        content: vec![ContentPart::text(format!(
            "committed {short} on {} ({} file{})",
            committed.branch,
            committed.files,
            if committed.files == 1 { "" } else { "s" }
        ))],
        structured: Some(json!({
            "commit": committed.commit,
            "branch": committed.branch,
            "files": committed.files,
        })),
    })
}

/// The blocking part: branch checks, the path boundary, `git add` and `git commit`.
fn commit(
    workspace: &Path,
    message: &str,
    paths: &[String],
    allowed_branch: Option<&str>,
    protected: &[String],
) -> Result<Committed, ToolFailure> {
    let branch = git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return Err(fail(
            "detached_head",
            "HEAD is detached; check out a branch before committing",
        ));
    }
    if branch == "main" || branch == "master" || protected.iter().any(|name| name == &branch) {
        return Err(fail(
            "protected_branch",
            format!(
                "refusing to commit on `{branch}` (main, master and configured protected \
                 branches are refused); commit on a feature branch"
            ),
        ));
    }
    if let Some(allowed) = allowed_branch
        && branch != allowed
    {
        return Err(fail(
            "wrong_branch",
            format!("this session may only commit to `{allowed}`, not `{branch}`"),
        ));
    }

    // The path boundary is the file tools' own: absolute paths, `..` and symlinks out of the
    // workspace are refused before git runs.
    let root = WorkspaceRoot::new(workspace)
        .map_err(|error| fail("workspace_unavailable", error.to_string()))?;
    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths {
        resolved.push(
            root.resolve_in_workspace(path)
                .map_err(|error| fail("outside_workspace", error))?,
        );
    }

    if paths.is_empty() {
        run_git(workspace, &["-c", "core.hooksPath=/dev/null", "add", "-A"])?;
    } else {
        let mut args = vec!["-c", "core.hooksPath=/dev/null", "add", "--"];
        let owned: Vec<String> = resolved
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        for path in &owned {
            args.push(path.as_str());
        }
        run_git(workspace, &args)?;
    }

    // `--no-verify` skips pre-commit/commit-msg; `core.hooksPath=/dev/null` is belt and
    // braces (a hook could still be found by another path).
    run_git(
        workspace,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--no-verify",
            "-m",
            message,
        ],
    )?;

    let commit = git(workspace, &["rev-parse", "HEAD"])?;
    let changed = git(
        workspace,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )?;
    let files = changed.lines().filter(|line| !line.is_empty()).count();
    Ok(Committed {
        commit,
        branch,
        files,
    })
}

/// Runs `git` in `workspace` and returns its trimmed stdout. A non-zero exit is an error with
/// the command (never a secret) and git's stderr.
fn git(workspace: &Path, args: &[&str]) -> Result<String, ToolFailure> {
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args(args)
        .output()
        .map_err(|error| fail("git_unavailable", format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(fail(
            "git_failed",
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Runs `git` for its side effect.
fn run_git(workspace: &Path, args: &[&str]) -> Result<(), ToolFailure> {
    git(workspace, args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_takes_a_message_and_optional_paths() {
        let spec = spec();
        assert_eq!(spec.name, "git_commit");
        assert_eq!(spec.input_schema["required"], json!(["message"]));
        assert!(spec.description.contains("--no-verify"));
        assert!(spec.description.contains("outside the sandbox"));
    }
}

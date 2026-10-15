//! `git_commit`: the host-side commit tool (ADR 0030 R10).
//!
//! The `workspace` tier keeps `.git` read-only (ADR 0016), so a model cannot `git commit` from
//! the sandbox. This tool runs in the **host process**, outside the sandbox: it is the one
//! write path for a commit.
//!
//! What it refuses, by construction:
//!
//! - It never runs a shell: every argument is argv.
//! - It never runs a workspace **hook**: `--no-verify` and `-c core.hooksPath=/dev/null` are on
//!   every call. A project's `core.hooksPath` (e.g. husky's `.husky/`) lives in the worktree the
//!   model can write, so running it in the host would execute agent-written scripts as the user
//!   (ADR 0030).
//! - It never runs a workspace **filter driver** it was not told to allow (ADR 0030):
//!   a `filter=<driver>` attribute in the writable `.gitattributes` makes `git add` run
//!   `filter.<driver>.clean`/`.process` from git config (repo or global). Before staging, the
//!   paths are read with `git check-attr filter`, and every driver must be in `[git]
//!   allowed_filters` (default `["lfs"]`); `lfs` is only allowed when its `clean`/`process`
//!   commands are unset or a `git-lfs` command. Anything else refuses and stages nothing.
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

/// The `[git] allowed_filters` default (ADR 0030): only Git LFS may run a
/// clean/process driver in the host.
pub fn default_allowed_filters() -> Vec<String> {
    vec!["lfs".to_owned()]
}

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
                      be running your scripts as the user), and it refuses a path whose \
                      `.gitattributes` `filter` names a driver outside `[git] allowed_filters`. \
                      It refuses a detached HEAD, \
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

/// The repository pinned when the session opened, **before** the model ran (ADR 0030
/// R10). Every `git` call uses these paths explicitly, and every call re-resolves them and
/// refuses when they changed: a model that creates a `.git/` in a workspace that was not one
/// (or is a subdirectory of one) cannot redirect the host's git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPin {
    git_dir: PathBuf,
    common_dir: PathBuf,
    work_tree: PathBuf,
    /// Whether the workspace root had a `.git` when the pin was taken. A `.git` that appears
    /// later is a repository swap; the filesystem check catches it **before** any `git` runs
    /// (running `git rev-parse` on the swapped repository would read its config).
    workspace_had_git: bool,
}

impl RepoPin {
    /// Resolves the repository from `workspace`, or `None` when it is not a git repository (the
    /// session then does not mount `git_commit` at all).
    pub fn resolve(workspace: &Path) -> Option<Self> {
        let git_dir = rev_parse(workspace, "--absolute-git-dir")?;
        let common = rev_parse(workspace, "--git-common-dir")?;
        let work_tree = rev_parse(workspace, "--show-toplevel")?;
        Some(Self {
            git_dir: canonicalize(Path::new(&git_dir))?,
            common_dir: if Path::new(&common).is_absolute() {
                canonicalize(Path::new(&common))?
            } else {
                canonicalize(&workspace.join(&common))?
            },
            work_tree: canonicalize(Path::new(&work_tree))?,
            workspace_had_git: workspace.join(".git").exists(),
        })
    }

    /// Whether the repository is still the pinned one. First a **filesystem** check: a `.git`
    /// that appeared in the workspace root after the pin is a swap, and we must not run `git
    /// rev-parse` (which reads the swapped repository's config) on it. Only then re-resolve.
    fn still(&self, workspace: &Path) -> bool {
        if !self.workspace_had_git && workspace.join(".git").exists() {
            return false;
        }
        Self::resolve(workspace).as_ref() == Some(self)
    }
}

fn canonicalize(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// `git rev-parse <arg>` in `workspace`, trimmed, or `None` on failure.
fn rev_parse(workspace: &Path, arg: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args(["rev-parse", arg])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
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
    /// The filter drivers `git add` may run (`[git] allowed_filters`, ADR 0030).
    allowed_filters: Vec<String>,
    /// The repository pinned at session open (ADR 0030 R10).
    pin: RepoPin,
}

impl GitCommitTools {
    /// Builds the tool for a workspace **that is a git repository** (the caller resolves the
    /// pin with [`RepoPin::resolve`] first and does not mount the tool when it is `None`).
    pub fn new(workspace: impl Into<PathBuf>, journal: Arc<SessionJournal>, pin: RepoPin) -> Self {
        Self {
            workspace: workspace.into(),
            journal,
            allowed_branch: None,
            protected_branches: Vec::new(),
            allowed_filters: default_allowed_filters(),
            pin,
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

    /// The filter drivers `git add` may run (`[git] allowed_filters`, ADR 0030).
    pub fn with_allowed_filters(mut self, filters: Vec<String>) -> Self {
        self.allowed_filters = filters;
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
    let pin = tools.pin.clone();
    let allowed = tools.allowed_branch.clone();
    let protected = tools.protected_branches.clone();
    let allowed_filters = tools.allowed_filters.clone();
    let message = args.message;
    let paths = args.paths.unwrap_or_default();
    let committed = tokio::task::spawn_blocking(move || {
        commit(
            &workspace,
            &pin,
            &message,
            &paths,
            allowed.as_deref(),
            &protected,
            &allowed_filters,
        )
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

/// The blocking part: branch checks, the path boundary, `git add` and `git commit`. Public so
/// the integration tests exercise the exact code the tool runs.
pub fn commit(
    workspace: &Path,
    pin: &RepoPin,
    message: &str,
    paths: &[String],
    allowed_branch: Option<&str>,
    protected: &[String],
    allowed_filters: &[String],
) -> Result<Committed, ToolFailure> {
    // The repository must still be the one pinned at session open: a `.git/` the model created
    // in the workspace (when there was none, or when the real one is above it) would otherwise
    // redirect every git call below (ADR 0030 R10).
    if !pin.still(workspace) {
        return Err(fail(
            "repository_changed",
            "the repository changed since the session opened; refusing to commit",
        ));
    }
    let branch = git(workspace, pin, &["rev-parse", "--abbrev-ref", "HEAD"])?;
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
        let resolved_path = root
            .resolve_in_workspace(path)
            .map_err(|error| fail("outside_workspace", error))?;
        // ADR 0036 D2: the task agent's progress note lives under `.radian/` (which
        // is gitignored); refusing the path here is what keeps it out of a commit, not the
        // prompt. The interactive session is covered too: the rule is structural.
        let under_radian = resolved_path
            .strip_prefix(workspace)
            .ok()
            .and_then(|relative| relative.components().next())
            .is_some_and(|component| component.as_os_str() == ".radian");
        if under_radian {
            return Err(fail(
                "radian_path",
                "refusing to commit a path under .radian/ (the progress note lives there and is never committed)",
            ));
        }
        resolved.push(resolved_path);
    }

    // ADR 0030: a `filter=<driver>` attribute runs `filter.<driver>.clean`/`.process`
    // from git config when the path is staged, and the model can write `.gitattributes`. Work
    // out exactly what will be staged, then refuse before any `git add` runs. The pathspecs are
    // the same ones `git add` gets, so a directory argument is expanded to its files.
    let scope = workspace.to_string_lossy().into_owned();
    let to_stage: Vec<String> = if paths.is_empty() {
        staged_paths(workspace, pin, &[scope.clone()])?
    } else {
        let resolved: Vec<String> = resolved
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        staged_paths(workspace, pin, &resolved)?
    };
    check_filters(workspace, pin, &to_stage, allowed_filters)?;

    if paths.is_empty() {
        // Scope `-A` to the workspace, not the whole repository: a subdirectory workspace must
        // not stage its siblings.
        run_git(workspace, pin, &["add", "-A", "--", &scope])?;
    } else {
        let mut args = vec!["add", "--"];
        let owned: Vec<String> = resolved
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        for path in &owned {
            args.push(path.as_str());
        }
        run_git(workspace, pin, &args)?;
    }

    // `--no-verify` skips pre-commit/commit-msg; `core.hooksPath=/dev/null` is belt and
    // braces (a hook could still be found by another path). Both are mandatory.
    let args = commit_args(message);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_git(workspace, pin, &borrowed)?;

    let commit = git(workspace, pin, &["rev-parse", "HEAD"])?;
    let changed = git(
        workspace,
        pin,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )?;
    let files = changed.lines().filter(|line| !line.is_empty()).count();
    Ok(Committed {
        commit,
        branch,
        files,
    })
}

/// Runs `git` in `workspace` and returns its trimmed stdout. Every call pins the repository
/// (`--git-dir`/`--work-tree`, no upward search) and disables `core.fsmonitor` (a repo config a
/// model could write runs commands on `git add`). A non-zero exit is an error with the command
/// (never a secret) and git's stderr.
fn git(workspace: &Path, pin: &RepoPin, args: &[&str]) -> Result<String, ToolFailure> {
    Ok(String::from_utf8_lossy(&git_raw(workspace, pin, args)?)
        .trim()
        .to_owned())
}

/// [`git`] without trimming, for the NUL-separated `-z` output (`check-attr`, `status`).
fn git_raw(workspace: &Path, pin: &RepoPin, args: &[&str]) -> Result<Vec<u8>, ToolFailure> {
    let full = common_args(pin, args);
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args(&full)
        .output()
        .map_err(|error| fail("git_unavailable", format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(fail(
            "git_failed",
            format!(
                "git {} failed: {}",
                full.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output.stdout)
}

/// `git config --get <key>` under the pinned repository, or `None` when the key is unset (exit
/// 1). `git config` reads the repo, global and system levels, so a driver configured anywhere is
/// seen (ADR 0030).
fn git_config_opt(
    workspace: &Path,
    pin: &RepoPin,
    key: &str,
) -> Result<Option<String>, ToolFailure> {
    let full = common_args(pin, &["config", "--get", key]);
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args(&full)
        .output()
        .map_err(|error| fail("git_unavailable", format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

/// The repo-relative files `git add` would stage (with content, so a clean filter runs) under
/// `pathspecs` (ADR 0030). `--cached --others --modified --exclude-standard` lists
/// every tracked file plus the untracked ones (`.gitignore` respected). `--cached` is required:
/// `git add -A` re-cleans an **unchanged** tracked file when a filter attribute is added, so it
/// must be checked too. `ls-files` expands a directory pathspec (unlike `status`, which reports
/// the directory `newdir/`) and does not depend on `status.showUntrackedFiles`. Deletions are
/// removed: they are staged without content, so no clean filter runs for them.
fn staged_paths(
    workspace: &Path,
    pin: &RepoPin,
    pathspecs: &[String],
) -> Result<Vec<String>, ToolFailure> {
    let mut paths = ls_files(
        workspace,
        pin,
        &["--cached", "--others", "--modified", "--exclude-standard"],
        pathspecs,
    )?;
    // `--deleted` must stand alone: combined with `--cached --others` it lists everything.
    let deleted = ls_files(workspace, pin, &["--deleted"], pathspecs)?;
    paths.retain(|path| !deleted.contains(path));
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// `git ls-files -z <flags> [-- <pathspecs>]`, as repo-relative paths.
fn ls_files(
    workspace: &Path,
    pin: &RepoPin,
    flags: &[&str],
    pathspecs: &[String],
) -> Result<Vec<String>, ToolFailure> {
    let mut args: Vec<String> = vec!["ls-files".into(), "-z".into()];
    args.extend(flags.iter().map(|flag| (*flag).to_owned()));
    if !pathspecs.is_empty() {
        args.push("--".into());
        args.extend(pathspecs.iter().cloned());
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw = git_raw(workspace, pin, &borrowed)?;
    Ok(split_nul(&raw))
}

/// The NUL-separated fields of a `-z` output, dropping the empty trailing field.
fn split_nul(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter(|field| !field.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Refuses any staged path whose `filter` attribute names a driver the host must not run
/// (ADR 0030). `git check-attr -z` gives `(path, filter, value)` triples; `set`,
/// `unset` and `unspecified` are not driver names. A driver must be in `allowed`; `lfs` is only
/// allowed when its `clean`/`process` commands are unset or a `git-lfs` command. Any other
/// allowed name is the user's opt-in and is not inspected further.
fn check_filters(
    workspace: &Path,
    pin: &RepoPin,
    paths: &[String],
    allowed: &[String],
) -> Result<(), ToolFailure> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec![
        "check-attr".into(),
        "-z".into(),
        "filter".into(),
        "--".into(),
    ];
    args.extend(paths.iter().cloned());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw = git_raw(workspace, pin, &borrowed)?;
    let text = String::from_utf8_lossy(&raw);
    let mut fields = text.split('\0');
    while let (Some(path), Some(_attr), Some(value)) = (fields.next(), fields.next(), fields.next())
    {
        if matches!(value, "set" | "unset" | "unspecified") {
            continue;
        }
        if !allowed.iter().any(|name| name == value) {
            return Err(fail(
                "filter_not_allowed",
                format!(
                    "refusing to stage `{path}`: its `filter` attribute runs the git filter \
                     driver `{value}`, which is not in `[git] allowed_filters`; add \
                     `{value}` there if you trust it, or remove the attribute"
                ),
            ));
        }
        if value == "lfs" {
            for key in ["clean", "process"] {
                let setting = format!("filter.lfs.{key}");
                if let Some(command) = git_config_opt(workspace, pin, &setting)?
                    && !command.starts_with("git-lfs ")
                {
                    return Err(fail(
                        "filter_not_allowed",
                        format!(
                            "refusing to stage `{path}`: `{setting}` is `{command}`, not a \
                             `git-lfs` command"
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The global arguments **every** git call carries (ADR 0030 R10): the pinned repository, and
/// the two config overrides that keep a model-writable repository config from running commands
/// in the host. `core.hooksPath` matters on `git add` too (`post-index-change`), not only on
/// commit, so it is here and not in `commit_args`.
fn common_args(pin: &RepoPin, args: &[&str]) -> Vec<String> {
    let mut full: Vec<String> = vec![
        format!("--git-dir={}", pin.git_dir.display()),
        format!("--work-tree={}", pin.work_tree.display()),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
    ];
    full.extend(args.iter().map(|arg| (*arg).to_owned()));
    full
}

/// The `git commit` subcommand arguments. `--no-verify` skips pre-commit/commit-msg; the
/// `core.hooksPath` override is in [`common_args`].
fn commit_args(message: &str) -> Vec<String> {
    vec![
        "commit".into(),
        "--no-verify".into(),
        "-m".into(),
        message.into(),
    ]
}

/// Runs `git` for its side effect.
fn run_git(workspace: &Path, pin: &RepoPin, args: &[&str]) -> Result<(), ToolFailure> {
    git(workspace, pin, args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0030: both config guards are on **every** git call (not only commit:
    /// `git add` runs `post-index-change`), so a project hook in the writable worktree can never
    /// run in the host.
    #[test]
    fn every_git_call_disables_the_dangerous_repo_config() {
        let pin = RepoPin {
            git_dir: PathBuf::from("/g"),
            common_dir: PathBuf::from("/g"),
            work_tree: PathBuf::from("/w"),
            workspace_had_git: true,
        };
        let args = common_args(&pin, &["status"]);
        assert!(
            args.iter().any(|arg| arg == "core.fsmonitor=false"),
            "every call must disable fsmonitor: {args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "core.hooksPath=/dev/null"),
            "every call must point hooks away: {args:?}"
        );
        // `--no-verify` stays on the commit itself.
        assert!(commit_args("m").iter().any(|arg| arg == "--no-verify"));
    }

    /// ADR 0036 D2: a path under `.radian/` is refused structurally, so the task
    /// agent's progress note can never enter a commit (the interactive session too).
    #[test]
    fn a_path_under_dot_radian_is_refused() {
        let dir = std::env::temp_dir().join(format!("radian-gitcommit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".radian/progress")).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        run(&["checkout", "-q", "-b", "feature"]);
        // An initial commit, so the branch resolves (an unborn HEAD is a different refusal).
        std::fs::write(dir.join("init.txt"), "init\n").unwrap();
        run(&["add", "init.txt"]);
        run(&["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join(".radian/progress/x.md"), "note\n").unwrap();
        // The real caller passes the canonical workspace; `/var` -> `/private/var` here.
        let dir = std::fs::canonicalize(&dir).unwrap();
        let pin = RepoPin::resolve(&dir).expect("a repository");
        let error = commit(
            &dir,
            &pin,
            "note",
            &[".radian/progress/x.md".to_owned()],
            None,
            &[],
            &default_allowed_filters(),
        )
        .expect_err("a .radian path must be refused");
        assert_eq!(error.code, "radian_path", "{error:?}");
        // A normal path still commits.
        std::fs::write(dir.join("ok.txt"), "ok\n").unwrap();
        let committed = commit(
            &dir,
            &pin,
            "ok",
            &["ok.txt".to_owned()],
            None,
            &[],
            &default_allowed_filters(),
        );
        assert!(committed.is_ok(), "{committed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_spec_takes_a_message_and_optional_paths() {
        let spec = spec();
        assert_eq!(spec.name, "git_commit");
        assert_eq!(spec.input_schema["required"], json!(["message"]));
        assert!(spec.description.contains("--no-verify"));
        assert!(spec.description.contains("outside the sandbox"));
    }
}

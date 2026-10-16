//! `git_commit`: the host-side commit tool (ADR 0030 R10).
//!
//! When the sandbox keeps `.git` read-only — a task agent (ADR 0030), or any session whose
//! backend does not open it (ADR 0038 decision 4; Linux is still pending) — a model cannot
//! `git commit` from the sandbox. This tool runs in the **host process**, outside the sandbox:
//! it is the write path for a commit there. A general session in the `workspace` tier on macOS
//! can commit with native git instead, but the tool stays mounted and remains usable.
//!
//! What it refuses, by construction:
//!
//! - It never runs a shell: every argument is argv.
//! - It never runs a workspace **hook**: `--no-verify` and `-c core.hooksPath=/dev/null` are on
//!   every call. A project's `core.hooksPath` (e.g. husky's `.husky/`) lives in the worktree the
//!   model can write, so running it in the host would execute agent-written scripts as the user
//!   (ADR 0030).
//! - It never runs a workspace **filter driver** it was not told to allow (ADR 0030,
//!   hardened by ADR 0039 decision 3): a `filter=<driver>` attribute in a writable attributes
//!   file makes git run `filter.<driver>.clean`/`.process` from git config (repo or global),
//!   which is host code execution. Staging never runs a filter at all: the paths are read once
//!   with `git check-attr filter` and staged with `git hash-object -w --no-filters` +
//!   `git update-index --add --cacheinfo` (deletions with `update-index --remove`). A driver
//!   must be in `[git] allowed_filters` (default `["lfs"]`), and only `lfs` is actually run —
//!   by Radian's own fixed `git-lfs clean` (never the command string from git config). Every
//!   other driver refuses and stages nothing. Every other git call this tool makes (the
//!   `status`/`modified` reads, `commit`, `worktree add`/`remove`, …) carries `-c` overrides
//!   that empty every repository-configured filter command, so a background job (ADR 0037)
//!   cannot make one run between a check and its use.
//! - `paths` must resolve inside the workspace (the file tools' own boundary: absolute paths,
//!   `..` and symlinks out are refused).
//! - It refuses a detached HEAD and a configured protected branch on every session. On top of
//!   that, `main`/`master` are refused **only for a task agent** (ADR 0038 decision 4): a task
//!   session sets [`GitCommitTools::with_refuse_main`] and is further limited to `radian/<id>`,
//!   while the interactive `general`/`lead` session may commit on `main`.
//! - It only adds and commits: no push, no branch, no amend, no rebase.

use std::collections::{HashMap, HashSet};
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
                      outside the sandbox: use it when the sandbox keeps `.git` read-only — a \
                      task agent, or any session whose backend does not open `.git` — where a \
                      sandboxed `git commit` fails by design; a general session in the \
                      `workspace` tier on macOS can commit with native git instead. It stages \
                      without ever running a repository filter (the blobs are written with `git hash-object --no-filters` plus `git update-index --cacheinfo`) and commits with `git commit --no-verify`; it never runs \
                      workspace hooks (a project's `core.hooksPath` lives in the worktree you \
                      can write, so the host would be running your scripts as the user), and it \
                      refuses a path whose \
                      `filter` attribute names any driver but `lfs`; `lfs` is run by Radian itself with its own fixed `git-lfs clean`, never the command from git config. \
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
    // A host call: the shared no-filter overrides (ADR 0039 decision 3).
    let output = radian_coding::host_git::host_git_command(workspace)
        .ok()?
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
    /// Whether `main`/`master` are refused (ADR 0038 decision 4). `true` for a task agent (the
    /// narrow default); `false` for the interactive `general`/`lead` sessions, which may commit
    /// on `main`. A configured protected branch and a detached HEAD are always refused.
    refuse_main: bool,
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
            refuse_main: true,
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

    /// Whether `main`/`master` are refused (ADR 0038 decision 4). The default is `true` (a task
    /// agent's narrow rule); the interactive `general`/`lead` session calls `with_refuse_main(false)`
    /// so it can commit on `main`. A configured protected branch and a detached HEAD are refused
    /// either way.
    pub fn with_refuse_main(mut self, refuse: bool) -> Self {
        self.refuse_main = refuse;
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
    let refuse_main = tools.refuse_main;
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
            refuse_main,
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

/// Whether `git status --porcelain` reports any change in `workspace` under the pinned
/// repository. Used by the end-of-task commit (ADR 0030: no changes, no commit) and
/// by `remove_task` (refuse to remove a worktree with uncommitted changes). `pin.still` is
/// checked first, like every other call; the call carries the no-filter overrides (ADR 0039
/// decision 3), because `git status` reads the clean filter of a stat-dirty file.
pub fn dirty(workspace: &Path, pin: &RepoPin) -> Result<bool, ToolFailure> {
    if !pin.still(workspace) {
        return Err(fail(
            "repository_changed",
            "the repository changed since the session opened; refusing to read its status",
        ));
    }
    let status = git(workspace, pin, &["status", "--porcelain"])?;
    Ok(!status.is_empty())
}

/// The blocking part: branch checks, the path boundary, staging and `git commit`. Public so the
/// integration tests exercise the exact code the tool runs.
///
/// `refuse_main` is the ADR 0038 decision 4 switch: `true` for a task agent (which also carries
/// an `allowed_branch`), `false` for the interactive `general`/`lead` session. A config-protected
/// branch and a detached HEAD are refused regardless.
#[allow(clippy::too_many_arguments)] // one branch rule per argument; a rules struct hides them
pub fn commit(
    workspace: &Path,
    pin: &RepoPin,
    message: &str,
    paths: &[String],
    allowed_branch: Option<&str>,
    protected: &[String],
    allowed_filters: &[String],
    refuse_main: bool,
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
    // `main`/`master` are refused only for a task agent (ADR 0038 decision 4); a configured
    // protected branch is refused for every session.
    if (refuse_main && matches!(branch.as_str(), "main" | "master"))
        || protected.iter().any(|name| name == &branch)
    {
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

    // ADR 0039 decision 3: no repository filter runs in the host. Work out the paths from the
    // pathspecs `git add` would have used (a subdirectory workspace does not stage its siblings),
    // read the `filter` attribute once, and stage with plumbing that runs no filter at all.
    let scope = workspace.to_string_lossy().into_owned();
    let pathspecs: Vec<String> = if paths.is_empty() {
        vec![scope]
    } else {
        resolved
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    };

    // - `check`: every tracked file plus the untracked ones. An *unchanged* tracked file still
    //   matters: a newly added filter attribute makes `git add` re-clean it (ADR 0030 revision
    //   4), so the single attribute read and the refusal must cover it. `ls-files` does not run
    //   a filter, so the protected overrides are not needed for the read itself.
    // - `modified`: modified tracked files plus untracked ones (the paths whose content changes).
    // - `deleted`: tracked files missing from the working tree.
    let check = ls_files(
        workspace,
        pin,
        &["--cached", "--others", "--exclude-standard"],
        &pathspecs,
    )?;
    let modified = ls_files(
        workspace,
        pin,
        &["--modified", "--others", "--exclude-standard"],
        &pathspecs,
    )?;
    let deleted = ls_files(workspace, pin, &["--deleted"], &pathspecs)?;

    // ONE attribute read (ADR 0039 decision 3), used directly for the staging decision below.
    // The read must see the true attributes (including `core.attributesFile`), so it carries no
    // override beyond the pinned repository.
    let drivers = filter_drivers(pin, &check)?;
    let driver_of: HashMap<&str, &str> = drivers
        .iter()
        .map(|(path, driver)| (path.as_str(), driver.as_str()))
        .collect();

    // Decide per path and refuse before anything is staged (all-or-nothing).
    let modified_set: HashSet<&str> = modified.iter().map(String::as_str).collect();
    let deleted_set: HashSet<&str> = deleted.iter().map(String::as_str).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut planned: Vec<Planned> = Vec::new();
    for path in check.iter().chain(modified.iter()) {
        if deleted_set.contains(path.as_str()) {
            continue;
        }
        let has_driver = driver_of.contains_key(path.as_str());
        if !modified_set.contains(path.as_str()) && !has_driver {
            continue; // unchanged and unfiltered: nothing to write
        }
        if !seen.insert(path.as_str()) {
            continue;
        }
        if let Some(plan) = plan_path(
            path,
            driver_of.get(path.as_str()).copied(),
            allowed_filters,
            pin,
        )? {
            planned.push(plan);
        }
    }
    planned.sort_by(|a, b| a.path.cmp(&b.path));

    stage(workspace, pin, &planned)?;
    remove_paths(workspace, pin, &deleted)?;

    // `--no-verify` skips pre-commit/commit-msg; `core.hooksPath=/dev/null` is belt and
    // braces (a hook could still be found by another path). Both are mandatory. The commit
    // refreshes the index, which reads the clean filter of a stat-dirty path, so it carries the
    // no-filter overrides too.
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

/// One path the commit writes into the index. The kind is decided from the single attribute read;
/// [`stage`] writes the content with plumbing that never runs a filter.
struct Planned {
    path: String,
    kind: StageKind,
}

enum StageKind {
    /// A regular file: its bytes are written from the working tree, through Radian's own fixed
    /// `git-lfs clean` when `lfs` is set.
    File { mode: String, lfs: bool },
    /// A symlink: its target path is the blob (mode 120000). Git never runs a filter on one.
    Symlink,
    /// A submodule (a directory tracked as a gitlink, mode 160000). No filter runs for it.
    Gitlink,
}

/// Decides how one path is staged, from the one attribute read. Refuses a driver Radian must not
/// run (anything but an allowed `lfs`) before any of the commit is staged. `None` means the path
/// went away between the listing and here (a background job); the next commit stages that
/// deletion — content is not the code-execution boundary.
fn plan_path(
    path: &str,
    driver: Option<&str>,
    allowed: &[String],
    pin: &RepoPin,
) -> Result<Option<Planned>, ToolFailure> {
    // Whatever the file type, a driver Radian may not run refuses the whole commit. Only the
    // allowed `lfs` driver goes on to run, and only for a regular file.
    let lfs = match driver {
        None => false,
        Some("lfs") if allowed.iter().any(|name| name == "lfs") => true,
        Some("lfs") => {
            return Err(fail(
                "filter_not_allowed",
                format!(
                    "refusing to stage `{path}`: its `filter` attribute runs the git filter \
                     driver `lfs`, which is not in `[git] allowed_filters`; add `lfs` there, or \
                     remove the attribute"
                ),
            ));
        }
        Some(driver) => {
            return Err(fail(
                "filter_not_allowed",
                format!(
                    "refusing to stage `{path}`: its `filter` attribute runs the git filter \
                     driver `{driver}`. Radian runs only `lfs` itself, with its own fixed \
                     `git-lfs clean`, and never a command from the repository git config; \
                     remove the attribute, or move the filtering out of the commit path"
                ),
            ));
        }
    };
    let abs = pin.work_tree.join(path);
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.file_type().is_symlink() {
        return Ok(Some(Planned {
            path: path.to_owned(),
            kind: StageKind::Symlink,
        }));
    }
    if meta.is_dir() {
        return Ok(Some(Planned {
            path: path.to_owned(),
            kind: StageKind::Gitlink,
        }));
    }
    Ok(Some(Planned {
        path: path.to_owned(),
        kind: StageKind::File {
            mode: file_mode(&meta),
            lfs,
        },
    }))
}

/// The index mode for a regular file: `100755` when any execute bit is set, else `100644`.
fn file_mode(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return "100755".into();
        }
    }
    "100644".into()
}

/// Writes every planned path into the index with plumbing that never runs a filter (ADR 0039
/// decision 3): `git hash-object -w --no-filters` for the bytes and `git update-index --add
/// --cacheinfo` for the entry. A path whose `filter` is the allowed `lfs` goes through Radian's
/// own fixed `git-lfs clean -- <path>` first — never the `filter.lfs.clean` string from config.
fn stage(workspace: &Path, pin: &RepoPin, planned: &[Planned]) -> Result<(), ToolFailure> {
    // The plain files go through one batched `hash-object --stdin-paths`.
    let plain: Vec<&str> = planned
        .iter()
        .filter(|entry| matches!(entry.kind, StageKind::File { lfs: false, .. }))
        .map(|entry| entry.path.as_str())
        .collect();
    let mut hashes: HashMap<String, String> = HashMap::new();
    if !plain.is_empty() {
        let shas = hash_object_paths(&pin.work_tree, pin, &plain)?;
        for (path, sha) in plain.iter().zip(shas) {
            hashes.insert((*path).to_owned(), sha);
        }
    }

    // (mode, sha, repo-relative path)
    let mut entries: Vec<(String, String, String)> = Vec::with_capacity(planned.len());
    for entry in planned {
        match &entry.kind {
            StageKind::Symlink => {
                let target =
                    std::fs::read_link(pin.work_tree.join(&entry.path)).map_err(|error| {
                        fail(
                            "git_commit_failed",
                            format!("cannot read the symlink {}: {error}", entry.path),
                        )
                    })?;
                let sha =
                    hash_object_bytes(&pin.work_tree, pin, target.as_os_str().as_encoded_bytes())?;
                entries.push(("120000".into(), sha, entry.path.clone()));
            }
            StageKind::Gitlink => {
                let sha = gitlink_head(&pin.work_tree.join(&entry.path))?;
                entries.push(("160000".into(), sha, entry.path.clone()));
            }
            StageKind::File { mode, lfs: false } => {
                let sha = hashes.remove(&entry.path).ok_or_else(|| {
                    fail(
                        "git_commit_failed",
                        format!("no blob was written for {}", entry.path),
                    )
                })?;
                entries.push((mode.clone(), sha, entry.path.clone()));
            }
            StageKind::File { mode, lfs: true } => {
                let content = std::fs::read(pin.work_tree.join(&entry.path)).map_err(|error| {
                    fail(
                        "git_commit_failed",
                        format!("cannot read {}: {error}", entry.path),
                    )
                })?;
                let pointer = lfs_clean(workspace, &entry.path, &content)?;
                let sha = hash_object_bytes(&pin.work_tree, pin, &pointer)?;
                entries.push((mode.clone(), sha, entry.path.clone()));
            }
        }
    }
    update_index_entries(&pin.work_tree, pin, &entries)
}

/// Radian's own fixed `git-lfs clean -- <path>` (ADR 0039 decision 3): the command line is
/// Radian's, never the `filter.lfs.clean` string from git config. `content` goes to stdin, the
/// pointer comes back on stdout. The program is `git-lfs`, looked up on `PATH` (the
/// `RADIAN_GIT_LFS` environment variable overrides it, a test seam). Refuses when it is missing.
fn lfs_clean(workspace: &Path, path: &str, content: &[u8]) -> Result<Vec<u8>, ToolFailure> {
    let program = std::env::var_os("RADIAN_GIT_LFS").unwrap_or_else(|| "git-lfs".into());
    let mut child = std::process::Command::new(&program)
        .current_dir(workspace)
        .args(["clean", "--", path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            fail(
                "filter_unavailable",
                format!(
                    "refusing to stage `{path}`: it needs the `lfs` filter, but `git-lfs` could \
                     not be run ({error}); install git-lfs, or remove the `filter=lfs` attribute"
                ),
            )
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = content.to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().map_err(|error| {
        fail(
            "filter_unavailable",
            format!("`git-lfs clean` failed for `{path}`: {error}"),
        )
    })?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(fail(
            "filter_unavailable",
            format!(
                "`git-lfs clean` failed for `{path}`: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output.stdout)
}

/// The submodule commit a gitlink records: `git rev-parse HEAD` in the submodule.
fn gitlink_head(abs: &Path) -> Result<String, ToolFailure> {
    let output = radian_coding::host_git::host_git_command(abs)
        .map_err(|error| {
            fail(
                "git_commit_failed",
                format!("could not read the submodule at {}: {error}", abs.display()),
            )
        })?
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| {
            fail(
                "git_commit_failed",
                format!("could not read the submodule at {}: {error}", abs.display()),
            )
        })?;
    if !output.status.success() {
        return Err(fail(
            "git_commit_failed",
            format!(
                "could not read the submodule at {}: {}",
                abs.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `git hash-object -w --no-filters --stdin-paths` for repo-relative paths, in the repo root so
/// the paths resolve without a cwd prefix. Returns one blob id per path, in input order.
fn hash_object_paths(
    cwd: &Path,
    pin: &RepoPin,
    paths: &[&str],
) -> Result<Vec<String>, ToolFailure> {
    let command = git_command(
        cwd,
        pin,
        &["hash-object", "-w", "--no-filters", "--stdin-paths"],
    )?;
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(b'\n');
    }
    let output = run_with_stdin(command, &input)?;
    if !output.status.success() {
        return Err(fail(
            "git_failed",
            format!(
                "git hash-object failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let shas: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    if shas.len() != paths.len() {
        return Err(fail(
            "git_failed",
            format!(
                "git hash-object returned {} ids for {} paths",
                shas.len(),
                paths.len()
            ),
        ));
    }
    Ok(shas)
}

/// `git hash-object -w --no-filters --stdin` over `content`. The `--no-filters` is the point:
/// the bytes are written exactly as given, never through a repository filter.
fn hash_object_bytes(cwd: &Path, pin: &RepoPin, content: &[u8]) -> Result<String, ToolFailure> {
    let command = git_command(cwd, pin, &["hash-object", "-w", "--no-filters", "--stdin"])?;
    let output = run_with_stdin(command, content)?;
    if !output.status.success() {
        return Err(fail(
            "git_failed",
            format!(
                "git hash-object failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `git update-index --add --cacheinfo <mode> <sha> <path>` for each entry, in batches so a
/// commit of very many files cannot hit `ARG_MAX`. The 3-argument form (not the comma form)
/// keeps a path that contains a comma unambiguous.
fn update_index_entries(
    cwd: &Path,
    pin: &RepoPin,
    entries: &[(String, String, String)],
) -> Result<(), ToolFailure> {
    for chunk in entries.chunks(400) {
        let mut args: Vec<String> = vec!["update-index".into(), "--add".into()];
        for (mode, sha, path) in chunk {
            args.push("--cacheinfo".into());
            args.push(mode.clone());
            args.push(sha.clone());
            args.push(path.clone());
        }
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(cwd, pin, &borrowed)?;
    }
    Ok(())
}

/// `git update-index --remove -- <paths>` for the deleted ones, in batches.
fn remove_paths(cwd: &Path, pin: &RepoPin, deleted: &[String]) -> Result<(), ToolFailure> {
    for chunk in deleted.chunks(400) {
        let mut args: Vec<String> = vec!["update-index".into(), "--remove".into(), "--".into()];
        args.extend(chunk.iter().cloned());
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(cwd, pin, &borrowed)?;
    }
    Ok(())
}

/// Runs a prepared host `git` command with `input` on stdin and returns the finished output.
/// Stdin is written from its own thread so a full stdout pipe can never deadlock the write.
fn run_with_stdin(
    mut command: std::process::Command,
    input: &[u8],
) -> Result<std::process::Output, ToolFailure> {
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| fail("git_unavailable", format!("could not run git: {error}")))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(&data);
    });
    let output = child
        .wait_with_output()
        .map_err(|error| fail("git_failed", format!("git failed: {error}")))?;
    let _ = writer.join();
    Ok(output)
}

/// A host `git` command pinned to the repository, with the shared no-filter overrides already
/// applied (ADR 0039 decision 3). Every host `git` call in this file is built from it.
fn git_command(
    cwd: &Path,
    pin: &RepoPin,
    args: &[&str],
) -> Result<std::process::Command, ToolFailure> {
    let mut command = radian_coding::host_git::host_git_command(cwd)
        .map_err(|error| fail("git_unavailable", error))?;
    command.arg(format!("--git-dir={}", pin.git_dir.display()));
    command.arg(format!("--work-tree={}", pin.work_tree.display()));
    command.args(args);
    Ok(command)
}

/// Runs `git` in `workspace` and returns its trimmed stdout. Every call pins the repository
/// (`--git-dir`/`--work-tree`, no upward search) and carries the shared no-filter overrides
/// (ADR 0039 decision 3). A non-zero exit is an error with the command (never a secret) and git's
/// stderr.
fn git(workspace: &Path, pin: &RepoPin, args: &[&str]) -> Result<String, ToolFailure> {
    Ok(String::from_utf8_lossy(&git_raw(workspace, pin, args)?)
        .trim()
        .to_owned())
}

/// [`git`] without trimming, for the NUL-separated `-z` output (`check-attr`, `ls-files`).
fn git_raw(workspace: &Path, pin: &RepoPin, args: &[&str]) -> Result<Vec<u8>, ToolFailure> {
    let output = git_command(workspace, pin, args)?
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
    Ok(output.stdout)
}

/// `git ls-files -z <flags> [-- <pathspecs>]`, as repo-relative paths. `ls-files` never runs a
/// filter; the overrides still travel with it so no call in this file is unguarded.
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

/// `git check-attr --stdin -z filter` over `paths`, as `(path, driver)` pairs for every path
/// whose `filter` value is a driver name. This is the **one** attribute read the staging decision
/// uses (ADR 0039 decision 3); it carries no no-filter override, because it must see the true
/// attributes including `core.attributesFile`. It runs in the repo root so the repo-relative
/// paths resolve without a cwd prefix. The paths go through **stdin**, not argv: since
/// the list is every tracked file, a large repository would otherwise hit `E2BIG` (ADR 0030
/// revision 4).
fn filter_drivers(pin: &RepoPin, paths: &[String]) -> Result<Vec<(String, String)>, ToolFailure> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = radian_coding::host_git::host_git_attributes_command(&pin.work_tree)
        .map_err(|error| fail("git_unavailable", error))?;
    command.arg(format!("--git-dir={}", pin.git_dir.display()));
    command.arg(format!("--work-tree={}", pin.work_tree.display()));
    command.args(["check-attr", "--stdin", "-z", "filter"]);
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let output = run_with_stdin(command, &input)?;
    if !output.status.success() {
        return Err(fail(
            "git_failed",
            format!(
                "git check-attr failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split('\0');
    let mut drivers = Vec::new();
    while let (Some(path), Some(_attr), Some(value)) = (fields.next(), fields.next(), fields.next())
    {
        if !matches!(value, "set" | "unset" | "unspecified") {
            drivers.push((path.to_owned(), value.to_owned()));
        }
    }
    Ok(drivers)
}

/// The `git commit` subcommand arguments. `--no-verify` skips pre-commit/commit-msg; the
/// `core.hooksPath` override is in the shared [`radian_coding::host_git`] overrides.
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

    /// ADR 0030: `git commit` skips the workspace's pre-commit/commit-msg hooks.
    /// The `core.fsmonitor`/`core.hooksPath`/filter overrides on **every** host call live in the
    /// shared helper (`radian_coding::host_git`), tested there.
    #[test]
    fn commit_args_skip_verify() {
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
            true,
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
            true,
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

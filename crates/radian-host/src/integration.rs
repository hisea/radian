//! The lead's **integration worktree** (ADR 0030, slice 10).
//!
//! ADR 0030 O7 (revision 1 R6, revision 2 item 5) says the lead merges task branches and reruns
//! the integration verify in a dedicated worktree on `radian/integration`, and that the lead's
//! delivery stops there. ADR 0039 decision 4 moved the task worktrees **out of the workspace** to
//! `~/.radian/worktrees/<repo-key>/<task-id>` and made the lead's sandbox read-only there, so the
//! merge can no longer be a shell command the model writes: it is a **host function** behind the
//! `integrate_task` lead tool.
//!
//! This module is the host side of that: where the worktree lives, how it is created lazily from
//! the lead's current HEAD, and how one task branch is merged into it with no hook and no filter
//! run. Nothing here reads the orchestration config or the task registry; [`crate::task`] and
//! [`crate::lead_tools`] wire those around it.
//!
//! Location (ADR 0030, ADR 0039 decision 4):
//!
//! ```text
//! <radian-home>/worktrees/<repo-key>/integration          on branch radian/integration
//! <radian-home>/worktrees/<repo-key>/integration-<lead>   on branch radian/integration-<lead>
//! ```
//!
//! It shares the task worktree root, so its levels get the same per-level `O_NOFOLLOW` check and
//! owner-only creation the task root uses ([`create_worktree`]), and no model can write in it.

use std::path::{Path, PathBuf};

use crate::git_commit::{Committed, RepoPin};
use crate::task::{TaskError, create_worktree, git_head, refs_has_branch, vet_worktree_root};

/// The integration branch for a lead (ADR 0030): `radian/integration` for
/// the main lead, `radian/integration-<lead>` for any other lead, so two leads never share one.
pub fn branch_for(lead_name: Option<&str>) -> String {
    match lead_name {
        None => "radian/integration".to_owned(),
        Some(name) => format!("radian/integration-{name}"),
    }
}

/// The integration worktree directory name below the task worktree root, matching
/// [`branch_for`].
pub fn dir_name_for(lead_name: Option<&str>) -> String {
    match lead_name {
        None => "integration".to_owned(),
        Some(name) => format!("integration-{name}"),
    }
}

/// One integration worktree's facts: where it is, the branch checked out in it, and the commit
/// it was created from. Cheap to clone; the registry caches it after the first `integrate_task`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    /// `<radian-home>/worktrees/<repo-key>/<integration[-lead]>`.
    pub worktree: PathBuf,
    /// `radian/integration` or `radian/integration-<lead>`.
    pub branch: String,
    /// The lead's HEAD when the worktree was created (ADR 0030). The base
    /// is never re-pointed.
    pub base: Option<String>,
}

/// Creates the integration worktree lazily, from the lead's current HEAD (ADR 0030
/// decision 2). `root` is the task worktree root (`<home>/worktrees/<repo-key>`,
/// [`crate::task::TaskRegistry::worktree_root`]).
///
/// It **refuses** when the branch or the directory already exists: ADR 0030
/// says the worktree is not reused and not reset, because a previous run may already have handed
/// `radian/integration` to the human. A registry that created it earlier in this process reuses
/// its own cached [`Integration`] instead of calling this again.
pub fn create(
    lead: &Path,
    home: &Path,
    root: &Path,
    lead_name: Option<&str>,
) -> Result<Integration, TaskError> {
    let branch = branch_for(lead_name);
    let worktree = root.join(dir_name_for(lead_name));
    // The root's levels are checked/created with the task root's own vetting (ADR 0039
    // decision 4); a symlink anywhere refuses before git runs.
    vet_worktree_root(home, root)?;
    if refs_has_branch(lead, &branch)? {
        return Err(TaskError::new(format!(
            "refusing to create the integration worktree: the branch `{branch}` already exists; \
             a previous lead already handed `{branch}` to the human (ADR 0030). \
             Finish or drop that branch before integrating again."
        )));
    }
    if worktree.exists() {
        return Err(TaskError::new(format!(
            "refusing to create the integration worktree {}: it already exists; a previous lead \
             left it behind (ADR 0030). Remove it before integrating again.",
            worktree.display()
        )));
    }
    // No start point: `git worktree add -b <branch> <path>` branches from the lead's HEAD, which
    // is the base ADR 0030 names.
    create_worktree(lead, home, &worktree, &branch)?;
    let base = git_head(&worktree);
    Ok(Integration {
        worktree,
        branch,
        base,
    })
}

/// The result of one `git merge` attempt (ADR 0030 and 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge was committed on the integration branch.
    Merged(Committed),
    /// The task branch was already an ancestor of the integration branch; nothing to do.
    AlreadyUpToDate,
    /// The merge conflicted. It has already been **aborted** (no `MERGE_HEAD`, clean tree), and
    /// the conflicted paths are reported for the lead to act on (ADR 0030).
    Conflict {
        paths: Vec<String>,
        status: String,
        stderr: String,
    },
}

/// How a `git merge` attempt failed before it could report a conflict.
impl MergeOutcome {
    /// Whether the branch merged cleanly and was committed.
    pub fn merged(&self) -> Option<&Committed> {
        match self {
            Self::Merged(committed) => Some(committed),
            _ => None,
        }
    }
}

/// Merges `task_branch` (e.g. `radian/<id>`) into the integration branch
/// (ADR 0030): `git merge --no-ff --no-commit` through the shared host
/// `git` helper (no hook, no filter), then the `git_commit` path with `integration_branch` as the
/// only allowed branch. A conflict is aborted before returning (decision 6).
///
/// `pin` is the integration worktree's [`RepoPin`]; `protected` and `filters` come from the
/// lead's session (the same rules the task's own commits use).
pub fn merge_task_branch(
    worktree: &Path,
    integration_branch: &str,
    task_id: &str,
    task_branch: &str,
    pin: &RepoPin,
    protected: &[String],
    filters: &[String],
) -> Result<MergeOutcome, TaskError> {
    // `--no-ff` forces a merge commit even when the branch could fast-forward, so the integration
    // branch always shows one commit per task (ADR 0030). `--no-commit`
    // leaves the result staged so the commit goes through the tested `git_commit` path below.
    let merge = radian_coding::host_git::host_git_command(worktree)
        .map_err(TaskError::new)?
        .args(["merge", "--no-ff", "--no-commit", task_branch])
        .output()
        .map_err(|error| TaskError::new(format!("could not run git merge: {error}")))?;
    if !merge.status.success() {
        // Collect the conflict facts **before** aborting (the abort clears them), then abort so
        // the shared worktree is left clean and a later `integrate_task` is not blocked
        // (ADR 0030: no `MERGE_HEAD`, clean tree).
        let paths = git_lines(worktree, &["diff", "--name-only", "--diff-filter=U"]);
        let status = git_text(worktree, &["status", "--porcelain"]).unwrap_or_default();
        let stderr = tail(&String::from_utf8_lossy(&merge.stderr), 2000);
        let _ = radian_coding::host_git::host_git_command(worktree)
            .map_err(TaskError::new)?
            .args(["merge", "--abort"])
            .output();
        return Ok(MergeOutcome::Conflict {
            paths,
            status,
            stderr,
        });
    }
    // A branch that is already an ancestor merges as "Already up to date": there is no
    // `MERGE_HEAD` to commit.
    if git_text(worktree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_none() {
        return Ok(MergeOutcome::AlreadyUpToDate);
    }
    let message = format!("radian: integrate task {task_id} ({task_branch})");
    match crate::git_commit::commit(
        worktree,
        pin,
        &message,
        &[],
        Some(integration_branch),
        protected,
        filters,
        // `refuse_main = true`: the merge must never commit on a protected branch; the
        // `allowed_branch` check is what pins it to `radian/integration`.
        true,
        true,
    ) {
        Ok(committed) => Ok(MergeOutcome::Merged(committed)),
        Err(failure) => {
            // The commit path refused (a filter, a swapped repository, …) **after** the merge was
            // staged. Abort so the worktree is left clean, then report.
            let _ = radian_coding::host_git::host_git_command(worktree)
                .map_err(TaskError::new)?
                .args(["merge", "--abort"])
                .output();
            Err(TaskError::new(failure.message))
        }
    }
}

/// The `git` output lines of a host call in `dir`, empty on failure.
fn git_lines(dir: &Path, args: &[&str]) -> Vec<String> {
    git_text(dir, args)
        .map(|text| {
            text.lines()
                .map(str::to_owned)
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A host `git` call in `dir`, trimmed, or `None` on any failure.
fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    let output = radian_coding::host_git::host_git_command(dir)
        .ok()?
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Keeps the last `limit` characters of `text`, with a marker when it cut. A merge's useful
/// message (the first conflict) is at the start, so this keeps a head instead.
fn tail(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{cut}\n… [truncated]")
}

/// The integration worktree path for `root` and a lead name, without creating anything. Used to
/// answer `spawn_task`'s `base` default and for tests.
pub fn worktree_path(root: &Path, lead_name: Option<&str>) -> PathBuf {
    root.join(dir_name_for(lead_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_branch_and_directory_follow_the_lead_name() {
        assert_eq!(branch_for(None), "radian/integration");
        assert_eq!(dir_name_for(None), "integration");
        assert_eq!(branch_for(Some("dev")), "radian/integration-dev");
        assert_eq!(dir_name_for(Some("dev")), "integration-dev");
    }

    #[test]
    fn the_worktree_path_is_below_the_root_for_the_lead() {
        let root = Path::new("/h/.radian/worktrees/repo-1a2b3c");
        assert_eq!(
            worktree_path(root, None),
            PathBuf::from("/h/.radian/worktrees/repo-1a2b3c/integration")
        );
        assert_eq!(
            worktree_path(root, Some("dev")),
            PathBuf::from("/h/.radian/worktrees/repo-1a2b3c/integration-dev")
        );
    }
}

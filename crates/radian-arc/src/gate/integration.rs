//! The `integration` stage (ADR 0048 decisions 6 and 10).
//!
//! Integration builds the commit that would be merged, with the `[integration] style`:
//! `merge` (a merge commit, or `head_sha` when the target has not moved), `squash` (one
//! commit on top of the target with the task's changes applied) or `rebase` (the task's
//! commits replayed on the target). The style changes the candidate's sha, never the task's
//! `verified_sha`; the review half compares what landed with the candidate after the push.
//!
//! The key commands run on the candidate. The verdict records the target branch it used
//! (`base_sha`), the candidate and the style. [`merge_ready`] re-reads the target branch: the
//! merge may only fast forward it when it is still that `base_sha`.
//!
//! [`candidate_for`] builds the same kind of candidate for a verification stage: `sha` itself
//! when the remote main is an ancestor of it, else a merge of `sha` onto the remote main. A
//! verification verdict keeps `sha` as the task's `verified_sha` and records the main it used
//! and the commit the stage ran on.
//!
//! The stage holds its own `gates/integration.lock` for its whole run, so two integration
//! runs never work in the shared `gates/integration/` checkout at the same time.

use std::path::Path;
use std::time::Duration;

use super::checkout::GateCheckout;
use super::commands::{SeriesOutcome, run_series};
use super::config::Stage;
use super::error::GateError;
use super::exclusive::{DirLock, PREPARE_LOCK};
use super::git;
use super::verdict::{
    VERDICT_SCHEMA_VERSION, Verdict, VerdictResult, attempt_dir, now_utc, verdict_path,
};
use crate::store::config::IntegrationStyle;

/// The lock file of the integration stage under `gates/`.
pub(crate) const INTEGRATION_LOCK: &str = "integration.lock";

/// The `integration` stage for one attempt.
///
/// `remote` is the repository's remote name and `main_ref` the ref that names the remote
/// target branch after a fetch, typically `refs/remotes/<remote>/main`.
#[derive(Debug, Clone)]
pub struct IntegrationRun<'a> {
    /// The repository root.
    pub repository: &'a Path,
    /// The repository's remote name, for the fetches and the target ref.
    pub remote: &'a str,
    /// The task id (one path component); it names the merged branch `work/<task>`.
    pub task: &'a str,
    /// The task's title, for a `squash` candidate's commit message.
    pub title: &'a str,
    /// The attempt id (one path component).
    pub attempt: &'a str,
    /// The commit the gate verified and that worker pushed.
    pub head_sha: &'a str,
    /// The target-branch commit the task's branch started from.
    pub base_sha: &'a str,
    /// The ref that names the remote target branch after a fetch.
    pub main_ref: &'a str,
    /// The `[integration] key_commands` the stage runs on the candidate.
    pub key_commands: &'a [String],
    /// How the candidate is built: `merge`, `squash` or `rebase`.
    pub style: IntegrationStyle,
    /// The gate profile the stage runs under, for the verdict (ADR 0048 decisions 10 and 15).
    pub profile: &'a str,
    /// Per-command timeout; zero waits forever.
    pub timeout: Duration,
}

impl IntegrationRun<'_> {
    /// Runs the stage and returns the verdict it wrote.
    ///
    /// # Errors
    ///
    /// The fetch, the checkout, a command's log file, or the verdict write failed. A
    /// conflict or a failing key command is a `fail` verdict, not an error.
    pub fn run(&self) -> Result<Verdict, GateError> {
        let _lock = DirLock::acquire(self.repository, INTEGRATION_LOCK)?;
        let started_at = now_utc();
        // The branch the merge puts the candidate on: `[integration] target_branch`, else
        // `[repository] main_branch`. A branch the remote does not have is a failed
        // integration with a reason, not a git error: the task goes to `review.rework_pending`
        // and nothing is pushed.
        let target_branch = git::main_branch_of(self.remote, self.main_ref)?;
        let head = git::resolve_commit(self.repository, self.head_sha)?;
        let mut commands = Vec::new();
        let mut conflict_paths = Vec::new();
        let mut reason = None;
        let mut result = VerdictResult::Pass;
        let mut target_sha = None;
        let candidate;
        if git::ls_remote_branch(self.repository, self.remote, target_branch)?.is_none() {
            candidate = None;
            result = VerdictResult::Fail;
            reason = Some(format!(
                "target branch `{target_branch}` does not exist on `{}`",
                self.remote
            ));
        } else {
            // The fetch and the checkout prepare update the shared repository, so they take
            // turns with every other stage preparation (ADR 0048 decision 10); the stage work
            // after the prepare runs unlocked, next to another task's stage.
            let prepare = DirLock::acquire(self.repository, PREPARE_LOCK)?;
            // The work branch comes with the ordinary fetch; the target branch needs its own
            // refspec so a clone whose fetch configuration leaves it out still reads the
            // current target.
            git::fetch_remote(self.repository, self.remote)?;
            git::fetch_main(self.repository, self.remote, target_branch)?;
            let task_base = git::resolve_commit(self.repository, self.base_sha)?;
            let remote_main = git::resolve_commit(self.repository, self.main_ref)?;
            target_sha = Some(remote_main.clone());
            let checkout = GateCheckout::new(self.repository, Stage::Integration, self.remote);
            let log_dir = attempt_dir(self.repository, self.task, self.attempt)?.join("logs");

            // Build the candidate with the configured style. The prepare lock covers every
            // checkout preparation; the key commands run after it is released (ADR 0048
            // decision 10).
            let built = match self.style {
                IntegrationStyle::Merge if remote_main == task_base => {
                    // The target has not moved: the merge result is the verified head itself.
                    checkout.prepare(&head)?;
                    BuildOutcome::ready(head.clone())
                }
                IntegrationStyle::Merge => {
                    // The target moved: merge head onto it, never the other way around.
                    checkout.prepare(&remote_main)?;
                    merge_onto(&checkout, self.task, &head)?
                }
                IntegrationStyle::Squash => {
                    // One commit on top of the target with the task's changes applied.
                    checkout.prepare(&remote_main)?;
                    squash_onto(&checkout, self.title, self.task, &head)?
                }
                IntegrationStyle::Rebase => {
                    if git::is_ancestor(self.repository, &remote_main, &head)? {
                        // The task's commits are already on the target: replaying them would
                        // only rewrite their shas.
                        checkout.prepare(&head)?;
                        BuildOutcome::ready(head.clone())
                    } else {
                        checkout.prepare(&head)?;
                        rebase_onto(&checkout, &remote_main, &head)?
                    }
                }
            };
            drop(prepare);
            candidate = built.candidate_sha;
            conflict_paths = built.conflict_paths;
            if let Some(failure) = built.failure {
                result = VerdictResult::Fail;
                reason = Some(failure);
            } else {
                let (records, series) = run_series(
                    &checkout.path(),
                    self.key_commands,
                    self.timeout,
                    &log_dir,
                    Stage::Integration,
                )?;
                commands = records;
                if series != SeriesOutcome::Passed {
                    result = VerdictResult::Fail;
                    reason = series.reason();
                }
            }
        }

        let verdict = Verdict {
            schema_version: VERDICT_SCHEMA_VERSION,
            stage: Stage::Integration,
            sha: head,
            result,
            reason,
            commands,
            started_at,
            ended_at: now_utc(),
            main_sha: None,
            base_sha: target_sha,
            candidate_sha: candidate,
            conflict_paths,
            approver: None,
            review: None,
            review_session_ids: Vec::new(),
            review_cost_micros: None,
            review_model_error: None,
            review_pre_existing: None,
            review_settings: None,
            profile: Some(self.profile.to_owned()),
            integration_style: Some(self.style),
        };
        verdict.write_new(&verdict_path(
            self.repository,
            self.task,
            self.attempt,
            Stage::Integration,
        )?)?;
        Ok(verdict)
    }
}

/// Whether the merge may proceed: the remote target branch (`remote`'s `main_ref`) must still
/// equal the integration verdict's recorded `base_sha` (ADR 0048 decision 6, "Main is still
/// `base_sha`").
///
/// It fetches `remote` and compares the ref again, so a target branch that moved after
/// integration makes this false. A verdict that is not a passing integration verdict is never
/// ready.
///
/// # Errors
///
/// The fetch or the resolution of `main_ref` failed.
pub fn merge_ready(
    repository: &Path,
    remote: &str,
    main_ref: &str,
    verdict: &Verdict,
) -> Result<bool, GateError> {
    if verdict.stage != Stage::Integration || !verdict.is_pass() {
        return Ok(false);
    }
    let (Some(base), Some(_candidate)) = (&verdict.base_sha, &verdict.candidate_sha) else {
        return Ok(false);
    };
    let main_branch = git::main_branch_of(remote, main_ref)?;
    git::fetch_main(repository, remote, main_branch)?;
    let current = git::resolve_commit(repository, main_ref)?;
    Ok(current == *base)
}

/// The verification candidate of one stage for `sha` (ADR 0048 decision 10).
///
/// `sha` stays the task's `verified_sha`; `candidate_sha` is the commit the stage runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// The resolved `verified_sha`.
    pub sha: String,
    /// The remote target branch the candidate was built on.
    pub main_sha: String,
    /// The commit the stage runs on, or `None` when the merge failed.
    pub candidate_sha: Option<String>,
    /// The conflicted paths of a failed merge, sorted.
    pub conflict_paths: Vec<String>,
    /// Why the merge failed: `conflict`, or `merge_failed: <git's message>`.
    pub failure: Option<String>,
}

/// Builds the verification candidate for `sha` in `checkout` (ADR 0048 decision 10).
///
/// It fetches `remote` and its target branch first. When the remote target branch is an
/// ancestor of `sha`, the candidate is `sha` itself. Otherwise `checkout` goes to the remote
/// target, where a `--no-ff` merge of `sha` builds the candidate with arc's own identity; that
/// commit is never pushed. The checkout is left on the candidate.
///
/// A conflict aborts the merge, leaves no candidate, and reports reason `conflict` with the
/// sorted paths. A merge that fails for another reason reports `merge_failed: <message>`.
///
/// # Errors
///
/// The fetch, the resolution of a commit, or a git command failed.
pub(crate) fn candidate_for(
    repository: &Path,
    remote: &str,
    main_ref: &str,
    sha: &str,
    task: &str,
    checkout: &GateCheckout,
) -> Result<Candidate, GateError> {
    // The `commands` and `review` pair prepares two checkouts at the same time: the fetches
    // and `git worktree add`/`prune` update the shared repository, so the preparations take
    // turns (the stage work after this call still runs at the same time).
    let _prepare = DirLock::acquire(repository, PREPARE_LOCK)?;
    let main_branch = git::main_branch_of(remote, main_ref)?;
    git::fetch_remote(repository, remote)?;
    git::fetch_main(repository, remote, main_branch)?;
    let head = git::resolve_commit(repository, sha)?;
    let remote_main = git::resolve_commit(repository, main_ref)?;
    if git::is_ancestor(repository, &remote_main, &head)? {
        checkout.prepare(&head)?;
        return Ok(Candidate {
            sha: head.clone(),
            main_sha: remote_main,
            candidate_sha: Some(head),
            conflict_paths: Vec::new(),
            failure: None,
        });
    }
    checkout.prepare(&remote_main)?;
    let outcome = merge_onto(checkout, task, &head)?;
    Ok(Candidate {
        sha: head,
        main_sha: remote_main,
        candidate_sha: outcome.candidate_sha,
        conflict_paths: outcome.conflict_paths,
        failure: outcome.failure,
    })
}

/// What one candidate build in a checkout produced.
struct BuildOutcome {
    candidate_sha: Option<String>,
    conflict_paths: Vec<String>,
    failure: Option<String>,
}

impl BuildOutcome {
    /// A candidate that is already a commit: `head_sha` itself when the target has not moved.
    fn ready(candidate_sha: String) -> Self {
        Self {
            candidate_sha: Some(candidate_sha),
            conflict_paths: Vec::new(),
            failure: None,
        }
    }
}

/// Merges `sha` onto the checkout's current HEAD, with arc's fixed identity.
///
/// The checkout must already be prepared at the target-branch commit. A conflict aborts the
/// merge and leaves the checkout on that commit, so the next run can reuse it.
fn merge_onto(checkout: &GateCheckout, task: &str, sha: &str) -> Result<BuildOutcome, GateError> {
    let message = merge_message(task, sha);
    let mut args = crate::git::IDENTITY.to_vec();
    args.extend(["merge", "--no-ff", "--no-edit", "-m", message.as_str(), sha]);
    let output = git::run(&checkout.path(), &args)?;
    if output.success() {
        let candidate_sha = git::ok(&checkout.path(), &["rev-parse", "HEAD"])?;
        return Ok(BuildOutcome::ready(candidate_sha));
    }
    let conflict_paths = conflicted_paths(&checkout.path())?;
    let _ = git::run(&checkout.path(), &["merge", "--abort"]);
    Ok(BuildOutcome {
        failure: Some(if conflict_paths.is_empty() {
            format!("merge_failed: {}", output.stderr.trim())
        } else {
            "conflict".to_string()
        }),
        conflict_paths,
        candidate_sha: None,
    })
}

/// Applies `sha`'s changes on the checkout's current HEAD as one commit with arc's identity.
///
/// The checkout must already be prepared at the target-branch commit. `git merge --squash`
/// stages the task's changes without a merge parent; the commit then has the target as its
/// only parent and the task's tree on top. An empty change still makes one (empty) commit, so
/// a `squash` integration always lands exactly one commit. A conflict resets the checkout to
/// the target commit, so the next run can reuse it.
fn squash_onto(
    checkout: &GateCheckout,
    title: &str,
    task: &str,
    sha: &str,
) -> Result<BuildOutcome, GateError> {
    let path = checkout.path();
    let merge = git::run(&path, &["-c", "merge.ff=true", "merge", "--squash", sha])?;
    if !merge.success() {
        let conflict_paths = conflicted_paths(&path)?;
        restore(&path, &["reset", "--hard", "HEAD"]);
        let _ = git::run(&path, &["clean", "-ffdx", "-e", "target"]);
        return Ok(BuildOutcome {
            failure: Some(if conflict_paths.is_empty() {
                format!("merge_failed: {}", merge.stderr.trim())
            } else {
                "conflict".to_string()
            }),
            conflict_paths,
            candidate_sha: None,
        });
    }
    let message = squash_message(title, task);
    let mut args = crate::git::IDENTITY.to_vec();
    args.extend(["commit", "--allow-empty", "-m", message.as_str()]);
    let commit = git::run(&path, &args)?;
    if !commit.success() {
        restore(&path, &["reset", "--hard", "HEAD"]);
        return Ok(BuildOutcome {
            failure: Some(format!("commit_failed: {}", commit.stderr.trim())),
            conflict_paths: Vec::new(),
            candidate_sha: None,
        });
    }
    let candidate_sha = git::ok(&path, &["rev-parse", "HEAD"])?;
    Ok(BuildOutcome::ready(candidate_sha))
}

/// Replays the commits between `upstream` and the checkout's HEAD onto `upstream`.
///
/// The checkout must already be prepared at `head`. A conflict aborts the rebase and leaves
/// the checkout on `head`, so the next run can reuse it. A branch that contains a merge commit
/// is linearized: `git rebase` drops the merge commit and replays the commits it brought in.
fn rebase_onto(
    checkout: &GateCheckout,
    upstream: &str,
    head: &str,
) -> Result<BuildOutcome, GateError> {
    let path = checkout.path();
    let output = git::run(&path, &["rebase", upstream])?;
    if output.success() {
        let candidate_sha = git::ok(&path, &["rev-parse", "HEAD"])?;
        return Ok(BuildOutcome::ready(candidate_sha));
    }
    let conflict_paths = conflicted_paths(&path)?;
    let aborted = git::run(&path, &["rebase", "--abort"]);
    if !matches!(&aborted, Ok(output) if output.success()) {
        // An abort can fail when the rebase had not started its sequencer; `--quit` clears
        // that state and the reset restores the checkout to the commit it started from.
        let _ = git::run(&path, &["rebase", "--quit"]);
        restore(&path, &["reset", "--hard", head]);
        let _ = git::run(&path, &["clean", "-ffdx", "-e", "target"]);
    }
    Ok(BuildOutcome {
        failure: Some(if conflict_paths.is_empty() {
            format!("rebase_failed: {}", output.stderr.trim())
        } else {
            "conflict".to_string()
        }),
        conflict_paths,
        candidate_sha: None,
    })
}

/// Runs a best-effort restore command, ignoring its result: it is cleanup after a failed
/// candidate build, and the next `prepare` resets the checkout again.
fn restore(checkout: &Path, args: &[&str]) {
    let _ = git::run(checkout, args);
}

fn merge_message(task: &str, head: &str) -> String {
    let short = &head[..head.len().min(7)];
    format!("Merge work/{task} ({short})")
}

/// A `squash` candidate's commit message: `<task title> (<task id>)`.
fn squash_message(title: &str, task: &str) -> String {
    format!("{title} ({task})")
}

fn conflicted_paths(checkout: &Path) -> Result<Vec<String>, GateError> {
    let output = git::ok(checkout, &["diff", "--name-only", "--diff-filter=U"])?;
    let mut paths: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

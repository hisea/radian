//! Verification, merge and reject: the review half of the state machine (ADR 0048
//! decisions 6, 10 and 11).
//!
//! - [`verify`] runs the gate's built stages against the verification candidate for the
//!   task's pushed `head_sha` and moves the task to `review.awaiting_human` or
//!   `review.rework_pending`. The candidate is `head_sha` when the remote main is its
//!   ancestor, else a merge of `head_sha` onto the remote main (ADR 0048 decision 10);
//!   `verified_sha` stays the task's own commit.
//! - [`merge`] takes the owner's approval for `verified_sha`, builds the integration
//!   candidate, pushes it to the remote main without force, and moves the task to
//!   `done.merged`. A main that moved makes it integrate again.
//! - [`reject`] records the owner's note and sends the task back to rework.
//!
//! The remote is the truth: before a verify or a merge acts, the remote's `work/<task>` and
//! `refs/heads/<main>` are read again. A branch that moved invalidates every verdict and the
//! approval of the old sha and returns the task to `review.verifying`.
//!
//! The verification set is the counted verdicts of the task's attempts for `verified_sha`,
//! except the `human` approval and the `integration` verdict: those two are written after
//! verification (at merge), so they cannot be part of the digest verification records.
//! [`Reason`] is the last refusal or rework reason, kept next to the verdicts so `show` can
//! print it.
//!
//! Every git call goes through the crate's one helper (`crate::git`).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::gate::{
    CommandRun, IntegrationRun, Stage, StageSpec, Verdict, attempt_dir, effective_stages,
    merge_ready, required_passed, verdict_path, verdict_set_digest,
};
use crate::store::{Error, Store, model};

/// The file in an attempt's run directory that holds the last verify or merge reason.
pub const REVIEW_FILE: &str = "review.json";
/// The owner's reject note, in the attempt's run directory.
pub const REJECT_FILE: &str = "reject.md";
/// How many integration runs one `merge` command may make after main moved.
pub const MAX_INTEGRATION_ROUNDS: u32 = 3;
/// The stages this build can run in verification. A listed stage outside it refuses with
/// "stage <name> is not built yet".
pub const BUILT_STAGES: [Stage; 1] = [Stage::Commands];

/// Why a verify or merge did not advance the task: a stage that failed, a branch that
/// moved, or a stage this build does not run yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    /// The stage the reason belongs to, when it is one stage's failure.
    pub stage: Option<Stage>,
    /// The reason itself, as the verdict wrote it.
    pub reason: String,
    /// The verdict file that records it, when there is one.
    pub verdict: Option<PathBuf>,
    /// What wrote it: `verify` or `merge`.
    pub action: &'static str,
}

impl Reason {
    fn new(action: &'static str, stage: Option<Stage>, reason: String) -> Self {
        Self {
            stage,
            reason,
            verdict: None,
            action,
        }
    }

    fn with_verdict(action: &'static str, stage: Stage, reason: String, verdict: PathBuf) -> Self {
        Self {
            stage: Some(stage),
            reason,
            verdict: Some(verdict),
            action,
        }
    }

    /// The one text `show` prints and the refusal carries.
    pub fn text(&self) -> String {
        let stage = self
            .stage
            .map(|stage| format!("{stage}: "))
            .unwrap_or_default();
        let verdict = self
            .verdict
            .as_ref()
            .map(|path| format!(" (verdict {})", path.display()))
            .unwrap_or_default();
        format!("{stage}{}{verdict}", self.reason)
    }

    /// Records the reason next to the attempt's verdicts. A failure to record it never hides
    /// the reason itself: the caller still gets the refusal.
    fn record(&self, store: &Store, task: &str, label: &str) {
        let Ok(dir) = attempt_dir(store.repo(), task, label) else {
            return;
        };
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let value = json!({
            "action": self.action,
            "stage": self.stage.map(Stage::as_str),
            "reason": self.reason,
            "verdict": self.verdict.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "text": self.text(),
            "at": crate::store::plan_ops::utc_now(),
        });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = fs::write(dir.join(REVIEW_FILE), format!("{text}\n"));
        }
    }
}

/// What one verification did.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    /// The attempt the gate ran for.
    pub attempt_id: String,
    /// The state the task is in after verification.
    pub state: &'static str,
    /// The phase the task is in after verification.
    pub phase: String,
    /// The rework reason, when a stage failed.
    pub reason: Option<Reason>,
    /// The recorded verdict set digest, on a pass.
    pub digest: Option<String>,
    /// The stages whose existing verdicts were reused instead of rerun.
    pub reused: Vec<Stage>,
}

impl Verified {
    /// The one line `radian arc start` prints for the verification.
    pub fn line(&self) -> String {
        let reason = self
            .reason
            .as_ref()
            .map(|reason| format!(" ({})", reason.text()))
            .unwrap_or_default();
        format!(
            "{} verify -> {}.{}{reason}",
            self.attempt_id, self.state, self.phase
        )
    }
}

/// What one merge did.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub task: String,
    /// `done` or `review`.
    pub state: &'static str,
    pub phase: String,
    /// The rework reason, when the integration failed.
    pub reason: Option<String>,
    /// The commit the remote main now has, on a merge.
    pub candidate_sha: Option<String>,
    /// How many integration runs the command made.
    pub rounds: u32,
    /// A cleanup step that failed after the merge; the merge itself is done.
    pub warning: Option<String>,
}

impl Merged {
    /// The line `radian arc merge` prints.
    pub fn line(&self) -> String {
        let mut line = format!("{} {}.{}", self.task, self.state, self.phase);
        if let Some(candidate) = &self.candidate_sha {
            line.push_str(&format!(" ({candidate})"));
        }
        if let Some(reason) = &self.reason {
            line.push_str(&format!(" ({reason})"));
        }
        line
    }
}

/// What one reject did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub task: String,
    pub attempt_id: String,
    pub state: &'static str,
    pub phase: &'static str,
}

impl Rejected {
    /// The line `radian arc reject` prints.
    pub fn line(&self) -> String {
        format!("{} {}.{}", self.task, self.state, self.phase)
    }
}

// ---- verification --------------------------------------------------------------------

/// `review.verifying`: run the effective stages of the configured profile and the task's
/// extra stages against the pushed `head_sha`, except `human`.
///
/// The remote's `work/<task>` must still be `head_sha`; when it moved, the task stays in
/// `review.verifying` and the reason is recorded. A verdict that already exists for this
/// attempt and stage is reused, so a crash between stages resumes instead of rerunning.
///
/// A `commands` failure moves the task to `review.rework_pending`. Every required stage
/// passing moves it to `review.awaiting_human`, with `verified_sha` and
/// `verdict_set_digest` recorded.
///
/// # Errors
///
/// The task is not `review.verifying`, the remote moved, a listed stage is not built in
/// this build, a verdict for another sha exists, or the gate could not run.
pub fn verify(store: &Store, task_id: &str) -> Result<Verified, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    if task.state != model::TaskState::Review || task.phase != "verifying" {
        return Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; only a `review.verifying` task is verified",
            task.state.as_str(),
            task.phase
        )));
    }
    let label = attempt_label(&task)?;
    let head = task
        .head_sha
        .clone()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| Error::Refused(format!("task `{task_id}` has no `head_sha`")))?;
    let config = store.config()?;
    let (remote, main_branch) = store.repository_names()?;
    let main_ref = format!("refs/remotes/{remote}/{main_branch}");
    let profile = config.gate.selected_profile().cloned().ok_or_else(|| {
        Error::Failed(format!(
            "gate.profile is `{}` but no such profile exists",
            config.gate.profile
        ))
    })?;
    let extra = extra_stages(&task);

    // The pushed branch is the truth: `verified_sha` is the `head_sha` the remote has.
    let remote_sha = remote_work_sha(store.repo(), &remote, task_id)?;
    if remote_sha.as_deref() != Some(head.as_str()) {
        let reason = Reason::new(
            "verify",
            None,
            format!(
                "the remote `{remote}` has {} for `work/{task_id}`, but `head_sha` is {head}",
                remote_sha.as_deref().unwrap_or("no branch")
            ),
        );
        reason.record(store, task_id, &label);
        return Err(Error::Refused(reason.text()));
    }

    let stages = effective_stages(&profile, &extra);
    if let Some(spec) = stages
        .iter()
        .find(|spec| spec.stage != Stage::Human && !BUILT_STAGES.contains(&spec.stage))
    {
        let reason = Reason::new(
            "verify",
            Some(spec.stage),
            format!("stage {} is not built yet", spec.stage),
        );
        reason.record(store, task_id, &label);
        return Err(Error::Refused(reason.text()));
    }

    let mut reused = Vec::new();
    for spec in &stages {
        if spec.stage == Stage::Human {
            continue;
        }
        let path = verdict_path(store.repo(), task_id, &label, spec.stage).map_err(gate_error)?;
        if path.is_file() {
            let verdict = Verdict::read(&path).map_err(gate_error)?;
            if verdict.sha != head {
                let reason = Reason::with_verdict(
                    "verify",
                    spec.stage,
                    format!("the verdict is for {}, not {head}", verdict.sha),
                    path,
                );
                reason.record(store, task_id, &label);
                return Err(Error::Refused(reason.text()));
            }
            reused.push(spec.stage);
            if !verdict.is_pass() {
                return failed_stage(store, &task, &label, &head, &verdict, path);
            }
            continue;
        }
        let Stage::Commands = spec.stage else {
            // `effective_stages` cannot produce an unbuilt stage here: it was refused above.
            return Err(Error::Failed(format!(
                "stage {} is not built yet",
                spec.stage
            )));
        };
        let run = CommandRun {
            repository: store.repo(),
            remote: &remote,
            task: task_id,
            attempt: &label,
            sha: &head,
            main_ref: &main_ref,
            commands: &profile.commands,
            timeout: config.gate.command_timeout(),
        };
        let verdict = run.run().map_err(gate_error)?;
        if !verdict.is_pass() {
            return failed_stage(store, &task, &label, &head, &verdict, path);
        }
    }

    let verdicts = task_verdicts(store, task_id)?;
    let mut check_profile = profile;
    check_profile
        .stages
        .retain(|spec| spec.stage != Stage::Human);
    let check_extra: Vec<StageSpec> = extra
        .into_iter()
        .filter(|spec| spec.stage != Stage::Human)
        .collect();
    if !required_passed(&check_profile, &check_extra, &verdicts, &head) {
        return Err(Error::Failed(format!(
            "the gate recorded no verdict for every required stage of {head}"
        )));
    }
    let digest = verification_digest(&verdicts, &head);
    store.change_task(task_id, |task| {
        task.state = model::TaskState::Review;
        task.phase = "awaiting_human".to_owned();
        task.verified_sha = Some(head.clone());
        task.verdict_set_digest = Some(digest.clone());
        task.retry_at = None;
        Ok(())
    })?;
    // A pass leaves no rework reason behind for `show` to print.
    clear_reason(store, task_id, &label);
    Ok(Verified {
        attempt_id: label,
        state: "review",
        phase: "awaiting_human".to_owned(),
        reason: None,
        digest: Some(digest),
        reused,
    })
}

fn failed_stage(
    store: &Store,
    task: &model::TaskFile,
    label: &str,
    head: &str,
    verdict: &Verdict,
    path: PathBuf,
) -> Result<Verified, Error> {
    let reason = Reason::with_verdict("verify", verdict.stage, failure_text(verdict), path);
    reason.record(store, &task.id, label);
    store.change_task(&task.id, |task| {
        task.state = model::TaskState::Review;
        task.phase = "rework_pending".to_owned();
        task.verified_sha = Some(head.to_owned());
        task.verdict_set_digest = None;
        task.retry_at = None;
        Ok(())
    })?;
    Ok(Verified {
        attempt_id: label.to_owned(),
        state: "review",
        phase: "rework_pending".to_owned(),
        reason: Some(reason),
        digest: None,
        reused: Vec::new(),
    })
}

/// The task's `extra_stages` as stage specs. Every name was checked when the task file was
/// read; the profile's stages are only added, never weakened.
fn extra_stages(task: &model::TaskFile) -> Vec<StageSpec> {
    task.extra_stages
        .iter()
        .filter_map(|name| {
            crate::gate::LISTED_STAGES
                .iter()
                .copied()
                .find(|stage| stage.as_str() == name)
                .map(|stage| StageSpec {
                    stage,
                    required: true,
                })
        })
        .collect()
}

// ---- merge ---------------------------------------------------------------------------

/// `radian arc merge <task>`: the owner's approval, the integration candidate, the push to
/// the remote main and `done.merged` (ADR 0048 decisions 6 and 11).
///
/// The task must be `review.awaiting_human`, or `review.integrating`/`review.ready_to_merge`
/// from an earlier `merge` that main moved under. The remote `work/<task>` must still be
/// `verified_sha`, and the recomputed verification digest must equal the recorded one;
/// otherwise the task is refused (exit 3) and, for a moved branch, returns to
/// `review.verifying`.
///
/// A passing integration writes `review.ready_to_merge`; the push is
/// `git push <remote> <candidate>:refs/heads/<main>` with no force. A main that moved makes
/// the task integrate again, at most [`MAX_INTEGRATION_ROUNDS`] runs per command.
///
/// # Errors
///
/// The state does not allow the merge, the branch moved, the digest changed, the push was
/// not a fast-forward after every integration run, or a git call failed.
pub fn merge(store: &Store, task_id: &str) -> Result<Merged, Error> {
    merge_as(store, task_id, None)
}

/// [`merge`] with the approver to record in the `human` verdict: `None` is the owner's own
/// `radian arc merge`, `Some("auto")` is the scheduling loop at auto level `merge` (ADR 0048
/// decision 12).
pub fn merge_as(store: &Store, task_id: &str, approver: Option<&str>) -> Result<Merged, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    let resumable = task.state == model::TaskState::Review
        && matches!(task.phase.as_str(), "integrating" | "ready_to_merge");
    if task.state != model::TaskState::Review || (task.phase != "awaiting_human" && !resumable) {
        return Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; only a `review.awaiting_human` task merges",
            task.state.as_str(),
            task.phase
        )));
    }
    let label = attempt_label(&task)?;
    let head = task
        .head_sha
        .clone()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| Error::Refused(format!("task `{task_id}` has no `head_sha`")))?;
    let verified = task
        .verified_sha
        .clone()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| {
            Error::Refused(format!(
                "task `{task_id}` has no `verified_sha`; it was not verified"
            ))
        })?;
    if verified != head {
        return Err(Error::Refused(format!(
            "`verified_sha` {verified} is not `head_sha` {head}; verify the task again"
        )));
    }
    let config = store.config()?;
    let (remote, main_branch) = store.repository_names()?;
    let main_ref = format!("refs/remotes/{remote}/{main_branch}");

    // The branch as it is now: a move invalidates every verdict and the approval.
    let remote_sha = remote_work_sha(store.repo(), &remote, task_id)?;
    if remote_sha.as_deref() != Some(head.as_str()) {
        let reason = Reason::new(
            "merge",
            None,
            format!(
                "the remote `{remote}` has {} for `work/{task_id}`, but `verified_sha` is {head}",
                remote_sha.as_deref().unwrap_or("no branch")
            ),
        );
        reason.record(store, task_id, &label);
        return_to_verifying(store, task_id)?;
        return Err(Error::Refused(reason.text()));
    }

    // The approval is bound to the verified sha and the verdict set digest.
    let expected = task.verdict_set_digest.clone().ok_or_else(|| {
        Error::Refused(format!(
            "task `{task_id}` has no `verdict_set_digest`; it was not verified"
        ))
    })?;
    let verdicts = task_verdicts(store, task_id)?;
    let actual = verification_digest(&verdicts, &head);
    if actual != expected {
        // The approval no longer matches what the gate recorded for this commit. As with a
        // moved branch, every verdict for the old approval is stale, so the task goes back to
        // verification instead of waiting for a merge that can never happen.
        let reason = Reason::new(
            "merge",
            None,
            format!(
                "the verdict set of {head} changed since it was verified (recorded {expected}, \
                 now {actual}); the approval is invalid"
            ),
        );
        reason.record(store, task_id, &label);
        return_to_verifying(store, task_id)?;
        return Err(Error::Refused(reason.text()));
    }

    if task.phase == "awaiting_human" {
        if !required_stages_passed(store, task_id, &head)? {
            let reason = Reason::new(
                "merge",
                None,
                format!("every required stage must pass for {head} before the approval"),
            );
            reason.record(store, task_id, &label);
            return Err(Error::Refused(reason.text()));
        }
        approve(store, task_id, &label, &head, approver)?;
        store.change_task(task_id, |task| {
            if task.state == model::TaskState::Review && task.phase == "awaiting_human" {
                task.phase = "integrating".to_owned();
            }
            Ok(())
        })?;
    }

    let base = task
        .base_sha
        .clone()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| Error::Refused(format!("task `{task_id}` has no `base_sha`")))?;
    let profile = config.gate.selected_profile().cloned().ok_or_else(|| {
        Error::Failed(format!(
            "gate.profile is `{}` but no such profile exists",
            config.gate.profile
        ))
    })?;
    let timeout = config.gate.command_timeout();
    let mut last = String::new();
    for round in 1..=MAX_INTEGRATION_ROUNDS {
        let integration_label = next_integration_label(store, task_id, &label)?;
        let verdict = IntegrationRun {
            repository: store.repo(),
            remote: &remote,
            task: task_id,
            attempt: &integration_label,
            head_sha: &head,
            base_sha: &base,
            main_ref: &main_ref,
            key_commands: &profile.key_commands,
            timeout,
        }
        .run()
        .map_err(gate_error)?;
        if !verdict.is_pass() {
            let reason = Reason::with_verdict(
                "merge",
                verdict.stage,
                failure_text(&verdict),
                integration_verdict(store, task_id, &integration_label)?,
            );
            reason.record(store, task_id, &label);
            to_rework(store, task_id)?;
            return Ok(Merged {
                task: task_id.to_owned(),
                state: "review",
                phase: "rework_pending".to_owned(),
                reason: Some(reason.text()),
                candidate_sha: None,
                rounds: round,
                warning: None,
            });
        }
        let candidate = match verdict.candidate_sha.clone() {
            Some(sha) if !sha.is_empty() => sha,
            _ => {
                return Err(Error::Failed(format!(
                    "the integration verdict of `{task_id}` passed without a candidate"
                )));
            }
        };
        store.change_task(task_id, |task| {
            if task.state == model::TaskState::Review {
                task.phase = "ready_to_merge".to_owned();
            }
            Ok(())
        })?;
        if !merge_ready(store.repo(), &remote, &main_ref, &verdict).map_err(gate_error)? {
            last = "main moved after integration".to_owned();
            back_to_integrating(store, task_id)?;
            continue;
        }
        match push_candidate(store.repo(), &remote, &candidate, &main_branch)? {
            Push::Pushed => {}
            Push::Rejected(stderr) => {
                last = format!("the push to `{remote}` `{main_branch}` was rejected: {stderr}");
                back_to_integrating(store, task_id)?;
                continue;
            }
        }
        let remote_main = ls_remote_branch(store.repo(), &remote, &main_branch)?;
        if remote_main.as_deref() != Some(candidate.as_str()) {
            last = format!(
                "`{remote}` `{main_branch}` is {}, not {candidate}",
                remote_main.as_deref().unwrap_or("missing")
            );
            back_to_integrating(store, task_id)?;
            continue;
        }
        if !is_ancestor(store.repo(), &head, &candidate)? {
            return Err(Error::Failed(format!(
                "{candidate} does not contain the verified {head}"
            )));
        }
        store.change_task(task_id, |task| {
            task.state = model::TaskState::Done;
            task.phase = "merged".to_owned();
            task.retry_at = None;
            Ok(())
        })?;
        let warning = remove_worktree(store, task_id).err();
        return Ok(Merged {
            task: task_id.to_owned(),
            state: "done",
            phase: "merged".to_owned(),
            reason: None,
            candidate_sha: Some(candidate),
            rounds: round,
            warning,
        });
    }

    back_to_integrating(store, task_id)?;
    Err(Error::Refused(format!(
        "{last}; {MAX_INTEGRATION_ROUNDS} integration runs did not settle, so the task stays \
         in `review.integrating`"
    )))
}

/// Writes the owner's `human` pass verdict for `sha` (once), or reuses the one a crash left.
fn approve(
    store: &Store,
    task: &str,
    label: &str,
    sha: &str,
    approver: Option<&str>,
) -> Result<(), Error> {
    let path = verdict_path(store.repo(), task, label, Stage::Human).map_err(gate_error)?;
    if path.is_file() {
        let verdict = Verdict::read(&path).map_err(gate_error)?;
        if !verdict.is_pass() || verdict.sha != sha {
            return Err(Error::Refused(format!(
                "the approval verdict {} is not a pass for {sha}",
                path.display()
            )));
        }
        return Ok(());
    }
    let verdict = match approver {
        Some(approver) => Verdict::approved_by(sha, approver),
        None => Verdict::human(sha, true, None),
    };
    verdict.write_new(&path).map_err(gate_error)
}

/// The text of a failed stage: its reason, and the conflicted paths when it is `conflict`.
fn failure_text(verdict: &Verdict) -> String {
    let reason = verdict
        .reason
        .clone()
        .unwrap_or_else(|| "failed".to_owned());
    if reason == "conflict" && !verdict.conflict_paths.is_empty() {
        format!("conflict: {}", verdict.conflict_paths.join(", "))
    } else {
        reason
    }
}

fn integration_verdict(store: &Store, task: &str, label: &str) -> Result<PathBuf, Error> {
    verdict_path(store.repo(), task, label, Stage::Integration).map_err(gate_error)
}

fn to_rework(store: &Store, task_id: &str) -> Result<(), Error> {
    store.change_task(task_id, |task| {
        if task.state == model::TaskState::Review {
            task.phase = "rework_pending".to_owned();
        }
        Ok(())
    })?;
    Ok(())
}

fn back_to_integrating(store: &Store, task_id: &str) -> Result<(), Error> {
    store.change_task(task_id, |task| {
        if task.state == model::TaskState::Review {
            task.phase = "integrating".to_owned();
        }
        Ok(())
    })?;
    Ok(())
}

/// Decision 6, first bullet under the state table: a branch that moved invalidates every
/// verdict and the approval of the old sha. `verified_sha` and the digest are cleared, so
/// only a fresh verify can approve the new tip.
fn return_to_verifying(store: &Store, task_id: &str) -> Result<(), Error> {
    store.change_task(task_id, |task| {
        if task.state == model::TaskState::Review {
            task.phase = "verifying".to_owned();
            task.verified_sha = None;
            task.verdict_set_digest = None;
        }
        Ok(())
    })?;
    Ok(())
}

/// The next free integration label for one attempt: the attempt id, then `<attempt id>.i2`,
/// `.i3`, … Each integration run writes its own verdict, which is never changed.
fn next_integration_label(store: &Store, task: &str, base: &str) -> Result<String, Error> {
    for n in 1..=1000u32 {
        let label = if n == 1 {
            base.to_owned()
        } else {
            format!("{base}.i{n}")
        };
        let path =
            verdict_path(store.repo(), task, &label, Stage::Integration).map_err(gate_error)?;
        if !path.is_file() {
            return Ok(label);
        }
    }
    Err(Error::Failed(format!(
        "no free integration attempt left for `{task}`"
    )))
}

// ---- reject --------------------------------------------------------------------------

/// `radian arc reject <task> --note <text>`: the owner's refusal of a `review.awaiting_human`
/// task (ADR 0048 decision 11).
///
/// The note goes to the attempt's `reject.md`, and the task moves to
/// `review.rework_pending`; `show` prints the note as the rework reason.
///
/// # Errors
///
/// The task is not `review.awaiting_human`, the note is empty, or the file changed outside
/// arc.
pub fn reject(store: &Store, task_id: &str, note: &str) -> Result<Rejected, Error> {
    if note.trim().is_empty() {
        return Err(Error::Usage("the reject note is empty".to_owned()));
    }
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    if task.state != model::TaskState::Review || task.phase != "awaiting_human" {
        return Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; only a `review.awaiting_human` task is rejected",
            task.state.as_str(),
            task.phase
        )));
    }
    let label = attempt_label(&task)?;
    let dir = attempt_dir(store.repo(), task_id, &label).map_err(gate_error)?;
    fs::create_dir_all(&dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let path = dir.join(REJECT_FILE);
    fs::write(&path, format!("{}\n", note.trim_end()))
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))?;
    store.change_task(task_id, |task| {
        if task.state != model::TaskState::Review || task.phase != "awaiting_human" {
            return Err(Error::Refused(format!(
                "task `{task_id}` is `{}.{}`; only a `review.awaiting_human` task is rejected",
                task.state.as_str(),
                task.phase
            )));
        }
        task.phase = "rework_pending".to_owned();
        Ok(())
    })?;
    Ok(Rejected {
        task: task_id.to_owned(),
        attempt_id: label,
        state: "review",
        phase: "rework_pending",
    })
}

// ---- reasons and verdicts ------------------------------------------------------------

/// Whether every required stage of the task's effective profile passed for `head`. This is
/// the owner's part of an approval (ADR 0048 decisions 11 and 12): a merge binds to the
/// verified sha and the digest only when the gate really passed every required stage.
fn required_stages_passed(store: &Store, task_id: &str, head: &str) -> Result<bool, Error> {
    let task = store.load_task(task_id)?.item;
    let config = store.config()?;
    let Some(profile) = config.gate.selected_profile().cloned() else {
        return Ok(false);
    };
    let extra = extra_stages(&task);
    let verdicts = task_verdicts(store, task_id)?;
    let mut check_profile = profile;
    check_profile
        .stages
        .retain(|spec| spec.stage != Stage::Human);
    let check_extra: Vec<StageSpec> = extra
        .into_iter()
        .filter(|spec| spec.stage != Stage::Human)
        .collect();
    Ok(required_passed(
        &check_profile,
        &check_extra,
        &verdicts,
        head,
    ))
}

/// What `show` prints for a task in `review.verifying` or `review.rework_pending`: the
/// owner's reject note, or the last verify/merge reason (a failed stage and its verdict, a
/// moved branch, a stage this build does not run yet), plus why the plan paused when an
/// automatic rework limit was reached.
#[must_use]
pub fn rework_reason(store: &Store, task: &model::TaskFile) -> Option<String> {
    if task.state != model::TaskState::Review
        || !matches!(task.phase.as_str(), "verifying" | "rework_pending")
    {
        return None;
    }
    let label = attempt_label(task).ok()?;
    let dir = attempt_dir(store.repo(), &task.id, &label).ok()?;
    let mut parts = Vec::new();
    if let Ok(note) = fs::read_to_string(dir.join(REJECT_FILE)) {
        let note = note.trim();
        if !note.is_empty() {
            parts.push(note.to_owned());
        }
    }
    if parts.is_empty()
        && let Ok(text) = fs::read_to_string(dir.join(REVIEW_FILE))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
        && let Some(text) = value.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        parts.push(text.to_owned());
    }
    // The automatic rework limit: the task stays in `rework_pending` and `show` says why the
    // plan stopped (ADR 0048 decision 12).
    if let Ok(text) = fs::read_to_string(dir.join(crate::store::attempt_ops::PAUSED_FILE))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
        && let Some(text) = value.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        parts.push(text.to_owned());
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn clear_reason(store: &Store, task: &str, label: &str) {
    let Ok(dir) = attempt_dir(store.repo(), task, label) else {
        return;
    };
    let _ = fs::remove_file(dir.join(REVIEW_FILE));
}

/// Every verdict file of every attempt of `task`, in attempt order.
fn task_verdicts(store: &Store, task: &str) -> Result<Vec<Verdict>, Error> {
    let attempts = store.root().join("runs").join(task).join("attempts");
    let mut verdicts = Vec::new();
    let Ok(entries) = fs::read_dir(&attempts) else {
        return Ok(verdicts);
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let verdict_dir = dir.join("verdicts");
        let Ok(entries) = fs::read_dir(&verdict_dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect();
        files.sort();
        for path in files {
            verdicts.push(Verdict::read(&path).map_err(gate_error)?);
        }
    }
    Ok(verdicts)
}

/// The digest of the verification set for `sha`: every counted verdict except the owner's
/// `human` approval and the `integration` candidate verdict. Both are written after
/// verification, at merge, so they cannot change the digest the approval binds to; a new
/// `commands` verdict for the same sha does change it.
#[must_use]
pub fn verification_digest(verdicts: &[Verdict], sha: &str) -> String {
    let set: Vec<Verdict> = verdicts
        .iter()
        .filter(|verdict| !matches!(verdict.stage, Stage::Human | Stage::Integration))
        .cloned()
        .collect();
    verdict_set_digest(&set, sha)
}

/// The attempt id of the task's current attempt, which names its run directory.
fn attempt_label(task: &model::TaskFile) -> Result<String, Error> {
    task.attempt_id
        .clone()
        .filter(|label| !label.is_empty())
        .or_else(|| (task.attempt > 0).then(|| format!("{}-{}", task.id, task.attempt)))
        .ok_or_else(|| Error::Refused(format!("task `{}` has no attempt", task.id)))
}

// ---- git ------------------------------------------------------------------------------

fn gate_error(error: crate::gate::GateError) -> Error {
    Error::Failed(error.to_string())
}

fn git(dir: &Path, args: &[&str]) -> Result<crate::git::GitOutput, Error> {
    crate::git::run(dir, args).map_err(|error| Error::Failed(error.to_string()))
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let out = git(dir, args)?;
    if !out.success() {
        return Err(Error::Failed(format!(
            "git {} failed: {}",
            args.join(" "),
            out.stderr.trim()
        )));
    }
    Ok(out.stdout.trim().to_string())
}

/// The remote's `work/<task>` after a fetch, when it exists.
fn remote_work_sha(repo: &Path, remote: &str, task: &str) -> Result<Option<String>, Error> {
    git_ok(repo, &["fetch", remote])?;
    remote_branch(repo, remote, &format!("work/{task}"))
}

/// The remote's branch after a fetch, when it exists.
fn remote_branch(repo: &Path, remote: &str, branch: &str) -> Result<Option<String>, Error> {
    let reference = format!("refs/remotes/{remote}/{branch}");
    let out = git(repo, &["rev-parse", "--verify", "--quiet", &reference])?;
    if !out.success() {
        return Ok(None);
    }
    let sha = out.stdout.trim();
    Ok((!sha.is_empty()).then(|| sha.to_owned()))
}

/// The remote's branch read over the wire (`git ls-remote`), when it exists.
fn ls_remote_branch(repo: &Path, remote: &str, branch: &str) -> Result<Option<String>, Error> {
    let reference = format!("refs/heads/{branch}");
    let out = git_ok(repo, &["ls-remote", remote, &reference])?;
    let sha = out.split_whitespace().next().unwrap_or_default();
    Ok((!sha.is_empty()).then(|| sha.to_owned()))
}

enum Push {
    Pushed,
    /// The remote main moved under the candidate. The caller integrates again.
    Rejected(String),
}

fn push_candidate(
    repo: &Path,
    remote: &str,
    candidate: &str,
    main_branch: &str,
) -> Result<Push, Error> {
    // Never `--force`: a main that moved must reject the push, not lose its commits.
    let refspec = format!("{candidate}:refs/heads/{main_branch}");
    let out = git(repo, &["push", remote, &refspec])?;
    if out.success() {
        return Ok(Push::Pushed);
    }
    let stderr = out.stderr.trim().to_owned();
    let raced = stderr.contains("non-fast-forward")
        || stderr.contains("fetch first")
        || stderr.contains("[rejected]");
    if raced {
        Ok(Push::Rejected(stderr))
    } else {
        Err(Error::Failed(format!(
            "git push {remote} {refspec} failed: {stderr}"
        )))
    }
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, Error> {
    let out = git(repo, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    Ok(out.success())
}

/// Removes the task's worktree after a merge. The branch stays on the remote: only the
/// checkout goes. A dirty worktree is not forced away: the merge stands, and the failure is
/// a warning.
fn remove_worktree(store: &Store, task: &str) -> Result<(), String> {
    let path = store.root().join("worktrees").join(task);
    if path.exists() {
        let path = path.to_string_lossy().into_owned();
        let out = crate::git::run(store.repo(), &["worktree", "remove", &path])
            .map_err(|error| error.to_string())?;
        if !out.success() {
            return Err(format!(
                "cannot remove worktree {path}: {}",
                out.stderr.trim()
            ));
        }
    }
    let out =
        crate::git::run(store.repo(), &["worktree", "prune"]).map_err(|error| error.to_string())?;
    if !out.success() {
        return Err(format!("git worktree prune failed: {}", out.stderr.trim()));
    }
    Ok(())
}

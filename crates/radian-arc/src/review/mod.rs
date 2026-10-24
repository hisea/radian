//! Verification, merge and reject: the review half of the state machine (ADR 0048
//! decisions 6, 10 and 11).
//!
//! - [`verify`] runs the gate's built stages against the verification candidate for the
//!   task's pushed `head_sha` and moves the task to `review.awaiting_human` or
//!   `review.rework_pending`. A conflict with main, when the profile merges main before an
//!   attempt, moves it to `todo.ready` with a brief arc writes itself (ADR 0048 decision 9).
//!   It is split into [`plan_verification`], [`run_stages`] (which writes verdicts only) and
//!   [`finish_verification`] (which moves the state), so a background gate can run the stages
//!   and a later pass can apply them. The candidate is
//!   `head_sha` when the remote main is its ancestor, else a merge of `head_sha` onto the
//!   remote main (ADR 0048 decision 10); `verified_sha` stays the task's own commit.
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

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;

use crate::gate::{
    CommandRun, IntegrationRun, PreviousReview, ReviewItem, ReviewRun, Stage, StageSpec, Verdict,
    attempt_dir, effective_stages, merge_ready, required_passed, verdict_path, verdict_set_digest,
};
use crate::store::attempt_ops::Paused;
use crate::store::{Error, Store, model};

/// The file in an attempt's run directory that holds the last verify or merge reason.
pub const REVIEW_FILE: &str = "review.json";
/// The owner's reject note, in the attempt's run directory.
pub const REJECT_FILE: &str = "reject.md";
/// How many integration runs one `merge` command may make after main moved.
pub const MAX_INTEGRATION_ROUNDS: u32 = 3;
/// The stages this build can run in verification. A listed stage outside it refuses with
/// "stage <name> is not built yet".
pub const BUILT_STAGES: [Stage; 2] = [Stage::Commands, Stage::Review];
/// The file in a task's run directory that records the pass's automatic reverifies (ADR
/// 0048 decision 9): the `(head_sha, remote main)` pairs already re-verified, and the text of
/// the last `[limits] max_auto_reverify` stop.
pub const AUTO_REVERIFY_FILE: &str = "auto_reverify.json";
/// The failure reasons a moved main can fix: a `commands` or integration command that failed,
/// or ran past its timeout (ADR 0048 decision 9).
pub const AUTO_REVERIFY_REASONS: &[&str] = &["command_failed", "timeout"];
/// Test-only (an input that exists only for tests is not in the manual, AGENTS.md): the program
/// a background gate runs instead of `radian exec` for the `review` stage, the same seam
/// [`ReviewRun::program`] gives the synchronous path. A test sets it on the `radian arc run`
/// process, which its gate child inherits. Never set by arc.
pub const REVIEW_PROGRAM_ENV: &str = "RADIAN_ARC_GATE_TEST_REVIEW_PROGRAM";

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

/// A conflict with main whose rework arc wrote itself (ADR 0048 decision 9). The task went to
/// `todo.ready`, or stayed in `review.rework_pending` when the run of conflicts reached
/// `[limits] max_conflict_rework`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The remote main commit the change conflicted with.
    pub main_sha: String,
    /// The stage that conflicted.
    pub stage: Stage,
    /// The conflicted paths, as the verdict lists them.
    pub paths: Vec<String>,
    /// The stop at `[limits] max_conflict_rework`: no brief was written, the task stays in
    /// `review.rework_pending`, and this is why `run` and `show` say so.
    pub stop: Option<String>,
}

/// The one line a conflict-with-main transition prints.
fn conflict_line(task: &str, conflict: &Conflict) -> String {
    match &conflict.stop {
        Some(text) => format!("{task} conflict with main -> review.rework_pending: {text}"),
        None => format!("{task} conflict with main -> todo.ready (automatic rework)"),
    }
}

/// The review rework arc wrote itself (ADR 0048 decision 9): the task went to `todo.ready`
/// with a brief that lists the review's blocking items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRework {
    /// The attempt's review verdict, which the brief tells the worker to read.
    pub verdict: PathBuf,
    /// The blocking items the brief lists, as the verdict recorded them.
    pub items: Vec<ReviewItem>,
}

/// The one line a review rework's transition prints.
fn review_line(task: &str) -> String {
    format!("{task} review blocking -> todo.ready (automatic rework)")
}

/// What one verification did.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    /// The task the verification ran for.
    pub task: String,
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
    /// Set when a conflict with main wrote the next attempt's brief instead of waiting for
    /// the lead.
    pub conflict: Option<Conflict>,
    /// Set when a failed review wrote the next attempt's brief instead of waiting for the lead.
    pub review: Option<ReviewRework>,
    /// Set when the automatic rework count paused the plan; the task stays in
    /// `review.rework_pending` and `run` prints the pause.
    pub paused: Option<Paused>,
}

impl Verified {
    /// The one line `radian arc start` prints for the verification.
    pub fn line(&self) -> String {
        if let Some(conflict) = &self.conflict {
            return conflict_line(&self.task, conflict);
        }
        // A paused plan did not move the task to `todo.ready`, so it prints the failure line
        // and the pause line follows it.
        if self.review.is_some() && self.paused.is_none() {
            return review_line(&self.task);
        }
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

/// What one `radian arc reverify` did: the worker-less attempt, then the verification that
/// followed it. The verification may refuse (a moved branch, a stage this build does not
/// run); the task then stays in `review.verifying`, as it does after a pushed attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct Reverified {
    pub attempt: u32,
    pub attempt_id: String,
    pub verification: Result<Verified, Error>,
}

impl Reverified {
    /// The lines `radian arc reverify` prints: the claim's own line, then the verification's
    /// when it ran.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("{} reverify -> review.verifying", self.attempt_id)];
        if let Ok(verified) = &self.verification {
            lines.push(verified.line());
        }
        lines
    }
}

/// The follow-up task arc proposed for a merged task's review (ADR 0048 decision 10): the
/// items the task's last review suggested, which the merge left unfixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followup {
    /// The new task's id.
    pub task: String,
    /// The merged task whose review's items it carries.
    pub from: String,
}

impl Followup {
    /// The one line the merge prints for it.
    pub fn line(&self) -> String {
        format!("{} proposed (follow-ups from {})", self.task, self.from)
    }
}

/// What one merge did.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub task: String,
    /// `done`, `review` or `todo`.
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
    /// Set when a conflict with main wrote the next attempt's brief instead of waiting for
    /// the lead.
    pub conflict: Option<Conflict>,
    /// The follow-up task arc proposed from the merged task's review, if any (ADR 0048
    /// decision 10).
    pub followup: Option<Followup>,
}

impl Merged {
    /// The line `radian arc merge` prints.
    pub fn line(&self) -> String {
        if let Some(conflict) = &self.conflict {
            return conflict_line(&self.task, conflict);
        }
        let mut line = format!("{} {}.{}", self.task, self.state, self.phase);
        if let Some(candidate) = &self.candidate_sha {
            line.push_str(&format!(" ({candidate})"));
        }
        if let Some(reason) = &self.reason {
            line.push_str(&format!(" ({reason})"));
        }
        if let Some(followup) = &self.followup {
            line.push('\n');
            line.push_str(&followup.line());
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
/// A `commands`, `mutation` or `review` failure moves the task to `review.rework_pending`, or
/// to `todo.ready` with a brief arc writes when it is a conflict with main and the profile
/// merges main before an attempt (ADR 0048 decision 9). Every required stage passing moves it
/// to `review.awaiting_human`, with `verified_sha` and `verdict_set_digest` recorded.
///
/// # Errors
///
/// The task is not `review.verifying`, the remote moved, a listed stage is not built in
/// this build, a verdict for another sha exists, or the gate could not run.
pub fn verify(store: &Store, task_id: &str) -> Result<Verified, Error> {
    verify_with(store, task_id, None)
}

/// [`verify`] with an optional review program (test-only; see [`ReviewRun::program`]).
///
/// This is the synchronous path (`radian arc start`, `radian arc reverify`): it runs the
/// stages and applies the verdicts in one call. A scheduling pass starts the same stages in
/// a background gate and applies them in a later pass.
///
/// # Errors
///
/// As [`verify`].
pub fn verify_with(
    store: &Store,
    task_id: &str,
    review_program: Option<&Path>,
) -> Result<Verified, Error> {
    // One gate per task: a background gate may hold this lock. Waiting here means two
    // verifications never run the same stages at once; when the gate that held it is done,
    // its verdicts are reused below.
    let _gate = crate::gate::background::lock_task(store, task_id)?;
    let plan = plan_verification(store, task_id)?;
    let reused = run_stages(store, &plan, review_program)?;
    let mut verified = finish_verification(store, &plan)?;
    verified.reused = reused;
    Ok(verified)
}

/// Everything one verification needs, read and checked once (ADR 0048 decision 10).
pub struct Verification {
    /// The task as it was read: its id, brief, shas and limits.
    pub task: model::TaskFile,
    /// The attempt id that names the verdict directory.
    pub label: String,
    /// The task's `head_sha`: the commit the gate verifies.
    pub head: String,
    /// The remote name.
    pub remote: String,
    /// `refs/remotes/<remote>/<main>`.
    pub main_ref: String,
    /// The effective profile.
    pub profile: crate::gate::Profile,
    /// The effective stages, in order (the profile's plus the task's extra stages).
    pub stages: Vec<StageSpec>,
    /// `config.toml`, for the stage timeouts and the review route.
    pub config: crate::store::config::Config,
    /// A digest of what this verification depends on: `config.toml`, the task's shas, its
    /// extra stages, and the remote `work/<task>` sha. A pass compares it with the one a
    /// refused gate recorded, so an unchanged refusal is not retried.
    pub inputs: String,
}

/// Reads and checks one `review.verifying` task's verification: its state, attempt, shas,
/// profile and stages, and the remote `work/<task>`. A refusal records its reason in the
/// attempt's `review.json` and leaves the task in `review.verifying`.
///
/// # Errors
///
/// The task is not `review.verifying`, it has no `head_sha`, the remote moved, a listed
/// stage is not built in this build, or `config.toml` could not be read.
pub fn plan_verification(store: &Store, task_id: &str) -> Result<Verification, Error> {
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
    let inputs = verification_inputs(store, &task, remote_sha.as_deref().unwrap_or_default());
    Ok(Verification {
        task,
        label,
        head,
        remote,
        main_ref,
        profile,
        stages,
        config,
        inputs,
    })
}

/// A digest of what a verification depends on: `config.toml`, the task's `head_sha`,
/// `base_sha`, extra stages and `allow_noop`, and the remote `work/<task>` sha. Two runs with
/// the same digest would refuse for the same reason, so a pass does not start their gate
/// again (ADR 0048 decision 10).
fn verification_inputs(store: &Store, task: &model::TaskFile, remote_sha: &str) -> String {
    let config = fs::read(store.root().join(crate::store::CONFIG_FILE)).unwrap_or_default();
    let fields = json!({
        "head": task.head_sha,
        "base": task.base_sha,
        "extra": task.extra_stages,
        "allow_noop": task.allow_noop,
    });
    let mut hasher = sha2::Sha256::new();
    hasher.update(&config);
    hasher.update(fields.to_string().as_bytes());
    hasher.update(remote_sha.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// A digest of what an integration depends on: `config.toml`, the task's `head_sha` and
/// `base_sha`, and the remote main over the wire. A pass records it when it starts an
/// integration gate, so a gate that exits without a record is not started again while these
/// inputs are unchanged, and a new head, a new main or a changed profile allows one new start
/// (ADR 0048 decision 10). `None` when the config, the task or the remote cannot be read.
#[must_use]
pub fn integration_inputs(store: &Store, task_id: &str) -> Option<String> {
    let task = store.load_task(task_id).ok()?.item;
    let config = fs::read(store.root().join(crate::store::CONFIG_FILE)).unwrap_or_default();
    let (remote, main_branch) = store.repository_names().ok()?;
    let main = ls_remote_branch(store.repo(), &remote, &main_branch).ok()??;
    let fields = json!({
        "kind": "integration",
        "head": task.head_sha,
        "base": task.base_sha,
        "main": main,
    });
    let mut hasher = sha2::Sha256::new();
    hasher.update(&config);
    hasher.update(fields.to_string().as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

/// Runs the stages the plan lists that have no verdict yet, in order, and stops at the first
/// failure. `commands` and `review` are the exception: when the profile lists both and neither
/// has a verdict, the two run at the same time, and the round is decided in the profile order
/// afterwards (ADR 0048 decision 10). A verdict already written for this attempt and stage is
/// reused, so a gate that died between stages resumes instead of rerunning, and a pair a gate
/// died under resumes only the stage without a verdict. This writes verdicts only: it never
/// moves the task's state. It returns the stages whose verdict was reused.
///
/// # Errors
///
/// A verdict exists for another sha, a stage this build does not run was reached, or the gate
/// could not run (a git or file error, a failing checkout, a stop).
pub fn run_stages(
    store: &Store,
    plan: &Verification,
    review_program: Option<&Path>,
) -> Result<Vec<Stage>, Error> {
    let mut reused = Vec::new();
    let pair = pair_positions(plan);
    let mut index = 0;
    while index < plan.stages.len() {
        let spec = plan.stages[index];
        if spec.stage == Stage::Human {
            index += 1;
            continue;
        }
        if let Some((commands_at, review_at)) = pair
            && index == commands_at
        {
            if !run_pair(store, plan, review_program, &mut reused)? {
                return Ok(reused);
            }
            index = review_at + 1;
            continue;
        }
        let verdict = match recorded_verdict(store, plan, &spec)? {
            Some(verdict) => {
                reused.push(spec.stage);
                verdict
            }
            None => run_stage(store, plan, &spec, review_program)?,
        };
        if !verdict.is_pass() {
            return Ok(reused);
        }
        index += 1;
    }
    Ok(reused)
}

/// The `(commands, review)` positions when the plan lists both. `Profile::validate` refuses
/// `review` before `commands`, so the two are in that order; an extra stage is only appended,
/// so a pair with `review` first cannot arise from a valid task either.
fn pair_positions(plan: &Verification) -> Option<(usize, usize)> {
    let commands = plan
        .stages
        .iter()
        .position(|spec| spec.stage == Stage::Commands)?;
    let review = plan
        .stages
        .iter()
        .position(|spec| spec.stage == Stage::Review)?;
    (commands < review).then_some((commands, review))
}

/// The verdict already written for `spec` in this attempt, when there is one. A verdict for
/// another sha refuses the verification and records the reason (ADR 0048 decision 10).
fn recorded_verdict(
    store: &Store,
    plan: &Verification,
    spec: &StageSpec,
) -> Result<Option<Verdict>, Error> {
    let path =
        verdict_path(store.repo(), &plan.task.id, &plan.label, spec.stage).map_err(gate_error)?;
    if !path.is_file() {
        return Ok(None);
    }
    let verdict = Verdict::read(&path).map_err(gate_error)?;
    if verdict.sha != plan.head {
        let reason = Reason::with_verdict(
            "verify",
            spec.stage,
            format!("the verdict is for {}, not {}", verdict.sha, plan.head),
            path,
        );
        reason.record(store, &plan.task.id, &plan.label);
        return Err(Error::Refused(reason.text()));
    }
    Ok(Some(verdict))
}

/// Runs the `commands`/`review` pair (ADR 0048 decision 10). A stage that already has a verdict
/// is reused; when both are pending they start at the same time and the gate waits for both,
/// so a failure of one never kills the other. Returns whether the round goes on, decided in
/// the profile order (`commands` first): the other verdict is always written before this
/// returns.
fn run_pair(
    store: &Store,
    plan: &Verification,
    review_program: Option<&Path>,
    reused: &mut Vec<Stage>,
) -> Result<bool, Error> {
    let commands_spec = plan
        .stages
        .iter()
        .find(|spec| spec.stage == Stage::Commands)
        .ok_or_else(|| Error::Failed("the plan lists no commands stage".to_owned()))?;
    let review_spec = plan
        .stages
        .iter()
        .find(|spec| spec.stage == Stage::Review)
        .ok_or_else(|| Error::Failed("the plan lists no review stage".to_owned()))?;
    let recorded_commands = recorded_verdict(store, plan, commands_spec)?;
    let recorded_review = recorded_verdict(store, plan, review_spec)?;
    if recorded_commands.is_some() {
        reused.push(Stage::Commands);
    }
    if recorded_review.is_some() {
        reused.push(Stage::Review);
    }
    let (commands, review) = match (recorded_commands, recorded_review) {
        (Some(commands), Some(review)) => (commands, review),
        (Some(commands), None) => (
            commands,
            run_stage(store, plan, review_spec, review_program)?,
        ),
        (None, Some(review)) => (
            run_stage(store, plan, commands_spec, review_program)?,
            review,
        ),
        (None, None) => {
            let (commands, review) = std::thread::scope(|scope| {
                let commands =
                    scope.spawn(|| run_stage(store, plan, commands_spec, review_program));
                let review = scope.spawn(|| run_stage(store, plan, review_spec, review_program));
                (commands.join(), review.join())
            });
            (joined(commands)?, joined(review)?)
        }
    };
    Ok(commands.is_pass() && review.is_pass())
}

/// A stage thread's result. A panic in a stage is the gate's panic, not a stage result, so it
/// keeps its behaviour.
fn joined(result: std::thread::Result<Result<Verdict, Error>>) -> Result<Verdict, Error> {
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Runs one stage and returns the verdict it wrote.
fn run_stage(
    store: &Store,
    plan: &Verification,
    spec: &StageSpec,
    review_program: Option<&Path>,
) -> Result<Verdict, Error> {
    let task = &plan.task;
    match spec.stage {
        Stage::Commands => CommandRun {
            repository: store.repo(),
            remote: &plan.remote,
            task: &task.id,
            attempt: &plan.label,
            sha: &plan.head,
            main_ref: &plan.main_ref,
            commands: &plan.profile.commands,
            timeout: plan.config.gate.command_timeout(),
        }
        .run()
        .map_err(gate_error),
        Stage::Review => {
            let review = &plan.config.gate.review;
            let (exec_args, key) = match &review.exec_args {
                Some(args) => (args.as_slice(), "gate.review.exec_args"),
                None => (plan.config.worker.exec_args.as_slice(), "worker.exec_args"),
            };
            // The stage owns the read-only mount and the session flags: an argument that
            // repeated one would overrule them (ADR 0048 decision 10).
            if let Some(owned) = exec_args
                .iter()
                .find_map(|arg| crate::gate::owned_name(arg))
            {
                return Err(Error::Refused(format!(
                    "`{key}` must not repeat `{owned}`: the review stage sets it on the \
                     review session's `radian exec` line"
                )));
            }
            let prompt_file = review
                .prompt_file
                .as_ref()
                .map(|file| store.repo().join(file));
            // A review that follows an earlier one of this task is a rework round: the stage
            // reviews the increment and judges the items arc moved (ADR 0048 decision 10).
            let earlier = previous_review(store, &task.id, &plan.label);
            let earlier = earlier.as_ref().map(|(_, verdict)| PreviousReview {
                sha: verdict.sha.clone(),
                blocking: verdict
                    .review
                    .as_ref()
                    .map(|review| review.blocking.clone())
                    .unwrap_or_default(),
            });
            ReviewRun {
                repository: store.repo(),
                remote: &plan.remote,
                task: &task.id,
                attempt: &plan.label,
                sha: &plan.head,
                brief: &task.body,
                main_ref: &plan.main_ref,
                exec_args,
                prompt_file: prompt_file.as_deref(),
                previous: earlier.as_ref(),
                timeout: review.timeout(),
                program: review_program,
            }
            .run()
            .map_err(gate_error)
        }
        other => {
            // `plan_verification` refused an unbuilt stage before this point.
            Err(Error::Failed(format!("stage {other} is not built yet")))
        }
    }
}

/// Applies the verdicts a gate left for `plan` (ADR 0048 decisions 6 and 10): a failed stage
/// moves the task to `review.rework_pending`; every required stage passing moves it to
/// `review.awaiting_human` with `verified_sha` and the verdict set digest. Only a pass that
/// holds `runner.lock` calls this; a background gate never moves the state itself.
///
/// # Errors
///
/// A stage left no verdict, a verdict is for another sha, or the task file changed outside
/// arc.
pub fn finish_verification(store: &Store, plan: &Verification) -> Result<Verified, Error> {
    let task = &plan.task;
    for spec in &plan.stages {
        if spec.stage == Stage::Human {
            continue;
        }
        let path =
            verdict_path(store.repo(), &task.id, &plan.label, spec.stage).map_err(gate_error)?;
        if !path.is_file() {
            return Err(Error::Failed(format!(
                "the gate of {} left no {} verdict",
                plan.label, spec.stage
            )));
        }
        let verdict = Verdict::read(&path).map_err(gate_error)?;
        if verdict.sha != plan.head {
            let reason = Reason::with_verdict(
                "verify",
                spec.stage,
                format!("the verdict is for {}, not {}", verdict.sha, plan.head),
                path,
            );
            reason.record(store, &task.id, &plan.label);
            return Err(Error::Refused(reason.text()));
        }
        if !verdict.is_pass() {
            return failed_stage(store, task, &plan.label, &plan.head, &verdict, path);
        }
    }

    let verdicts = task_verdicts(store, &task.id)?;
    let mut check_profile = plan.profile.clone();
    check_profile
        .stages
        .retain(|spec| spec.stage != Stage::Human);
    let check_extra: Vec<StageSpec> = extra_stages(task)
        .into_iter()
        .filter(|spec| spec.stage != Stage::Human)
        .collect();
    if !required_passed(&check_profile, &check_extra, &verdicts, &plan.head) {
        return Err(Error::Failed(format!(
            "the gate recorded no verdict for every required stage of {}",
            plan.head
        )));
    }
    let digest = verification_digest(&verdicts, &plan.head);
    store.change_task(&task.id, |task| {
        task.state = model::TaskState::Review;
        task.phase = "awaiting_human".to_owned();
        task.verified_sha = Some(plan.head.clone());
        task.verdict_set_digest = Some(digest.clone());
        task.retry_at = None;
        Ok(())
    })?;
    // Every required stage passed: this verification did not end in a conflict.
    reset_conflict_rework(store, &task.id)?;
    // A pass leaves no rework reason behind for `show` to print.
    clear_reason(store, &task.id, &plan.label);
    Ok(Verified {
        task: task.id.clone(),
        attempt_id: plan.label.clone(),
        state: "review",
        phase: "awaiting_human".to_owned(),
        reason: None,
        digest: Some(digest),
        reused: Vec::new(),
        conflict: None,
        review: None,
        paused: None,
    })
}

/// Applies the verdicts a background gate left for a `review.verifying` task: re-reads the
/// plan, then moves the task as [`finish_verification`] does. The state moves here, in a pass
/// that holds `runner.lock`, never in the gate process.
///
/// # Errors
///
/// A refusal (a moved branch, an unbuilt stage) leaves the task in `review.verifying`; a
/// missing verdict or a task file that changed outside arc is an error.
pub fn apply_verification(store: &Store, task_id: &str) -> Result<Verified, Error> {
    let plan = plan_verification(store, task_id)?;
    finish_verification(store, &plan)
}

/// `radian arc reverify <task>`: claim a new attempt without a worker and verify it at once
/// (ADR 0048 decisions 10 and 11).
///
/// The claim is refused (exit 3) when the task is not `review.rework_pending` or
/// `review.awaiting_human`, when its rework reason is a reject note or a delivery failure, or
/// when it has no `head_sha`. The verification then runs as it does after a pushed attempt:
/// the remote `work/<task>` must still be `head_sha`, and a moved branch leaves the task in
/// `review.verifying` with the reason recorded. The new attempt's verdicts are its own.
///
/// # Errors
///
/// The claim is refused, or the task file changed outside arc. A refused verification is in
/// [`Reverified::verification`], not in the outer result.
pub fn reverify(store: &Store, task_id: &str) -> Result<Reverified, Error> {
    reverify_with(store, task_id, None)
}

/// [`reverify`] with an optional review program (test-only; see [`ReviewRun::program`]).
///
/// # Errors
///
/// As [`reverify`].
pub fn reverify_with(
    store: &Store,
    task_id: &str,
    review_program: Option<&Path>,
) -> Result<Reverified, Error> {
    let claim = crate::store::attempt_ops::claim_reverify(store, task_id)?;
    let verification = verify_with(store, task_id, review_program);
    Ok(Reverified {
        attempt: claim.attempt,
        attempt_id: claim.attempt_id,
        verification,
    })
}

// ---- automatic reverify ----------------------------------------------------------------

/// What one scheduling pass's automatic reverify step did for one task (ADR 0048 decision 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoReverify {
    /// Nothing ran: the task is not one a moved main applies to, main has not moved, or this
    /// `(head, remote main)` pair was re-verified already.
    Not,
    /// A new worker-less attempt was claimed. The pass that claimed it starts its gate.
    Started {
        attempt_id: String,
        /// The remote main the pass saw when it claimed the attempt.
        main_sha: String,
    },
    /// Main moved, but the task used its `[limits] max_auto_reverify` automatic reverifies.
    /// `text` is what `run` prints after the task and the short sha, and what `show` puts in
    /// `rework_reason`.
    Limit { main_sha: String, text: String },
}

/// One `(head_sha, remote main)` pair an automatic reverify ran for. The pair is the first
/// guard against a loop: the same inputs are re-verified automatically at most once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AutoReverifyPair {
    head_sha: String,
    main_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    at: String,
}

/// The last `[limits] max_auto_reverify` stop: the inputs it was reported for, and its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AutoReverifyLimit {
    head_sha: String,
    main_sha: String,
    text: String,
    at: String,
}

/// `runs/<task>/auto_reverify.json` (ADR 0048 decision 9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct AutoReverifyRecord {
    /// The pairs already re-verified automatically, in order. Its length is the task's count.
    reverified: Vec<AutoReverifyPair>,
    /// The last count stop, so `show` can print it without the network.
    limit: Option<AutoReverifyLimit>,
}

/// The scheduling loop's automatic reverify (ADR 0048 decision 9): re-run the gate, without a
/// worker, for a task a `commands` or integration command failure blocked, once the remote
/// main is no longer the main that failing verdict recorded.
///
/// Two guards stop a loop: a `(head_sha, remote main)` pair is re-verified at most once (kept
/// in [`AUTO_REVERIFY_FILE`]), and `[limits] max_auto_reverify` counts the automatic reverifies
/// of one task; `0` turns the step off. The pair and the count are written before the claim, so
/// a pass that dies in between spends them rather than running the pair twice; a claim that is
/// refused gives them back.
///
/// A refused claim is the ordinary "nothing ran" result: the task moved since it was read.
///
/// # Errors
///
/// The remote main or a stored file could not be read, or the claim failed for a reason other
/// than the task having moved.
pub fn auto_reverify(store: &Store, task_id: &str) -> Result<AutoReverify, Error> {
    let Some((task, main_sha)) = auto_reverify_cause(store, task_id)? else {
        return Ok(AutoReverify::Not);
    };
    let head = task.head_sha.clone().unwrap_or_default();
    let mut record = read_auto_reverify(store, task_id);
    if record
        .reverified
        .iter()
        .any(|pair| pair.head_sha == head && pair.main_sha == main_sha)
    {
        return Ok(AutoReverify::Not);
    }
    let max = store.config()?.limits.max_auto_reverify;
    if max == 0 {
        return Ok(AutoReverify::Not);
    }
    if record.reverified.len() >= max as usize {
        if record
            .limit
            .as_ref()
            .is_some_and(|limit| limit.head_sha == head && limit.main_sha == main_sha)
        {
            // Reported once for these inputs: a later pass only waits for the lead.
            return Ok(AutoReverify::Not);
        }
        let text = format!(
            "the task used {} automatic reverifies and `[limits] max_auto_reverify` is {max}",
            record.reverified.len()
        );
        record.limit = Some(AutoReverifyLimit {
            head_sha: head,
            main_sha: main_sha.clone(),
            text: text.clone(),
            at: crate::store::plan_ops::utc_now(),
        });
        write_auto_reverify(store, task_id, &record)?;
        return Ok(AutoReverify::Limit { main_sha, text });
    }
    // The pair is spent before the claim: a pass that dies between the two leaves an input set
    // that does not run again, never one that runs twice.
    record.reverified.push(AutoReverifyPair {
        head_sha: head.clone(),
        main_sha: main_sha.clone(),
        attempt_id: None,
        at: crate::store::plan_ops::utc_now(),
    });
    write_auto_reverify(store, task_id, &record)?;
    match crate::store::attempt_ops::claim_reverify_by(
        store,
        task_id,
        crate::store::attempt_ops::ReverifyBy::Automatic,
    ) {
        Ok(claim) => {
            let mut record = read_auto_reverify(store, task_id);
            if let Some(pair) = record.reverified.iter_mut().find(|pair| {
                pair.head_sha == head && pair.main_sha == main_sha && pair.attempt_id.is_none()
            }) {
                pair.attempt_id = Some(claim.attempt_id.clone());
            }
            write_auto_reverify(store, task_id, &record)?;
            Ok(AutoReverify::Started {
                attempt_id: claim.attempt_id,
                main_sha,
            })
        }
        Err(error) => {
            // The claim changed nothing, so the count is not spent.
            let mut record = read_auto_reverify(store, task_id);
            record.reverified.retain(|pair| {
                !(pair.head_sha == head && pair.main_sha == main_sha && pair.attempt_id.is_none())
            });
            write_auto_reverify(store, task_id, &record)?;
            match error {
                Error::Refused(_) => Ok(AutoReverify::Not),
                other => Err(other),
            }
        }
    }
}

/// The task and the remote main, when an automatic reverify applies to `task_id` (ADR 0048
/// decision 9): the plan's `auto` is not `off`, the task is in `review.rework_pending` for a
/// `commands` or integration command failure or timeout, and the remote main is no longer the
/// main that failing verdict recorded.
fn auto_reverify_cause(
    store: &Store,
    task_id: &str,
) -> Result<Option<(model::TaskFile, String)>, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Ok(None);
    }
    let task = loaded.item;
    if task.state != model::TaskState::Review || task.phase != "rework_pending" {
        return Ok(None);
    }
    let plan = store.load_plan(&task.plan)?;
    if plan.changed_outside_arc || plan.item.auto == "off" {
        return Ok(None);
    }
    let Some(recorded_main) = recorded_failure_main(store, &task)? else {
        return Ok(None);
    };
    let (remote, main_branch) = store.repository_names()?;
    let Some(main_sha) = ls_remote_branch(store.repo(), &remote, &main_branch)? else {
        return Ok(None);
    };
    if main_sha == recorded_main {
        return Ok(None);
    }
    Ok(Some((task, main_sha)))
}

/// The main sha the failing verdict of `task`'s current attempt recorded, when the failure is
/// one a moved main can fix (ADR 0048 decision 9): a `commands` or integration stage that
/// failed with `command_failed` or `timeout`. `None` for every other reason, and for a reason
/// no verdict records (a moved branch, a reject note, a delivery failure).
fn recorded_failure_main(store: &Store, task: &model::TaskFile) -> Result<Option<String>, Error> {
    let label = attempt_label(task)?;
    let dir = attempt_dir(store.repo(), &task.id, &label).map_err(gate_error)?;
    let Ok(text) = fs::read_to_string(dir.join(REVIEW_FILE)) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    let stage = value
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let fixable = [Stage::Commands.as_str(), Stage::Integration.as_str()].contains(&stage)
        && AUTO_REVERIFY_REASONS.contains(&reason);
    if !fixable {
        return Ok(None);
    }
    let Some(verdict_file) = value.get("verdict").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Ok(verdict) = Verdict::read(Path::new(verdict_file)) else {
        return Ok(None);
    };
    if verdict.is_pass() || verdict.stage.as_str() != stage {
        return Ok(None);
    }
    if Some(verdict.sha.as_str()) != task.head_sha.as_deref() {
        return Ok(None);
    }
    // A verification verdict records `main_sha`; an integration verdict records the main it
    // merged onto as `base_sha` (decision 10).
    let main = verdict
        .main_sha
        .as_deref()
        .filter(|sha| !sha.is_empty())
        .or_else(|| verdict.base_sha.as_deref().filter(|sha| !sha.is_empty()));
    Ok(main.map(str::to_owned))
}

/// `runs/<task>/auto_reverify.json`.
fn auto_reverify_path(store: &Store, task: &str) -> PathBuf {
    store
        .root()
        .join("runs")
        .join(task)
        .join(AUTO_REVERIFY_FILE)
}

fn read_auto_reverify(store: &Store, task: &str) -> AutoReverifyRecord {
    fs::read_to_string(auto_reverify_path(store, task))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_auto_reverify(
    store: &Store,
    task: &str,
    record: &AutoReverifyRecord,
) -> Result<(), Error> {
    let path = auto_reverify_path(store, task);
    let dir = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let text = serde_json::to_string_pretty(record)
        .map_err(|error| Error::Failed(format!("cannot encode {}: {error}", path.display())))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", temporary.display())))?;
    fs::rename(&temporary, &path)
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))
}

fn failed_stage(
    store: &Store,
    task: &model::TaskFile,
    label: &str,
    head: &str,
    verdict: &Verdict,
    path: PathBuf,
) -> Result<Verified, Error> {
    // A conflict with main starts the next attempt mid-merge, so arc writes its brief itself
    // when the profile runs `merge-main` before an attempt (ADR 0048 decision 9).
    let conflict = conflict_auto_rework(store, task, verdict)?;
    let mut text = failure_text(verdict);
    if let Some(stop) = conflict
        .as_ref()
        .and_then(|conflict| conflict.stop.as_ref())
    {
        // The stop is part of the recorded reason, so `show` prints why the task waits.
        text = format!("{text}; {stop}");
    }
    let reason = Reason::with_verdict("verify", verdict.stage, text, path.clone());
    reason.record(store, &task.id, label);
    if let Some(conflict) = conflict {
        let stopped = conflict.stop.is_some();
        return Ok(Verified {
            task: task.id.clone(),
            attempt_id: label.to_owned(),
            state: if stopped { "review" } else { "todo" },
            phase: if stopped { "rework_pending" } else { "ready" }.to_owned(),
            reason: Some(reason),
            digest: None,
            reused: Vec::new(),
            conflict: Some(conflict),
            review: None,
            paused: None,
        });
    }
    // This verification did not end in a conflict, so the task's run of conflicts is over.
    reset_conflict_rework(store, &task.id)?;
    // A failed review writes its own rework when the plan is not at `auto` `off` (ADR 0048
    // decision 9). The pause is the same limit the other automatic reworks use.
    if let Some((review, paused)) = review_auto_rework(store, task, verdict, &path)? {
        let paused_here = paused.is_some();
        // The rework path below records the sha it failed on and drops the digest; the task
        // keeps that record whether it returns to `todo.ready` or pauses here.
        store.change_task(&task.id, |task| {
            task.verified_sha = Some(head.to_owned());
            task.verdict_set_digest = None;
            task.retry_at = None;
            Ok(())
        })?;
        return Ok(Verified {
            task: task.id.clone(),
            attempt_id: label.to_owned(),
            state: if paused_here { "review" } else { "todo" },
            phase: if paused_here {
                "rework_pending"
            } else {
                "ready"
            }
            .to_owned(),
            reason: Some(reason),
            digest: None,
            reused: Vec::new(),
            conflict: None,
            review: Some(review),
            paused,
        });
    }
    store.change_task(&task.id, |task| {
        task.state = model::TaskState::Review;
        task.phase = "rework_pending".to_owned();
        task.verified_sha = Some(head.to_owned());
        task.verdict_set_digest = None;
        task.retry_at = None;
        Ok(())
    })?;
    Ok(Verified {
        task: task.id.clone(),
        attempt_id: label.to_owned(),
        state: "review",
        phase: "rework_pending".to_owned(),
        reason: Some(reason),
        digest: None,
        reused: Vec::new(),
        conflict: None,
        review: None,
        paused: None,
    })
}

/// The conflict rework arc writes itself (ADR 0048 decision 9): when the profile runs
/// `merge-main` before an attempt, a stage that failed with reason `conflict` returns the task
/// to `todo.ready` with the next attempt's brief instead of waiting for the lead.
///
/// `None` when the verdict is not a conflict or the profile does not run `merge-main`; the
/// caller then keeps the path to `review.rework_pending`.
///
/// `[limits] max_conflict_rework` bounds the conflict reworks arc writes in a row: while the
/// count is below the limit, arc writes the brief and raises the count; when it has reached
/// the limit, no brief is written and the task stays in `review.rework_pending` with the
/// reason in [`Conflict::stop`]. `0` writes none. The lead's own rework and an attempt that
/// does not conflict set the count back to `0` ([`reset_conflict_rework`]).
fn conflict_auto_rework(
    store: &Store,
    task: &model::TaskFile,
    verdict: &Verdict,
) -> Result<Option<Conflict>, Error> {
    if verdict.reason.as_deref() != Some("conflict") {
        return Ok(None);
    }
    let config = store.config()?;
    let Some(profile) = config.gate.selected_profile() else {
        return Ok(None);
    };
    if !profile.runs_merge_main() {
        return Ok(None);
    }
    // A verification verdict records `main_sha`; an integration verdict records the main it
    // merged onto as `base_sha`.
    let main_sha = verdict
        .main_sha
        .as_deref()
        .filter(|sha| !sha.is_empty())
        .or_else(|| verdict.base_sha.as_deref().filter(|sha| !sha.is_empty()))
        .unwrap_or_default()
        .to_owned();
    let limit = config.limits.max_conflict_rework;
    let mut record = read_conflict_rework(store, &task.id);
    let conflict = Conflict {
        main_sha,
        stage: verdict.stage,
        paths: verdict.conflict_paths.clone(),
        stop: None,
    };
    if record.reworks >= limit {
        let conflict = Conflict {
            stop: Some(format!(
                "the task wrote {} conflict reworks in a row and `[limits] \
                 max_conflict_rework` is {limit}; it waits for a rework from the lead",
                record.reworks
            )),
            ..conflict
        };
        store.change_task(&task.id, |task| {
            if task.state == model::TaskState::Review {
                task.phase = "rework_pending".to_owned();
                task.retry_at = None;
            }
            Ok(())
        })?;
        return Ok(Some(conflict));
    }
    record.reworks = record.reworks.saturating_add(1);
    write_conflict_rework(store, &task.id, &record)?;
    let brief = conflict_rework_brief(&conflict);
    crate::store::plan_ops::add_rework_with(
        store,
        &task.id,
        &brief,
        crate::store::plan_ops::CONFLICT_REWORK_SUFFIX,
    )?;
    Ok(Some(conflict))
}

/// `runs/<task>/conflict_rework.json` (ADR 0048 decision 9): how many conflict reworks arc
/// has written for the task in a row. It is not part of the task file: it is bookkeeping for
/// one limit, like [`AUTO_REVERIFY_FILE`].
pub const CONFLICT_REWORK_FILE: &str = "conflict_rework.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct ConflictReworkRecord {
    /// How many conflict reworks arc has written for the task in a row. The lead's rework and
    /// an attempt that does not conflict set it back to 0.
    reworks: u32,
}

fn conflict_rework_path(store: &Store, task: &str) -> PathBuf {
    store
        .root()
        .join("runs")
        .join(task)
        .join(CONFLICT_REWORK_FILE)
}

fn read_conflict_rework(store: &Store, task: &str) -> ConflictReworkRecord {
    fs::read_to_string(conflict_rework_path(store, task))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_conflict_rework(
    store: &Store,
    task: &str,
    record: &ConflictReworkRecord,
) -> Result<(), Error> {
    let path = conflict_rework_path(store, task);
    let dir = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let text = serde_json::to_string_pretty(record)
        .map_err(|error| Error::Failed(format!("cannot encode {}: {error}", path.display())))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", temporary.display())))?;
    fs::rename(&temporary, &path)
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))
}

/// A conflict rework arc wrote in a row is over: the lead's own rework, a verification that
/// did not end in a conflict, and an attempt that never reached the gate all set the count back
/// to 0 (ADR 0048 decision 9). Only a run of conflict reworks can reach
/// `[limits] max_conflict_rework`; [`crate::store::attempt_ops::apply`] calls this for an
/// attempt that ends without reaching the gate.
pub(crate) fn reset_conflict_rework(store: &Store, task: &str) -> Result<(), Error> {
    if read_conflict_rework(store, task).reworks == 0 {
        return Ok(());
    }
    write_conflict_rework(store, task, &ConflictReworkRecord::default())
}

/// The fixed, short brief of a conflict rework (ADR 0048 decision 9): the main it conflicted
/// with, the stage, the paths, and what the next attempt does first.
fn conflict_rework_brief(conflict: &Conflict) -> String {
    let short = &conflict.main_sha[..conflict.main_sha.len().min(7)];
    let paths = if conflict.paths.is_empty() {
        "- none listed in the verdict".to_owned()
    } else {
        conflict
            .paths
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "The change conflicted with main at `{short}` in `{}`.\n\n\
         Conflicted paths:\n\n{paths}\n\n\
         The next attempt starts mid-merge. Resolve the conflict keeping both sides' intent, \
         commit the merge (`git commit --no-edit`), rebuild, run the touched crates' tests, and \
         change nothing else.",
        conflict.stage
    )
}

/// The review rework arc writes itself (ADR 0048 decision 9): a `review` stage that failed with
/// blocking items and a plan whose `auto` is not `off` returns the task to `todo.ready` with
/// the next attempt's brief, or pauses the plan at `max_auto_rework` and leaves the task in
/// `review.rework_pending`.
///
/// `None` when the verdict is not a review failure with blocking items, or the plan is at
/// `auto` `off`: the caller then keeps the path to `review.rework_pending`.
fn review_auto_rework(
    store: &Store,
    task: &model::TaskFile,
    verdict: &Verdict,
    path: &Path,
) -> Result<Option<(ReviewRework, Option<Paused>)>, Error> {
    if verdict.stage != Stage::Review || verdict.reason.as_deref() != Some("review_blocking") {
        return Ok(None);
    }
    let Some(review) = verdict.review.as_ref() else {
        return Ok(None);
    };
    if review.blocking.is_empty() {
        return Ok(None);
    }
    let plan = store.load_plan(&task.plan)?;
    if plan.changed_outside_arc || plan.item.auto == "off" {
        return Ok(None);
    }
    // The count lives in `review.rework_pending`, where the pause rule and `show` read it
    // (decision 12).
    to_rework(store, &task.id)?;
    let paused = crate::store::attempt_ops::record_auto_rework(store, &task.id)?;
    let rework = ReviewRework {
        verdict: path.to_path_buf(),
        items: review.blocking.clone(),
    };
    if paused.is_some() {
        return Ok(Some((rework, paused)));
    }
    let brief = review_rework_brief(&rework);
    crate::store::plan_ops::add_rework_with(
        store,
        &task.id,
        &brief,
        crate::store::plan_ops::REVIEW_REWORK_SUFFIX,
    )?;
    Ok(Some((rework, None)))
}

/// The fixed brief of a review rework (ADR 0048 decision 9): what to fix, the blocking items as
/// `[level] path:line text`, and the verdict to read for detail.
fn review_rework_brief(rework: &ReviewRework) -> String {
    let items = rework
        .items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The review found these items. Fix them and nothing else, append commits to the \
         branch, and do not rebase.\n\n{items}\n\nRead the verdict for detail: `{}`",
        rework.verdict.display()
    )
}

// ---- the review's follow-up task (ADR 0048 decision 10) -------------------------------

/// The review's follow-up task of one merged task (ADR 0048 decision 10): the items the
/// task's last review suggested become one `todo.proposed` task of the same plan, `after` the
/// merged task. Nothing from `blocking` is carried: the review passed for the merge only
/// because none was left.
///
/// `None` when `[gate.review] propose_followups` is `false`, when the task has no review
/// verdict, when the review suggested nothing, or when the plan already holds this task's
/// follow-up: a task gets at most one.
fn propose_followup(store: &Store, task: &model::TaskFile) -> Result<Option<Followup>, Error> {
    if !store.config()?.gate.review.propose_followups {
        return Ok(None);
    }
    let Some((verdict_path, verdict)) = latest_review_verdict(store, &task.id) else {
        return Ok(None);
    };
    let items = verdict
        .review
        .as_ref()
        .map(|review| review.suggestions.clone())
        .unwrap_or_default();
    if items.is_empty() {
        return Ok(None);
    }
    let plan = store.load_plan(&task.plan)?;
    if plan.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task.plan.clone()));
    }
    if has_followup(store, &plan.item, &task.id)? {
        return Ok(None);
    }
    let id = store.add_task_proposed(
        &task.plan,
        &followup_slug(&task.id),
        &followup_title(&task.id),
        vec![task.id.clone()],
        false,
        Vec::new(),
        &followup_brief(&task.id, &items, &verdict_path),
    )?;
    Ok(Some(Followup {
        task: id,
        from: task.id.clone(),
    }))
}

/// Whether the plan already holds `task`'s follow-up: a task whose title and `after` are the
/// ones arc gives it. A task gets at most one, even when a second merge path reaches the same
/// merge.
fn has_followup(store: &Store, plan: &model::PlanFile, task: &str) -> Result<bool, Error> {
    for id in &plan.tasks {
        let Ok(loaded) = store.load_task(id) else {
            continue;
        };
        if loaded.item.title == followup_title(task) && loaded.item.after.as_slice() == [task] {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The slug of `task`'s follow-up: `<merged task slug>-followups`, cut to the 48-character
/// slug limit, with a trailing `-` removed so the cut stays a valid slug.
fn followup_slug(task: &str) -> String {
    let slug = model::split_id(task)
        .map(|(_, _, slug)| slug)
        .unwrap_or(task);
    let mut text: String = format!("{slug}-followups")
        .chars()
        .take(model::SLUG_MAX_CHARS)
        .collect();
    while text.ends_with('-') {
        text.pop();
    }
    text
}

/// The title arc gives `task`'s follow-up.
fn followup_title(task: &str) -> String {
    format!("Follow-ups from {task}'s review")
}

/// The fixed brief of `task`'s follow-up (ADR 0048 decision 10): the line that says where the
/// items come from and what to do with each, the items as `[level] path:line text`, and the
/// review verdict to read for detail.
fn followup_brief(task: &str, items: &[ReviewItem], verdict: &Path) -> String {
    let items = items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The items below are from the review of `{task}`, which is now merged; fix each one, \
         or leave it with a one-line reason in the final answer.\n\n{items}\n\n\
         Read the verdict for detail: `{}`",
        verdict.display()
    )
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
///
/// This is the synchronous path: approval, integration and push in one call. The scheduling
/// loop uses `merge_step`, which starts the integration in the background.
pub fn merge_as(store: &Store, task_id: &str, approver: Option<&str>) -> Result<Merged, Error> {
    let context = prepare_merge(store, task_id, approver)?;
    let mut last = String::new();
    for round in 1..=MAX_INTEGRATION_ROUNDS {
        let integration_label = next_integration_label(store, task_id, &context.label)?;
        let verdict = run_integration(store, &context, &integration_label)?;
        if !verdict.is_pass() {
            return integration_failed(store, &context, &integration_label, &verdict, round);
        }
        match finish_integration(store, &context, &verdict, round)? {
            Integration::Merged(merged) => return Ok(*merged),
            Integration::Again(reason) => last = reason,
        }
    }

    back_to_integrating(store, task_id)?;
    Err(Error::Refused(format!(
        "{last}; {MAX_INTEGRATION_ROUNDS} integration runs did not settle, so the task stays \
         in `review.integrating`"
    )))
}

/// What one merge's preconditions and approval need (ADR 0048 decisions 6 and 11).
pub struct MergeContext {
    pub task_id: String,
    /// The attempt id that names the task's verdict directory.
    pub label: String,
    pub head: String,
    pub base: String,
    pub remote: String,
    pub main_branch: String,
    pub main_ref: String,
    pub profile: crate::gate::Profile,
    /// The phase after this call: `integrating`, or `ready_to_merge` on a resume.
    pub phase: String,
}

/// Checks the approval's preconditions, writes the approval when the task is
/// `review.awaiting_human`, and moves it to `review.integrating`. It starts nothing.
///
/// # Errors
///
/// The state does not allow the merge, the branch moved, the digest changed, or every
/// required stage did not pass.
fn prepare_merge(
    store: &Store,
    task_id: &str,
    approver: Option<&str>,
) -> Result<MergeContext, Error> {
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
    Ok(MergeContext {
        task_id: task_id.to_owned(),
        label,
        head,
        base,
        remote,
        main_branch,
        main_ref,
        profile,
        phase: if task.phase == "awaiting_human" {
            "integrating".to_owned()
        } else {
            task.phase
        },
    })
}

/// The integration stage for one context and label.
fn run_integration(store: &Store, context: &MergeContext, label: &str) -> Result<Verdict, Error> {
    let config = store.config()?;
    IntegrationRun {
        repository: store.repo(),
        remote: &context.remote,
        task: &context.task_id,
        attempt: label,
        head_sha: &context.head,
        base_sha: &context.base,
        main_ref: &context.main_ref,
        key_commands: &context.profile.key_commands,
        timeout: config.gate.command_timeout(),
    }
    .run()
    .map_err(gate_error)
}

/// One integration for a background gate: check that the task is `review.integrating`, run the
/// stage for the next free integration label, and return that label. The state is the pass's
/// to move.
///
/// # Errors
///
/// The task is not `review.integrating`, has no shas, or the stage could not run.
pub fn gate_integration(store: &Store, task_id: &str) -> Result<String, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    if task.state != model::TaskState::Review || task.phase != "integrating" {
        return Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; only a `review.integrating` task integrates",
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
    let base = task
        .base_sha
        .clone()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| Error::Refused(format!("task `{task_id}` has no `base_sha`")))?;
    let config = store.config()?;
    let (remote, main_branch) = store.repository_names()?;
    let main_ref = format!("refs/remotes/{remote}/{main_branch}");
    let profile = config.gate.selected_profile().cloned().ok_or_else(|| {
        Error::Failed(format!(
            "gate.profile is `{}` but no such profile exists",
            config.gate.profile
        ))
    })?;
    let integration_label = next_integration_label(store, task_id, &label)?;
    IntegrationRun {
        repository: store.repo(),
        remote: &remote,
        task: task_id,
        attempt: &integration_label,
        head_sha: &head,
        base_sha: &base,
        main_ref: &main_ref,
        key_commands: &profile.key_commands,
        timeout: config.gate.command_timeout(),
    }
    .run()
    .map_err(gate_error)?;
    Ok(integration_label)
}

/// What a passing integration did.
enum Integration {
    /// Boxed: the result is much larger than the other variant.
    Merged(Box<Merged>),
    /// Main moved or the push was rejected: the task is back in `review.integrating` and
    /// needs another round.
    Again(String),
}

/// Finishes a passing integration verdict: `review.ready_to_merge`, the fast-forward push to
/// the remote main, `done.merged`, and the removal of the worktree (ADR 0048 decisions 6 and
/// 11).
fn finish_integration(
    store: &Store,
    context: &MergeContext,
    verdict: &Verdict,
    round: u32,
) -> Result<Integration, Error> {
    let task_id = context.task_id.as_str();
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
    if !merge_ready(store.repo(), &context.remote, &context.main_ref, verdict)
        .map_err(gate_error)?
    {
        back_to_integrating(store, task_id)?;
        return Ok(Integration::Again(
            "main moved after integration".to_owned(),
        ));
    }
    match push_candidate(
        store.repo(),
        &context.remote,
        &candidate,
        &context.main_branch,
    )? {
        Push::Pushed => {}
        Push::Rejected(stderr) => {
            back_to_integrating(store, task_id)?;
            return Ok(Integration::Again(format!(
                "the push to `{}` `{}` was rejected: {stderr}",
                context.remote, context.main_branch
            )));
        }
    }
    let remote_main = ls_remote_branch(store.repo(), &context.remote, &context.main_branch)?;
    if remote_main.as_deref() != Some(candidate.as_str()) {
        back_to_integrating(store, task_id)?;
        return Ok(Integration::Again(format!(
            "`{}` `{}` is {}, not {candidate}",
            context.remote,
            context.main_branch,
            remote_main.as_deref().unwrap_or("missing")
        )));
    }
    if !is_ancestor(store.repo(), &context.head, &candidate)? {
        return Err(Error::Failed(format!(
            "{candidate} does not contain the verified {}",
            context.head
        )));
    }
    store.change_task(task_id, |task| {
        task.state = model::TaskState::Done;
        task.phase = "merged".to_owned();
        task.retry_at = None;
        Ok(())
    })?;
    let mut warning = remove_worktree(store, task_id).err();
    // The merge stands whatever the follow-up does: a plan that changed outside arc leaves a
    // warning, not a failed merge.
    let followup = match store.load_task(task_id) {
        Ok(merged) => match propose_followup(store, &merged.item) {
            Ok(followup) => followup,
            Err(error) => {
                let failure = format!("cannot propose the follow-up task: {}", error.message());
                warning = Some(match warning {
                    Some(warning) => format!("{warning}; {failure}"),
                    None => failure,
                });
                None
            }
        },
        Err(error) => {
            let failure = format!("cannot read the merged task: {}", error.message());
            warning = Some(match warning {
                Some(warning) => format!("{warning}; {failure}"),
                None => failure,
            });
            None
        }
    };
    Ok(Integration::Merged(Box::new(Merged {
        task: task_id.to_owned(),
        state: "done",
        phase: "merged".to_owned(),
        reason: None,
        candidate_sha: Some(candidate),
        rounds: round,
        warning,
        conflict: None,
        followup,
    })))
}

/// A failed integration verdict: record the reason and send the task to
/// `review.rework_pending`. It is a normal result, not an error.
fn integration_failed(
    store: &Store,
    context: &MergeContext,
    integration_label: &str,
    verdict: &Verdict,
    round: u32,
) -> Result<Merged, Error> {
    // The same conflict path as a failed verification stage (ADR 0048 decision 9).
    let task = store.load_task(&context.task_id)?.item;
    let conflict = conflict_auto_rework(store, &task, verdict)?;
    let mut text = failure_text(verdict);
    if let Some(stop) = conflict
        .as_ref()
        .and_then(|conflict| conflict.stop.as_ref())
    {
        text = format!("{text}; {stop}");
    }
    let reason = Reason::with_verdict(
        "merge",
        verdict.stage,
        text,
        integration_verdict(store, &context.task_id, integration_label)?,
    );
    reason.record(store, &context.task_id, &context.label);
    if let Some(conflict) = conflict {
        let stopped = conflict.stop.is_some();
        return Ok(Merged {
            task: context.task_id.clone(),
            state: if stopped { "review" } else { "todo" },
            phase: if stopped { "rework_pending" } else { "ready" }.to_owned(),
            reason: Some(reason.text()),
            candidate_sha: None,
            rounds: round,
            warning: None,
            conflict: Some(conflict),
            followup: None,
        });
    }
    // This integration did not end in a conflict, so the task's run of conflicts is over.
    reset_conflict_rework(store, &context.task_id)?;
    to_rework(store, &context.task_id)?;
    Ok(Merged {
        task: context.task_id.clone(),
        state: "review",
        phase: "rework_pending".to_owned(),
        reason: Some(reason.text()),
        candidate_sha: None,
        rounds: round,
        warning: None,
        conflict: None,
        followup: None,
    })
}

/// What one pass's merge step for one task did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergeStep {
    /// The transition line, when the step moved the task.
    pub line: Option<String>,
    /// A reason the step could not move the task; the summary prints it.
    pub note: Option<String>,
}

impl MergeStep {
    fn quiet() -> Self {
        Self::default()
    }

    fn line(line: String) -> Self {
        Self {
            line: Some(line),
            note: None,
        }
    }

    fn note(note: String) -> Self {
        Self {
            line: None,
            note: Some(note),
        }
    }

    /// A merged task: its line (and the follow-up task's, when arc proposed one), with the
    /// warning a cleanup step left after the merge as the summary's note.
    fn merged(merged: Merged) -> Self {
        Self {
            line: Some(merged.line()),
            note: merged.warning,
        }
    }
}

/// One pass's merge step for a task of a plan at auto `merge` (ADR 0048 decisions 10, 12 and
/// 13): take the approval, start the integration gate in the background, or finish the
/// integration the gate left. It never waits for the gate.
pub fn merge_step(store: &Store, task_id: &str, stop: bool) -> Result<MergeStep, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Ok(MergeStep::note(format!(
            "{task_id} changed outside arc; run `radian arc adopt {task_id}`"
        )));
    }
    let task = loaded.item;
    let phase = task.phase.clone();
    if task.state != model::TaskState::Review
        || !matches!(
            phase.as_str(),
            "awaiting_human" | "integrating" | "ready_to_merge"
        )
    {
        return Ok(MergeStep::quiet());
    }
    if phase == "awaiting_human" {
        let context = prepare_merge(store, task_id, Some("auto"))?;
        return Ok(MergeStep::line(format!(
            "{} approved -> review.integrating ({})",
            context.task_id, context.label
        )));
    }
    if phase == "integrating" {
        // A gate record of another kind belongs to verification.
        let record = crate::gate::background::read(store, task_id)
            .filter(|record| record.kind == "integration");
        // How many deaths this attempt and these inputs already have (ADR 0048 decision 10).
        let mut deaths = 0;
        if let Some(record) = &record {
            match record.status.as_str() {
                // A gate that wrote `running` and is gone died; arc's own stop (`interrupted`)
                // is restarted below without counting.
                crate::gate::background::RUNNING => {
                    if !crate::gate::background::running(store, task_id) {
                        let inputs = integration_inputs(store, task_id);
                        match crate::gate::background::count_death(
                            store,
                            task_id,
                            "integration",
                            &attempt_label(&task)?,
                            record,
                            inputs.as_deref(),
                        )? {
                            crate::gate::background::Death::Stop => {
                                return Ok(MergeStep::note(format!(
                                    "{task_id} integration cannot run: {}",
                                    crate::gate::background::DIED_TEXT
                                )));
                            }
                            crate::gate::background::Death::Count(count) => deaths = count,
                            crate::gate::background::Death::Restart => {}
                        }
                    }
                }
                // The pass wrote the inputs it started the integration with and the gate
                // wrote nothing: a new start with the same inputs would only repeat it.
                crate::gate::background::STARTED => {
                    if !crate::gate::background::running(store, task_id)
                        && record.attempt == attempt_label(&task)?
                        && let Some(inputs) = record.inputs.clone()
                    {
                        crate::gate::background::write_no_record(
                            store,
                            task_id,
                            "integration",
                            &record.attempt,
                            &inputs,
                        )?;
                        return Ok(MergeStep::note(format!(
                            "{task_id} integration cannot run: {}",
                            crate::gate::background::NO_RECORD_TEXT
                        )));
                    }
                }
                crate::gate::background::NO_RECORD | crate::gate::background::DIED => {
                    let current = integration_inputs(store, task_id);
                    let reported = format!(
                        "{task_id} integration cannot run: {}",
                        record.text.clone().unwrap_or_else(|| {
                            crate::gate::background::NO_RECORD_TEXT.to_owned()
                        })
                    );
                    if record.attempt == attempt_label(&task)?
                        && current.as_deref() == record.inputs.as_deref()
                    {
                        return Ok(MergeStep::note(reported));
                    }
                    // The inputs changed (a new head, a new main, a changed profile), or the
                    // record belongs to an earlier attempt: start one new gate below.
                }
                // arc stopped the gate itself: start it again at once, with no death counted.
                crate::gate::background::INTERRUPTED => {}
                _ => {
                    crate::gate::background::clear(store, task_id);
                    if record.status == "error" {
                        return Err(Error::Failed(
                            record
                                .text
                                .clone()
                                .unwrap_or_else(|| "the integration gate failed".to_owned()),
                        ));
                    }
                    if record.status == "refused" {
                        return Ok(MergeStep::note(format!(
                            "{task_id} integration cannot run: {}",
                            record.text.clone().unwrap_or_default()
                        )));
                    }
                    let path =
                        verdict_path(store.repo(), task_id, &record.attempt, Stage::Integration)
                            .map_err(gate_error)?;
                    let verdict = Verdict::read(&path).map_err(gate_error)?;
                    let context = prepare_merge(store, task_id, Some("auto"))?;
                    if !verdict.is_pass() {
                        let merged =
                            integration_failed(store, &context, &record.attempt, &verdict, 1)?;
                        return Ok(MergeStep::line(merged.line()));
                    }
                    return match finish_integration(store, &context, &verdict, 1)? {
                        Integration::Merged(merged) => Ok(MergeStep::merged(*merged)),
                        Integration::Again(reason) => Ok(MergeStep::line(format!(
                            "{task_id} integration -> review.integrating ({reason})"
                        ))),
                    };
                }
            }
        }
        // A gate that is still running is the reason this pass starts none.
        if crate::gate::background::running(store, task_id) {
            return Ok(MergeStep::quiet());
        }
        if stop {
            return Ok(MergeStep::quiet());
        }
        return match crate::gate::background::start(
            store,
            task_id,
            &crate::gate::background::Start {
                kind: "integration",
                attempt: attempt_label(&task)?,
                inputs: integration_inputs(store, task_id),
                deaths,
            },
        )? {
            Some(pid) => Ok(MergeStep::line(format!(
                "{task_id} integration started (pid {pid})"
            ))),
            None => Ok(MergeStep::quiet()),
        };
    }
    // `review.ready_to_merge`: finish the last passing integration verdict, or integrate
    // again when main moved under it.
    let context = prepare_merge(store, task_id, Some("auto"))?;
    match latest_integration_verdict(store, task_id, &context.head) {
        Some((_, verdict)) if verdict.is_pass() => {
            match finish_integration(store, &context, &verdict, 1)? {
                Integration::Merged(merged) => Ok(MergeStep::merged(*merged)),
                Integration::Again(reason) => Ok(MergeStep::line(format!(
                    "{task_id} integration -> review.integrating ({reason})"
                ))),
            }
        }
        _ => {
            back_to_integrating(store, task_id)?;
            Ok(MergeStep::line(format!(
                "{task_id} review.ready_to_merge -> review.integrating (no usable integration verdict)"
            )))
        }
    }
}

/// The newest integration verdict for `head`, with its path.
fn latest_integration_verdict(store: &Store, task: &str, head: &str) -> Option<(PathBuf, Verdict)> {
    task_verdict_files(store, task)
        .ok()?
        .into_iter()
        .rfind(|(_, verdict)| verdict.stage == Stage::Integration && verdict.sha == head)
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
/// A `review_blocking` failure lists its blocking items, because the rework reason is how the
/// lead learns what to fix.
fn failure_text(verdict: &Verdict) -> String {
    let reason = verdict
        .reason
        .clone()
        .unwrap_or_else(|| "failed".to_owned());
    if reason == "conflict" && !verdict.conflict_paths.is_empty() {
        return format!("conflict: {}", verdict.conflict_paths.join(", "));
    }
    if reason == "review_blocking"
        && let Some(review) = &verdict.review
        && !review.blocking.is_empty()
    {
        return format!(
            "review_blocking: {}",
            review
                .blocking
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    reason
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
pub(crate) fn next_integration_label(
    store: &Store,
    task: &str,
    base: &str,
) -> Result<String, Error> {
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
    // A gate that exited without a record, or one that died once too often after it wrote
    // `running`: the pass recorded the fact in the gate record, and `show` reports the same
    // text `radian arc run` prints (ADR 0048 decision 10).
    if parts.is_empty()
        && let Some(record) = crate::gate::background::read(store, &task.id)
        && record.kind == "verify"
        && matches!(
            record.status.as_str(),
            crate::gate::background::NO_RECORD | crate::gate::background::DIED
        )
        && record.attempt == label
    {
        parts.push(
            record
                .text
                .clone()
                .unwrap_or_else(|| crate::gate::background::NO_RECORD_TEXT.to_owned()),
        );
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
    // The automatic reverify count: the task stays in `rework_pending` and `show` says why
    // (ADR 0048 decision 9). The text belongs to the attempt that hit the limit.
    if task.phase == "rework_pending" {
        let record = read_auto_reverify(store, &task.id);
        if let Some(limit) = record.limit
            && Some(limit.head_sha.as_str()) == task.head_sha.as_deref()
        {
            parts.push(limit.text);
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn clear_reason(store: &Store, task: &str, label: &str) {
    let Ok(dir) = attempt_dir(store.repo(), task, label) else {
        return;
    };
    let _ = fs::remove_file(dir.join(REVIEW_FILE));
}

/// Every verdict file of every attempt of `task`, in attempt order, with its path.
fn task_verdict_files(store: &Store, task: &str) -> Result<Vec<(PathBuf, Verdict)>, Error> {
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
            verdicts.push((path.clone(), Verdict::read(&path).map_err(gate_error)?));
        }
    }
    Ok(verdicts)
}

/// Every verdict of every attempt of `task`, in attempt order.
fn task_verdicts(store: &Store, task: &str) -> Result<Vec<Verdict>, Error> {
    Ok(task_verdict_files(store, task)?
        .into_iter()
        .map(|(_, verdict)| verdict)
        .collect())
}

/// The task's newest `review` verdict and its path, across every attempt: what `show`
/// reports as the review's summary and findings. A later review (a rework, a `reverify`)
/// replaces an earlier one.
#[must_use]
pub fn latest_review_verdict(store: &Store, task: &str) -> Option<(PathBuf, Verdict)> {
    task_verdict_files(store, task)
        .ok()?
        .into_iter()
        .rfind(|(_, verdict)| verdict.stage == Stage::Review)
}

/// The task's newest `review` verdict from an attempt other than `label`, with its path: the
/// candidate previous review of a rework round (ADR 0048 decision 10). A verdict with an
/// empty `sha` is not one, and the stage ignores it when its `sha` is the commit under review.
#[must_use]
pub fn previous_review(store: &Store, task: &str, label: &str) -> Option<(PathBuf, Verdict)> {
    let current = verdict_path(store.repo(), task, label, Stage::Review).ok();
    task_verdict_files(store, task)
        .ok()?
        .into_iter()
        .rfind(|(path, verdict)| {
            verdict.stage == Stage::Review
                && !verdict.sha.is_empty()
                && Some(path) != current.as_ref()
        })
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
pub(crate) fn attempt_label(task: &model::TaskFile) -> Result<String, Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The follow-up's slug is the merged task's slug plus the marker, cut to the slug limit
    /// and still a valid slug: the cut never leaves a trailing `-`.
    #[test]
    fn the_followup_slug_carries_the_merged_slug_and_stays_a_slug() {
        assert_eq!(followup_slug("TK0001-work"), "work-followups");
        let longest = "a".repeat(model::SLUG_MAX_CHARS);
        let cut = followup_slug(&format!("TK0002-{longest}"));
        assert_eq!(cut, longest, "the marker is cut off first: {cut}");
        assert!(model::valid_slug(&cut), "{cut}");
        let shorter = "a".repeat(model::SLUG_MAX_CHARS - 1);
        let cut = followup_slug(&format!("TK0003-{shorter}"));
        assert_eq!(cut, shorter, "the bare `-` at the cut is removed: {cut}");
        assert!(model::valid_slug(&cut), "{cut}");
        assert_eq!(
            followup_title("TK0001-work"),
            "Follow-ups from TK0001-work's review"
        );
    }

    /// The brief has the fixed form: the one line, one item per line as `[level] path:line
    /// text`, and the verdict path.
    #[test]
    fn the_followup_brief_names_the_task_the_items_and_the_verdict() {
        let items = vec![ReviewItem {
            level: Some("minor".to_owned()),
            file: Some("a.rs".to_owned()),
            line: Some(7),
            text: "a nicer name".to_owned(),
            pre_existing: true,
        }];
        let brief = followup_brief("TK0001-work", &items, Path::new("/r/verdicts/review.json"));
        assert!(
            brief.contains(
                "The items below are from the review of `TK0001-work`, which is now merged"
            ),
            "{brief}"
        );
        assert!(brief.contains("one-line reason"), "{brief}");
        assert!(
            brief
                .lines()
                .any(|line| line == "[minor] a.rs:7 a nicer name"),
            "{brief}"
        );
        assert!(brief.contains("/r/verdicts/review.json"), "{brief}");
    }
}

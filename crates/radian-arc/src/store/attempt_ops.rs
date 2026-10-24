//! The attempt lifecycle's store side (ADR 0048 decisions 4, 6, 9 and 12): the claim that
//! creates an attempt, the application of its result, the retry time, the budget pause and
//! its reset.
//!
//! Every function here changes a task file in one transaction under `state.lock`, the same
//! way [`super::plan_ops`] does: compare the file with the ledger, refuse a file that changed
//! outside arc, write the file, its ledger entry and the `.last.md` copy.
//!
//! The host-local attempt files next to `runs/` and `attempts/` are not part of the ledger:
//! `start.json` is written by the claim, `signal.json` by whoever signals the wrapper, and
//! `attempt_result.json` is arc's own record of what it applied.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use super::model::{self, TaskState};
use super::{Error, Kind, Store, cancelled_plan, require_unchanged};
use crate::store::config::ReworkSession;

/// The wrapper's own end record, in the attempt's run directory.
pub const ATTEMPT_FINISHED_FILE: &str = "attempt_finished.json";
/// The wrapper's last-written end record, in the host-local attempt directory.
pub const STATUS_FILE: &str = "status.json";
/// The claim's host-local start record: the attempt id, token and `started_at`, the time a
/// reconcile uses to judge a wrapper that has not written its first heartbeat yet.
pub const START_FILE: &str = "start.json";
/// Why the wrapper was signalled (`cancel`, `stall`, `wall_time`).
pub const SIGNAL_FILE: &str = "signal.json";
/// arc's record of the applied result: class, cost, and the state it wrote.
pub const ATTEMPT_RESULT_FILE: &str = "attempt_result.json";
/// The class a worker-less `radian arc reverify` attempt records (ADR 0048 decision 11). It is
/// not a failure class: the attempt ran no worker and failed nothing.
pub const REVERIFY_CLASS: &str = "reverify";
/// The record that the plan paused because this task used its automatic reworks.
pub const PAUSED_FILE: &str = "paused.json";
/// The wrapper's heartbeat, in the host-local attempt directory (ADR 0048 decision 7.9).
pub const HEARTBEAT_FILE: &str = "heartbeat.json";

/// The longest gap between two heartbeats: 15 seconds, or a third of the stall window when
/// that is shorter.
pub const HEARTBEAT_MAX: Duration = Duration::from_secs(15);

/// How often the wrapper writes its heartbeat: a third of the attempt's stall window, and 15
/// seconds when the window is longer than 45 seconds. Every window therefore holds at least
/// three beats (a 1-second window gives 333 ms). Without a stall limit, the beats are 15
/// seconds apart and the liveness window is three beats, 45 seconds.
pub fn heartbeat_interval(stall_secs: u64) -> Duration {
    if stall_secs == 0 {
        HEARTBEAT_MAX
    } else {
        (Duration::from_secs(stall_secs) / 3).min(HEARTBEAT_MAX)
    }
}

/// How old a last heartbeat may be before a pass calls the attempt dead: the attempt's stall
/// window, or three beats when the attempt has no stall limit (ADR 0048 decision 9).
pub fn liveness_window_secs(stall_secs: u64) -> u64 {
    if stall_secs == 0 {
        heartbeat_interval(0).as_secs() * 3
    } else {
        stall_secs
    }
}

/// How many seconds ago `text` (an RFC 3339 UTC time) was. `None` when it cannot be read.
pub fn utc_age_secs(text: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    let age = chrono::Utc::now().signed_duration_since(at.with_timezone(&chrono::Utc));
    u64::try_from(age.num_seconds()).ok()
}

/// The failure classes of decision 9, as one closed list the manual's drift test reads.
/// `stall` and `wall_time` are written by the wrapper when it signalled the worker; the other
/// four are the ADR's classes.
pub const FAILURE_CLASSES: &[&str] = &[
    "infrastructure",
    "model",
    "verification",
    "cancel",
    "stall",
    "wall_time",
];

/// The retry backoff: 10 seconds, doubling per attempt, capped at 5 minutes.
pub const RETRY_BASE_SECS: u64 = 10;
pub const RETRY_CAP_SECS: u64 = 300;

/// One failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Infrastructure,
    Model,
    Verification,
    Cancel,
    Stall,
    WallTime,
}

impl Class {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Infrastructure => "infrastructure",
            Class::Model => "model",
            Class::Verification => "verification",
            Class::Cancel => "cancel",
            Class::Stall => "stall",
            Class::WallTime => "wall_time",
        }
    }

    /// The class a stored spelling names.
    pub fn parse(text: &str) -> Option<Self> {
        [
            Class::Infrastructure,
            Class::Model,
            Class::Verification,
            Class::Cancel,
            Class::Stall,
            Class::WallTime,
        ]
        .into_iter()
        .find(|class| class.as_str() == text)
    }
}

/// The class an outcome maps to when arc did not signal the wrapper. `poisoned` is a model
/// failure that never retries (it goes to `failed.irrecoverable`); every other entry retries.
fn outcome_class(outcome: &str, wrapper_exit: Option<i32>) -> Class {
    if wrapper_exit == Some(4) {
        return Class::Infrastructure;
    }
    match outcome {
        "refused" | "failed" | "incomplete" | "budget_exceeded" | "cancelled" => Class::Model,
        // A crash, a kill, or an outcome this build does not know: arc cannot prove the
        // worker ran to its own end, so it retries on the infrastructure side.
        _ => Class::Infrastructure,
    }
}

/// What one claim created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub task: String,
    pub attempt: u32,
    pub attempt_id: String,
    pub token: String,
    /// A later attempt that continues the task's one session (`[worker] rework_session =
    /// "resume"`). With `fresh` every attempt has its own session, so this is false.
    pub resume: bool,
    /// The session the worker runs in: the task id with `resume`, `<task>-<attempt>` with
    /// `fresh` (ADR 0048 decision 4).
    pub session_id: String,
    pub allow_noop: bool,
    /// The task body: the brief and every rework section.
    pub brief: String,
}

/// What one worker-less `radian arc reverify` claim created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverifyClaim {
    pub attempt: u32,
    pub attempt_id: String,
    pub token: String,
}

/// What one applied result did.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub attempt_id: String,
    pub outcome: String,
    pub class: Option<Class>,
    pub state: String,
    pub phase: String,
    pub retry_at: Option<String>,
    pub reason: Option<String>,
    /// The cost this attempt added, in US dollars.
    pub cost_usd: f64,
    /// Set when nothing changed: the result did not belong to the current attempt.
    pub ignored: Option<String>,
}

impl Applied {
    /// The one line `radian arc start` prints.
    pub fn line(&self) -> String {
        if let Some(reason) = &self.ignored {
            return format!("{} ignored: {reason}", self.attempt_id);
        }
        let mut line = format!(
            "{} {} -> {}.{}",
            self.attempt_id, self.outcome, self.state, self.phase
        );
        if let Some(retry_at) = &self.retry_at {
            line.push_str(&format!(" (retry at {retry_at})"));
        }
        if let Some(reason) = &self.reason
            && !reason.is_empty()
        {
            line.push_str(&format!(" ({reason})"));
        }
        line
    }

    /// Whether the task left `doing` (the `start` exit code's rule).
    pub fn left_doing(&self) -> bool {
        self.state != TaskState::Doing.as_str()
    }
}

/// The wrapper's `AttemptFinished` record, as arc reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub attempt_id: String,
    pub token: String,
    pub exit_code: i32,
    pub outcome: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub pushed: bool,
    pub failure: Option<String>,
}

impl Finish {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            attempt_id: value.get("attempt_id")?.as_str()?.to_owned(),
            token: value.get("token")?.as_str()?.to_owned(),
            exit_code: value.get("exit_code").and_then(Value::as_i64).unwrap_or(-1) as i32,
            outcome: value.get("outcome")?.as_str()?.to_owned(),
            base_sha: sha_field(value, "base_sha"),
            head_sha: sha_field(value, "head_sha"),
            pushed: value
                .get("pushed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            failure: value
                .get("failure")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

fn sha_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}

// ---- paths ---------------------------------------------------------------------------

/// `runs/<task>/attempts/<task>-<attempt>/`: the attempt id names the directory, the same
/// label the gate writes its verdicts under (ADR 0048 decisions 5 and 10).
pub fn run_dir(store: &Store, task: &str, attempt: u32) -> PathBuf {
    store
        .root()
        .join("runs")
        .join(task)
        .join("attempts")
        .join(format!("{task}-{attempt}"))
}

/// `attempts/<task>/<attempt>/`.
pub fn attempt_dir(store: &Store, task: &str, attempt: u32) -> PathBuf {
    store
        .root()
        .join("attempts")
        .join(task)
        .join(attempt.to_string())
}

/// The wrapper's end record for one attempt, when it wrote one. The host-local `status.json`
/// is the fallback: the wrapper writes both, and either proves the attempt ended.
pub fn read_finish(store: &Store, task: &str, attempt: u32) -> Option<Finish> {
    let run = run_dir(store, task, attempt).join(ATTEMPT_FINISHED_FILE);
    let status = attempt_dir(store, task, attempt).join(STATUS_FILE);
    let text = fs::read_to_string(run)
        .or_else(|_| fs::read_to_string(status))
        .ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Finish::from_value(&value)
}

/// Why the wrapper of one attempt was signalled, when arc signalled it.
pub fn read_signal(store: &Store, task: &str, attempt: u32) -> Option<Class> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join(SIGNAL_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("reason")
        .and_then(Value::as_str)
        .and_then(Class::parse)
}

/// The wrapper's last heartbeat time for one attempt, as it wrote it.
pub fn read_heartbeat(store: &Store, task: &str, attempt: u32) -> Option<String> {
    let text = fs::read_to_string(attempt_dir(store, task, attempt).join(HEARTBEAT_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("last_beat")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The time the claim that created the attempt recorded, for a wrapper that has not beaten
/// yet. `start.json` is written in the same transaction as the task's move to `doing`.
pub fn read_start_time(store: &Store, task: &str, attempt: u32) -> Option<String> {
    let text = fs::read_to_string(attempt_dir(store, task, attempt).join(START_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("started_at")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Records why arc signalled the wrapper. The first reason wins: a stall followed by a wall
/// time records the stall, which is what ended the attempt.
pub fn write_signal(store: &Store, task: &str, attempt: u32, class: Class) -> Result<(), Error> {
    let path = run_dir(store, task, attempt).join(SIGNAL_FILE);
    if path.is_file() {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let text = serde_json::to_string_pretty(&json!({"reason": class.as_str()}))
        .map_err(|error| Error::Failed(format!("cannot encode the signal: {error}")))?;
    fs::write(&path, format!("{text}\n"))
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))
}

// ---- claim ---------------------------------------------------------------------------

/// Claims one task for the owner's `start` (decision 6): `todo.ready` or `todo.retry_wait` to
/// `doing.starting` in one transaction. A `todo.retry_wait` task is claimed without consulting
/// `retry_at`, because the backoff exists for the scheduling loop (decision 9).
///
/// Refused (exit 3) when the task is in any other state or phase, its plan is not `confirmed`,
/// an `after` task is not `done`, or `[limits] concurrency` tasks are already in `doing`.
pub fn claim(store: &Store, task_id: &str) -> Result<Claim, Error> {
    claim_by(store, task_id, ClaimBy::Owner)
}

/// Who claims a worker attempt. The allowed phases differ (decision 9): the owner's `start`
/// may claim a task that is backing off, a scheduling pass may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimBy {
    /// `radian arc start`: `todo.ready` and `todo.retry_wait` are both claimable, and
    /// `retry_at` is not consulted.
    Owner,
    /// A scheduling pass: only `todo.ready` is claimable. The phase is re-read under the lock,
    /// so a task that entered `todo.retry_wait` after the pass took its snapshot keeps its
    /// backoff.
    SchedulingLoop,
}

/// [`Claim`] with the caller's [`ClaimBy`] rules (decision 9).
///
/// Refused (exit 3) when the task is in any other state or phase, its plan is not `confirmed`,
/// an `after` task is not `done`, or `[limits] concurrency` tasks are already in `doing`.
pub fn claim_by(store: &Store, task_id: &str, by: ClaimBy) -> Result<Claim, Error> {
    let _lock = store.lock_state()?;
    let loaded = store.load_task(task_id)?;
    require_unchanged(task_id, loaded.changed_outside_arc)?;
    let mut task = loaded.item;
    let claimable = task.state == TaskState::Todo
        && match by {
            ClaimBy::Owner => matches!(task.phase.as_str(), "ready" | "retry_wait"),
            ClaimBy::SchedulingLoop => task.phase == "ready",
        };
    if !claimable {
        let allowed = match by {
            ClaimBy::Owner => "only a `todo.ready` or `todo.retry_wait` task starts",
            ClaimBy::SchedulingLoop => "only a `todo.ready` task starts from the scheduling loop",
        };
        return Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; {allowed}",
            task.state.as_str(),
            task.phase
        )));
    }
    let plan = store.load_plan(&task.plan)?;
    require_unchanged(&task.plan, plan.changed_outside_arc)?;
    if plan.item.status != "confirmed" {
        if plan.item.is_cancelled() {
            return Err(cancelled_plan(&task.plan));
        }
        return Err(Error::Refused(format!(
            "plan `{}` is `{}`; confirm it before starting its tasks",
            task.plan, plan.item.status
        )));
    }
    for dependency in &task.after {
        let dependency = store.load_task(dependency).map_err(|error| match error {
            Error::Failed(message) if message.starts_with("no task ") => Error::Refused(format!(
                "`after` names a task that does not exist: {message}"
            )),
            other => other,
        })?;
        if dependency.item.state != TaskState::Done {
            return Err(Error::Refused(format!(
                "`after` task `{}` is `{}.{}`; every `after` task must be done",
                dependency.item.id,
                dependency.item.state.as_str(),
                dependency.item.phase
            )));
        }
    }
    let config = store.config()?;
    let running = count_doing(store)?;
    if running >= u64::from(config.limits.concurrency) {
        return Err(Error::Refused(format!(
            "{running} task(s) are in `doing` and `limits.concurrency` is {}",
            config.limits.concurrency
        )));
    }
    let number = task.attempt + 1;
    let token = random_token()?;
    let attempt_id = format!("{task_id}-{number}");
    // ADR 0048 decision 4: `fresh` (the default) gives every attempt a session of its own, in
    // the shape the review sessions use; `resume` keeps the task's one session and continues
    // it. The wrapper is told both, so it never resumes a fresh attempt.
    let session_id = match config.worker.rework_session {
        ReworkSession::Fresh => format!("{task_id}-{number}"),
        ReworkSession::Resume => task_id.to_owned(),
    };
    let resume = number > 1 && config.worker.rework_session == ReworkSession::Resume;
    task.attempt = number;
    task.attempt_id = Some(attempt_id.clone());
    task.token = Some(token.clone());
    task.state = TaskState::Doing;
    task.phase = "starting".to_owned();
    task.retry_at = None;
    let version = store.next_version(Kind::Task, task_id);
    store.write_item(Kind::Task, task_id, &task.to_text(), version)?;
    // The claim's start time, so a reconcile can judge a claim whose wrapper has not written
    // its first heartbeat yet. It is host-local and outside the ledger. Nothing reads the
    // claiming process's identity here: the wrapper records its own in `identity.json`.
    let dir = attempt_dir(store, task_id, number);
    fs::create_dir_all(&dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let record = json!({
        "attempt_id": attempt_id,
        "token": token,
        "started_at": super::plan_ops::utc_now(),
    });
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| Error::Failed(format!("cannot encode the start record: {error}")))?;
    fs::write(dir.join(START_FILE), format!("{text}\n")).map_err(|error| {
        Error::Failed(format!(
            "cannot write {}: {error}",
            dir.join(START_FILE).display()
        ))
    })?;
    Ok(Claim {
        task: task_id.to_owned(),
        attempt: number,
        attempt_id,
        token,
        resume,
        session_id,
        allow_noop: task.allow_noop,
        brief: task.body.clone(),
    })
}

/// Claims a new attempt for `radian arc reverify` (ADR 0048 decision 11): no worker runs and
/// nothing is pushed.
///
/// The task must be `review.rework_pending` or `review.awaiting_human`, and a rework reason
/// must come from a verification stage: a reject note, a delivery failure and any other
/// reason are refused (exit 3). A `review.verifying` task is accepted when its gate is stuck
/// (`no_record` or `died` for its current attempt, with no gate running). The claim keeps
/// `head_sha`, gives the attempt a new number, `attempt_id` and `token`, clears the old
/// approval, and records an attempt result whose class is [`REVERIFY_CLASS`]. No
/// `result.jsonl` is written: no worker ran.
pub fn claim_reverify(store: &Store, task_id: &str) -> Result<ReverifyClaim, Error> {
    claim_reverify_by(store, task_id, ReverifyBy::Owner)
}

/// Who claims a worker-less attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverifyBy {
    /// `radian arc reverify`: the rework reason must be a failed verification stage.
    Owner,
    /// A scheduling pass's automatic reverify: the pass proved itself that the reason is a
    /// `commands` or integration command failure or timeout that a moved main can fix
    /// (ADR 0048 decision 9), so the owner's reason check does not apply.
    Automatic,
}

/// [`claim_reverify`] for one source of the claim. An automatic claim is the same transaction
/// and leaves the same records; only the owner's rework-reason check is skipped.
///
/// # Errors
///
/// As [`claim_reverify`].
pub fn claim_reverify_by(
    store: &Store,
    task_id: &str,
    by: ReverifyBy,
) -> Result<ReverifyClaim, Error> {
    let _lock = store.lock_state()?;
    let loaded = store.load_task(task_id)?;
    require_unchanged(task_id, loaded.changed_outside_arc)?;
    let mut task = loaded.item;
    // A cancelled plan is final: its tasks are not re-verified.
    let plan = store.load_plan(&task.plan)?;
    if plan.item.is_cancelled() {
        return Err(cancelled_plan(&task.plan));
    }
    // A stuck gate: the task stayed in `review.verifying` because its gate left no record or
    // died, and the owner wants it run again (ADR 0048 decisions 10 and 11).
    let gate_stuck = task.state == TaskState::Review
        && task.phase == "verifying"
        && by == ReverifyBy::Owner
        && crate::gate::background::stuck(store, task_id, &crate::review::attempt_label(&task)?);
    let allowed = task.state == TaskState::Review
        && (matches!(task.phase.as_str(), "rework_pending" | "awaiting_human") || gate_stuck);
    if !allowed {
        return Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; only a `review.rework_pending` or \
             `review.awaiting_human` task, or a `review.verifying` one whose gate left no \
             record or died, is re-verified",
            task.state.as_str(),
            task.phase
        )));
    }
    if task.attempt == 0 {
        return Err(Error::Refused(format!(
            "task `{task_id}` has no attempt to re-verify"
        )));
    }
    if task.head_sha.as_deref().is_none_or(|sha| sha.is_empty()) {
        return Err(Error::Refused(format!(
            "task `{task_id}` has no `head_sha`"
        )));
    }
    if by == ReverifyBy::Owner && task.phase != "verifying" {
        reverify_reason_allowed(store, &task)?;
    }
    let number = task.attempt + 1;
    let token = random_token()?;
    let attempt_id = format!("{task_id}-{number}");
    task.attempt = number;
    task.attempt_id = Some(attempt_id.clone());
    task.token = Some(token.clone());
    task.state = TaskState::Review;
    task.phase = "verifying".to_owned();
    // The new attempt's verdicts are its own: an old approval never carries over.
    task.verified_sha = None;
    task.verdict_set_digest = None;
    task.retry_at = None;
    let version = store.next_version(Kind::Task, task_id);
    store.write_item(Kind::Task, task_id, &task.to_text(), version)?;
    write_reverify_result(store, task_id, number, &attempt_id)?;
    Ok(ReverifyClaim {
        attempt: number,
        attempt_id,
        token,
    })
}

/// Whether `radian arc reverify` may re-run the gate for `task`: its rework reason must be a
/// failed verification stage. A reject note and a delivery failure are refused, and so is any
/// other recorded reason; a task with no recorded reason is accepted (the decision's "if any").
fn reverify_reason_allowed(store: &Store, task: &model::TaskFile) -> Result<(), Error> {
    if task.phase == "awaiting_human" {
        return Ok(());
    }
    let run = run_dir(store, &task.id, task.attempt);
    if run.join(crate::review::REJECT_FILE).is_file() {
        return Err(Error::Refused(format!(
            "task `{}` was rejected with a note; `radian arc reverify` re-runs only a \
             verification stage",
            task.id
        )));
    }
    if let Ok(text) = fs::read_to_string(run.join(crate::review::REVIEW_FILE))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
    {
        let action = value.get("action").and_then(Value::as_str);
        let stage = value.get("stage").and_then(Value::as_str);
        if action == Some("verify") && stage.is_some_and(is_verification_stage) {
            return Ok(());
        }
        let reason = value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("a gate reason");
        return Err(Error::Refused(format!(
            "task `{}` waits for rework from `{reason}`, which is not a verification stage",
            task.id
        )));
    }
    if run.join(PAUSED_FILE).is_file() {
        return Err(Error::Refused(format!(
            "task `{}` was paused by the automatic rework limit; `radian arc reverify` re-runs \
             only a verification stage",
            task.id
        )));
    }
    if let Ok(text) = fs::read_to_string(run.join(ATTEMPT_RESULT_FILE))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
        && value.get("class").and_then(Value::as_str) == Some(Class::Verification.as_str())
    {
        return Err(Error::Refused(format!(
            "task `{}` failed its delivery check; `radian arc reverify` re-runs only a \
             verification stage",
            task.id
        )));
    }
    Ok(())
}

/// Whether `name` is a stage verification runs. `human` is the owner's approval and
/// `integration` runs at merge; neither is a verification stage.
fn is_verification_stage(name: &str) -> bool {
    crate::gate::LISTED_STAGES
        .iter()
        .copied()
        .find(|stage| stage.as_str() == name)
        .is_some_and(|stage| stage != crate::gate::Stage::Human)
}

/// Writes the worker-less attempt's own result record: class [`REVERIFY_CLASS`], no cost, and
/// the state the claim wrote. It is the record reconcile reads to apply the attempt's end; the
/// attempt never reaches [`apply`], so it changes no retry count.
fn write_reverify_result(
    store: &Store,
    task: &str,
    attempt: u32,
    attempt_id: &str,
) -> Result<(), Error> {
    let record = json!({
        "attempt_id": attempt_id,
        "outcome": "reverify",
        "class": REVERIFY_CLASS,
        "reason": null,
        "cost_usd": 0.0,
        "session_cost_usd": null,
        "state": "review",
        "phase": "verifying",
        "retry_at": null,
        "applied_at": super::plan_ops::utc_now(),
    });
    let path = run_dir(store, task, attempt).join(ATTEMPT_RESULT_FILE);
    let dir = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| Error::Failed(format!("cannot encode the attempt result: {error}")))?;
    fs::write(&path, format!("{text}\n"))
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))
}

/// How many tasks are in `doing`.
fn count_doing(store: &Store) -> Result<u64, Error> {
    let mut count = 0;
    for id in store.task_ids()? {
        if store.load_task(&id)?.item.state == TaskState::Doing {
            count += 1;
        }
    }
    Ok(count)
}

/// A random token: 16 bytes from the OS, hex. `/dev/urandom` is the only entropy source arc
/// needs, and it is present on every host v0 runs on.
fn random_token() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| Error::Failed(format!("cannot read /dev/urandom: {error}")))?;
    let mut token = String::with_capacity(32);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

// ---- apply ---------------------------------------------------------------------------

/// Applies one attempt's result (decision 9) in one transaction.
///
/// `finish` is the wrapper's record. `signal` is set when the attempt was signalled: `cancel`
/// ends the task at `failed.cancelled`; `stall` and `wall_time` are written by the wrapper
/// when it stopped its own worker, and are the attempt's class. `wrapper_exit` is the
/// wrapper's own exit code, used when it wrote no record (a crash or a lock refusal).
///
/// The token must be the task's current attempt's, and the attempt number must be the
/// current one; otherwise nothing changes and the result is `ignored`.
pub fn apply(
    store: &Store,
    task_id: &str,
    attempt: u32,
    finish: Option<&Finish>,
    signal: Option<Class>,
    wrapper_exit: Option<i32>,
) -> Result<Applied, Error> {
    let cost = attempt_cost(store, task_id, attempt);
    let budget = BudgetCheck::new(store, task_id)?;
    let (applied, pause_plan, reached_verification) = {
        let _lock = store.lock_state()?;
        let loaded = store.load_task(task_id)?;
        require_unchanged(task_id, loaded.changed_outside_arc)?;
        let mut task = loaded.item;
        if task.state != TaskState::Doing {
            return Ok(ignored(
                &task,
                finish,
                format!("the task is `{}.{}`", task.state.as_str(), task.phase),
            ));
        }
        if task.attempt != attempt {
            return Ok(ignored(
                &task,
                finish,
                format!("the task's current attempt is {}", task.attempt),
            ));
        }
        if let Some(finish) = finish
            && (task.token.as_deref() != Some(finish.token.as_str())
                || task.attempt_id.as_deref() != Some(finish.attempt_id.as_str()))
        {
            return Ok(ignored(
                &task,
                Some(finish),
                "the result's token is not current".to_owned(),
            ));
        }
        let outcome = finish
            .map(|finish| finish.outcome.clone())
            .unwrap_or_else(|| "terminated".to_owned());
        let exit_code = finish.map(|finish| finish.exit_code);
        let delivery_failure = finish.and_then(|finish| finish.failure.clone());
        let pushed = finish.is_some_and(|finish| finish.pushed);

        // The mapping (decision 9): a cancel ends the task; a pushed `completed` goes to
        // review; a completed child whose delivery check failed is a `verification` result;
        // a poisoned session is irrecoverable; everything else retries under its class.
        let (class, next) = if signal == Some(Class::Cancel) {
            (Some(Class::Cancel), Next::Cancelled)
        } else if outcome == "completed" && pushed {
            (None, Next::Verifying)
        } else if outcome == "completed" {
            (Some(Class::Verification), Next::Rework)
        } else if outcome == "poisoned" {
            (Some(Class::Model), Next::Irrecoverable)
        } else if let Some(signal) = signal {
            (Some(signal), Next::Retry)
        } else {
            (Some(outcome_class(&outcome, wrapper_exit)), Next::Retry)
        };

        let reason = delivery_failure.clone().or_else(|| match &next {
            Next::Retry if exit_code == Some(4) => {
                Some("another attempt holds the lock".to_owned())
            }
            Next::Retry if signal.is_some() => {
                signal.map(|class| format!("the wrapper was signalled: {}", class.as_str()))
            }
            Next::Irrecoverable => Some("the session cannot continue".to_owned()),
            _ => None,
        });
        // Every applied attempt adds its cost to the task's running total (decision 12), so a
        // budget that ran out while the attempt ran pauses the task even when it failed
        // otherwise retryable.
        task.cost_usd = Some(task.cost_usd.unwrap_or(0.0) + cost.cost_usd);
        let reached_verification = matches!(next, Next::Verifying);
        let mut retry_at = None;
        let mut pause_plan = false;
        match next {
            Next::Verifying | Next::Rework => {
                task.state = TaskState::Review;
                task.phase = if matches!(next, Next::Verifying) {
                    "verifying"
                } else {
                    "rework_pending"
                }
                .to_owned();
                task.base_sha = finish.and_then(|finish| finish.base_sha.clone());
                task.head_sha = finish.and_then(|finish| finish.head_sha.clone());
                task.retry_at = None;
            }
            Next::Irrecoverable => {
                task.state = TaskState::Failed;
                task.phase = "irrecoverable".to_owned();
                task.retry_at = None;
            }
            Next::Cancelled => {
                task.state = TaskState::Failed;
                task.phase = "cancelled".to_owned();
                task.retry_at = None;
            }
            Next::Retry => {
                let max_attempts = task
                    .max_attempts
                    .unwrap_or(budget.config.limits.max_attempts);
                let plan_reached = budget.plan_reached(cost.cost_usd);
                if budget.task_reached(&task) || plan_reached {
                    task.state = TaskState::Todo;
                    task.phase = "paused_budget".to_owned();
                    task.retry_at = None;
                    pause_plan = plan_reached;
                } else if task.retry_count < max_attempts {
                    task.state = TaskState::Todo;
                    task.phase = "retry_wait".to_owned();
                    task.retry_count += 1;
                    let at = retry_at_after(task.retry_count);
                    task.retry_at = Some(at.clone());
                    retry_at = Some(at);
                } else {
                    task.state = TaskState::Failed;
                    task.phase = "exhausted".to_owned();
                    task.retry_at = None;
                }
            }
        }
        let version = store.next_version(Kind::Task, task_id);
        store.write_item(Kind::Task, task_id, &task.to_text(), version)?;
        let applied = Applied {
            attempt_id: finish
                .map(|finish| finish.attempt_id.clone())
                .unwrap_or_else(|| format!("{task_id}-{attempt}")),
            outcome,
            class,
            state: task.state.as_str().to_owned(),
            phase: task.phase.clone(),
            retry_at,
            reason,
            cost_usd: cost.cost_usd,
            ignored: None,
        };
        (applied, pause_plan, reached_verification)
    };
    // An attempt that ends without reaching the gate did not conflict with main, so the task's
    // run of conflicts is over (ADR 0048 decision 9).
    if !reached_verification {
        crate::review::reset_conflict_rework(store, task_id)?;
    }
    // The attempt's own record: the class, the cost, and the state arc wrote. Outside the
    // ledger, so a crash after the task write leaves the task's state authoritative.
    let record = json!({
        "attempt_id": applied.attempt_id,
        "outcome": applied.outcome,
        "class": applied.class.map(Class::as_str),
        "reason": applied.reason,
        "cost_usd": applied.cost_usd,
        "session_cost_usd": cost.session_cost_usd,
        "state": applied.state,
        "phase": applied.phase,
        "retry_at": applied.retry_at,
        "applied_at": super::plan_ops::utc_now(),
    });
    let path = run_dir(store, task_id, attempt).join(ATTEMPT_RESULT_FILE);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&record) {
        let _ = fs::write(&path, format!("{text}\n"));
    }
    // A plan budget pauses the whole plan: its auto level goes to `off` in its own
    // transaction, after the task lock is released.
    if pause_plan {
        let plan_id = store.load_task(task_id)?.item.plan;
        let _ = store.change_plan(&plan_id, |plan| {
            plan.auto = "off".to_owned();
            Ok(())
        });
    }
    Ok(applied)
}

/// What an applied result moves the task to.
enum Next {
    Verifying,
    Rework,
    Irrecoverable,
    Cancelled,
    Retry,
}

fn ignored(task: &model::TaskFile, finish: Option<&Finish>, reason: String) -> Applied {
    Applied {
        attempt_id: finish
            .map(|finish| finish.attempt_id.clone())
            .or_else(|| task.attempt_id.clone())
            .unwrap_or_else(|| format!("{}-{}", task.id, task.attempt)),
        outcome: finish
            .map(|finish| finish.outcome.clone())
            .unwrap_or_default(),
        class: None,
        state: task.state.as_str().to_owned(),
        phase: task.phase.clone(),
        retry_at: task.retry_at.clone(),
        reason: None,
        cost_usd: 0.0,
        ignored: Some(reason),
    }
}

/// The retry time for an attempt that failed: 10 s for the first retry, doubled per retry
/// since the task's last rework, capped at 5 minutes.
pub fn retry_at_after(retry_count: u32) -> String {
    let delay = retry_delay_secs(retry_count);
    let at = chrono::Utc::now() + chrono::Duration::seconds(delay as i64);
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The backoff in seconds for the retry that just ended the attempt.
pub fn retry_delay_secs(retry_count: u32) -> u64 {
    let exponent = retry_count.saturating_sub(1).min(5);
    (RETRY_BASE_SECS << exponent).min(RETRY_CAP_SECS)
}

// ---- cost and budget -----------------------------------------------------------------

/// One attempt's cost: the cost the attempt added. `Result.cost` is the session total
/// (ADR 0044), so an attempt that shares the previous attempt's session (`resume`) adds the
/// difference, and a `fresh` attempt, whose session is its own, adds the whole total.
struct AttemptCost {
    cost_usd: f64,
    session_cost_usd: Option<f64>,
}

fn attempt_cost(store: &Store, task: &str, attempt: u32) -> AttemptCost {
    let this = session_total_micros(store, task, attempt);
    let shared_session = attempt > 1
        && attempt_session_id(store, task, attempt).is_some_and(|session| {
            attempt_session_id(store, task, attempt - 1).as_deref() == Some(session.as_str())
        });
    let previous = shared_session
        .then(|| session_total_micros(store, task, attempt - 1))
        .flatten();
    let delta = this.unwrap_or(0).saturating_sub(previous.unwrap_or(0));
    AttemptCost {
        cost_usd: delta as f64 / 1_000_000.0,
        session_cost_usd: this.map(|micros| micros as f64 / 1_000_000.0),
    }
}

/// The worker session an attempt recorded in its `execution.json`. `None` for an attempt whose
/// wrapper wrote no record (a worker-less `reverify`) and for an attempt that never ran. This
/// is how a later reader finds a worker's journal: by the attempt's session id, which is the
/// task id only with `[worker] rework_session = "resume"` or for an older attempt.
pub fn attempt_session_id(store: &Store, task: &str, attempt: u32) -> Option<String> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join("execution.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The `Result.cost.micros` of one attempt's jsonl stream, when it wrote one.
fn session_total_micros(store: &Store, task: &str, attempt: u32) -> Option<u64> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join("result.jsonl")).ok()?;
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("Result") {
            continue;
        }
        return value
            .get("cost")
            .and_then(|cost| cost.get("micros"))
            .and_then(Value::as_u64);
    }
    None
}

/// What the budget checks need: the config, the task's limit, and the plan's limit and its
/// counted cost before this attempt.
struct BudgetCheck {
    config: super::config::Config,
    task_limit: Option<f64>,
    plan_limit: Option<f64>,
    plan_total: f64,
}

impl BudgetCheck {
    fn new(store: &Store, task_id: &str) -> Result<Self, Error> {
        let config = store.config()?;
        let task = store.load_task(task_id)?.item;
        let plan = store.load_plan(&task.plan)?.item;
        let mut plan_total = 0.0;
        for id in &plan.tasks {
            if let Ok(task) = store.load_task(id) {
                plan_total += task.item.cost_usd.unwrap_or(0.0);
            }
        }
        Ok(Self {
            config,
            task_limit: task.max_cost_usd,
            plan_limit: plan.max_cost_usd,
            plan_total,
        })
    }

    /// Whether the task's own budget (its field, else `[limits] max_cost_usd`) is used up.
    fn task_reached(&self, task: &model::TaskFile) -> bool {
        let limit = self.task_limit.or(self.config.limits.max_cost_usd);
        limit.is_some_and(|limit| task.cost_usd.unwrap_or(0.0) >= limit)
    }

    /// Whether the plan's budget is used up once this attempt's cost is counted.
    fn plan_reached(&self, attempt_cost: f64) -> bool {
        self.plan_limit
            .is_some_and(|limit| self.plan_total + attempt_cost >= limit)
    }
}

// ---- retries and reset-budget --------------------------------------------------------

/// `todo.retry_wait` to `todo.ready` for every task whose `retry_at` is at or before `now`
/// (an absolute UTC time). The scheduling loop calls this; it changes nothing else.
pub fn release_retries(store: &Store, now: &str) -> Result<Vec<String>, Error> {
    if !model::valid_utc(now) {
        return Err(Error::Usage(format!(
            "`{now}` is not an absolute UTC time (YYYY-MM-DDTHH:MM:SSZ)"
        )));
    }
    let mut released = Vec::new();
    for id in store.task_ids()? {
        let loaded = store.load_task(&id)?;
        if loaded.changed_outside_arc {
            // arc does not act on a file that changed outside it; another due task must not
            // be blocked by this one.
            continue;
        }
        let task = loaded.item;
        if task.state != TaskState::Todo || task.phase != "retry_wait" {
            continue;
        }
        let due = task.retry_at.as_deref().is_some_and(|at| at <= now);
        if !due {
            continue;
        }
        store.change_task(&id, |task| {
            task.state = TaskState::Todo;
            task.phase = "ready".to_owned();
            task.retry_at = None;
            Ok(())
        })?;
        released.push(id);
    }
    Ok(released)
}

/// What one `reset-budget` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetReset {
    pub id: String,
    /// `"plan"` or `"task"`.
    pub kind: &'static str,
    /// The tasks moved from `todo.paused_budget` back to `todo.ready`.
    pub tasks: Vec<String>,
}

/// `radian arc reset-budget <id>`: moves the paused task, or every paused task of the plan,
/// back to `todo.ready`, and resets the counted cost to zero from now on.
///
/// The id resolves to the plan or the task as `show` does; an id that names both is refused.
pub fn reset_budget(store: &Store, id: &str) -> Result<BudgetReset, Error> {
    match store.resolve_kind(id)? {
        Kind::Plan => {
            let plan = store.load_plan(id)?;
            require_unchanged(id, plan.changed_outside_arc)?;
            if plan.item.is_cancelled() {
                return Err(cancelled_plan(id));
            }
            let mut moved = Vec::new();
            for task_id in &plan.item.tasks {
                let task = store.load_task(task_id)?;
                if task.changed_outside_arc {
                    // arc does not act on a file that changed outside it.
                    continue;
                }
                let paused =
                    task.item.state == TaskState::Todo && task.item.phase == "paused_budget";
                // The plan's counted cost is every task's, so every task resets even when
                // only some of them are paused.
                store.change_task(task_id, |task| {
                    task.cost_usd = Some(0.0);
                    if paused {
                        task.state = TaskState::Todo;
                        task.phase = "ready".to_owned();
                        task.retry_at = None;
                    }
                    Ok(())
                })?;
                if paused {
                    moved.push(task_id.clone());
                }
            }
            Ok(BudgetReset {
                id: id.to_owned(),
                kind: "plan",
                tasks: moved,
            })
        }
        Kind::Task => {
            let task = store.load_task(id)?;
            let plan = store.load_plan(&task.item.plan)?;
            if plan.item.is_cancelled() {
                return Err(cancelled_plan(&plan.item.id));
            }
            if task.item.state != TaskState::Todo || task.item.phase != "paused_budget" {
                return Err(Error::Refused(format!(
                    "task `{id}` is `{}.{}`; only a `todo.paused_budget` task has a budget to reset",
                    task.item.state.as_str(),
                    task.item.phase
                )));
            }
            store.change_task(id, |task| {
                task.state = TaskState::Todo;
                task.phase = "ready".to_owned();
                task.retry_at = None;
                task.cost_usd = Some(0.0);
                Ok(())
            })?;
            Ok(BudgetReset {
                id: id.to_owned(),
                kind: "task",
                tasks: vec![id.to_owned()],
            })
        }
    }
}

// ---- automatic rework ------------------------------------------------------------------

/// What one automatic rework did to the plan: the pause, when the task went past its limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paused {
    pub task: String,
    pub plan: String,
    /// How many reworks the task has now had while the plan was not `off`.
    pub count: u32,
    pub max_auto_rework: u32,
    /// The one line `run` prints and `show` reports.
    pub text: String,
}

/// Counts one automatic rework for a task that just reached `review.rework_pending` (a failed
/// stage or a failed integration). Nothing happens while the task's plan is at `auto` `off`.
///
/// When the count would go past `[limits] max_auto_rework`, arc pauses the whole plan: its
/// `auto` becomes `off`, the task stays in `review.rework_pending`, and the reason is written
/// next to the attempt so `show` can print it.
pub fn record_auto_rework(store: &Store, task_id: &str) -> Result<Option<Paused>, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Ok(None);
    }
    let task = loaded.item;
    if task.state != TaskState::Review || task.phase != "rework_pending" {
        return Ok(None);
    }
    let plan = store.load_plan(&task.plan)?;
    if plan.changed_outside_arc || plan.item.auto == "off" {
        // A person, or a plan budget, already paused the plan: this rework is not automatic.
        return Ok(None);
    }
    let max = store.config()?.limits.max_auto_rework;
    let mut count = task.auto_rework;
    store.change_task(task_id, |task| {
        task.auto_rework += 1;
        count = task.auto_rework;
        Ok(())
    })?;
    if count <= max {
        return Ok(None);
    }
    let text = format!(
        "the plan paused: task {task_id} used {count} automatic reworks and the auto rework \
         limit is {max}"
    );
    let record = json!({
        "kind": "auto_rework",
        "task": task_id,
        "plan": plan.item.id,
        "count": count,
        "max_auto_rework": max,
        "text": text,
        "at": super::plan_ops::utc_now(),
    });
    let dir = run_dir(store, task_id, task.attempt);
    if fs::create_dir_all(&dir).is_ok()
        && let Ok(body) = serde_json::to_string_pretty(&record)
    {
        let _ = fs::write(dir.join(PAUSED_FILE), format!("{body}\n"));
    }
    let plan_id = plan.item.id.clone();
    store.change_plan(&plan_id, |plan| {
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    Ok(Some(Paused {
        task: task_id.to_owned(),
        plan: plan_id,
        count,
        max_auto_rework: max,
        text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every stall window holds at least three beats, and no stall limit gives the 45-second
    /// window of three 15-second beats. Mutation: return a flat 15 seconds and a 1-second
    /// window gets one beat.
    #[test]
    fn the_heartbeat_fits_three_beats_in_every_stall_window() {
        for stall in [0, 1, 3, 5, 45, 900] {
            let interval = heartbeat_interval(stall);
            let window = Duration::from_secs(liveness_window_secs(stall));
            assert!(
                interval * 3 <= window,
                "stall {stall}: interval {interval:?} does not fit three times in {window:?}"
            );
        }
        assert_eq!(heartbeat_interval(0), Duration::from_secs(15));
        assert_eq!(liveness_window_secs(0), 45);
        assert_eq!(heartbeat_interval(1), Duration::from_secs(1) / 3);
        assert_eq!(heartbeat_interval(45), Duration::from_secs(15));
        assert_eq!(heartbeat_interval(900), Duration::from_secs(15));
        assert_eq!(liveness_window_secs(5), 5);
    }

    /// The closed class list and the class spellings cannot drift.
    #[test]
    fn the_failure_classes_are_one_closed_list() {
        let all = [
            Class::Infrastructure,
            Class::Model,
            Class::Verification,
            Class::Cancel,
            Class::Stall,
            Class::WallTime,
        ];
        assert_eq!(all.map(Class::as_str).to_vec(), FAILURE_CLASSES.to_vec());
        for class in all {
            assert_eq!(Class::parse(class.as_str()), Some(class));
        }
        assert_eq!(Class::parse("nope"), None);
    }

    /// 10 s, then 20 s, then 40 s, capped at 5 minutes.
    #[test]
    fn the_retry_backoff_doubles_and_is_capped() {
        assert_eq!(retry_delay_secs(1), 10);
        assert_eq!(retry_delay_secs(2), 20);
        assert_eq!(retry_delay_secs(3), 40);
        assert_eq!(retry_delay_secs(4), 80);
        assert_eq!(retry_delay_secs(5), 160);
        assert_eq!(retry_delay_secs(6), 300);
        assert_eq!(retry_delay_secs(100), 300);
    }

    /// The outcome table: the four model outcomes, a crash, and a lock refusal.
    #[test]
    fn outcomes_map_to_their_classes() {
        for outcome in ["refused", "failed", "incomplete", "budget_exceeded"] {
            assert_eq!(outcome_class(outcome, Some(1)), Class::Model, "{outcome}");
        }
        for outcome in ["terminated", "something-new"] {
            assert_eq!(
                outcome_class(outcome, None),
                Class::Infrastructure,
                "{outcome}"
            );
        }
        assert_eq!(outcome_class("cancelled", None), Class::Model);
        assert_eq!(outcome_class("failed", Some(4)), Class::Infrastructure);
    }
}

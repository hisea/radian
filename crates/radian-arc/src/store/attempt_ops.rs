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

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::model::{self, TaskState};
use super::{Error, Kind, Store, require_unchanged};
use crate::store::config::ReworkSession;

/// The wrapper's own end record, in the attempt's run directory.
pub const ATTEMPT_FINISHED_FILE: &str = "attempt_finished.json";
/// The wrapper's last-written end record, in the host-local attempt directory.
pub const STATUS_FILE: &str = "status.json";
/// The claim's host-local start record: the attempt id, token, `started_at`, the session id
/// and the worker the claim decided on. A reconcile uses `started_at` to judge a wrapper that
/// has not written its first heartbeat yet; `show` and the arc TUI use `session_id` to open the
/// worker's journal, and `worker` to classify the attempt, before the wrapper writes
/// `identity.json` or `execution.json`.
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
/// The attempt whose `review` stage cost arc has already added to the task's counters, and the
/// counters that charge left behind, in the task's run directory. It is not part of the task
/// file: it is bookkeeping that keeps the charge idempotent when a pass dies between the charge
/// and the state move, like the conflict rework count.
pub const REVIEW_COST_FILE: &str = "review_cost.json";

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

/// The worker line one attempt runs, decided once when the attempt is claimed (ADR 0048
/// decision 4). The claim records it in `start.json` and starts the wrapper from the same value,
/// so a later change to `config.toml` cannot make the record and the launched worker disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerLine {
    /// `radian acp`, with the flow's `exec_args` from the claim's configuration snapshot.
    Radian {
        exec_args: Vec<String>,
        flow: String,
    },
    /// The flow's `command`, with a program that contains a `/` made absolute from the
    /// repository root.
    Command { command: Vec<String>, flow: String },
    /// The flow's `acp`: an agent the wrapper starts and speaks ACP to (ADR 0048 decision 7).
    /// The program is resolved like a command's: a path from the repository root or a name on
    /// `PATH`.
    Acp { acp: Vec<String>, flow: String },
}

impl WorkerLine {
    /// The `worker` record `start.json` carries, with the flow the attempt starts from.
    #[must_use]
    pub fn record(&self) -> Value {
        match self {
            WorkerLine::Radian { flow, .. } => json!({"kind": "radian", "flow": flow}),
            WorkerLine::Command { command, flow } => json!({
                "kind": "command",
                "flow": flow,
                "program": command.first().cloned().unwrap_or_default(),
            }),
            WorkerLine::Acp { acp, flow } => json!({
                "kind": "acp",
                "flow": flow,
                "program": acp.first().cloned().unwrap_or_default(),
            }),
        }
    }

    /// The flow whose worker this attempt runs.
    #[must_use]
    pub fn flow(&self) -> &str {
        match self {
            WorkerLine::Radian { flow, .. }
            | WorkerLine::Command { flow, .. }
            | WorkerLine::Acp { flow, .. } => flow,
        }
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
    /// Whether the wrapper commits what a `completed` worker leaves uncommitted or untracked
    /// before the delivery check: the flow's `flows.<name>.commit_dirty`, read at this claim
    /// (ADR 0048 decision 8).
    pub commit_dirty: bool,
    pub brief: String,
    /// The worker this attempt runs, from the same configuration snapshot as its `start.json`
    /// record. The wrapper starts from this value, never from a later configuration read.
    pub worker: WorkerLine,
    /// The one line a claim prints when it starts a command worker for a task or plan with a
    /// cost limit: `<task>: worker.command reports no cost; the cost limit does not apply`
    /// (ADR 0048 decisions 7 and 12).
    pub no_cost_warning: Option<String>,
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
    pub task: String,
    pub attempt_id: String,
    pub outcome: String,
    pub class: Option<Class>,
    pub state: String,
    pub phase: String,
    pub retry_at: Option<String>,
    pub reason: Option<String>,
    /// The attempt ended on an unavailable model or a usage limit and the task waits on it
    /// without a retry (ADR 0048 decision 9).
    pub model_wait: bool,
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
        if self.model_wait
            && let Some(retry_at) = &self.retry_at
        {
            return format!(
                "{} worker model unavailable -> retry at {retry_at}",
                self.task
            );
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
/// an `after` task is not `done`, its flow is full, or `[limits] concurrency` tasks are already
/// in `doing`.
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
/// an `after` task is not `done`, its flow is full, or `[limits] concurrency` tasks are already
/// in `doing`.
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
    // A cancelled plan is final, by its `status` or by its tasks (ADR 0048 decisions 3, 9).
    store.refuse_cancelled(&task.plan)?;
    if plan.item.status != "confirmed" {
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
    // The task's flow decides the worker (ADR 0048 decisions 7 and 15). A flow the file no
    // longer defines leaves the task where it is: the claim writes nothing.
    let flow_name = task.flow.as_deref().unwrap_or(super::config::DEFAULT_FLOW);
    let Some(flow) = config.flows.get(flow_name) else {
        return Err(Error::Refused(super::config::missing_flow_reason(
            task_id, flow_name,
        )));
    };
    // Both limits apply (ADR 0048 decision 13): the task's own flow may not be full, and the
    // repository total may not be full. The flow's refusal comes first, so a full flow names
    // its own limit even when the total is full too; a file without `[flows]` has no flow
    // limit and keeps today's refusal.
    let counts = doing_counts(store)?;
    if let Some(limit) = config.flow_concurrency(flow_name) {
        let limit = u64::from(limit.max(1));
        if counts.in_flow(flow_name) >= limit {
            return Err(Error::Refused(format!(
                "{} task(s) of flow `{flow_name}` are in `doing` and \
                 `flows.{flow_name}.concurrency` is {limit}",
                counts.in_flow(flow_name)
            )));
        }
    }
    let running = counts.total;
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
    // The worker this attempt runs, decided here because a flow's worker takes effect at the
    // next claim. The wrapper is started with the same value, so a reader can tell a command
    // worker from the radian worker from the claim on, and a later `config.toml` change cannot
    // make the record and the launched worker disagree (decision 4).
    let worker = if let Some(words) = &flow.acp {
        WorkerLine::Acp {
            acp: crate::worker_args::resolved(words, store.repo()),
            flow: flow_name.to_owned(),
        }
    } else if let Some(words) = &flow.command {
        WorkerLine::Command {
            command: crate::worker_args::resolved(words, store.repo()),
            flow: flow_name.to_owned(),
        }
    } else {
        WorkerLine::Radian {
            exec_args: flow.exec_args.clone(),
            flow: flow_name.to_owned(),
        }
    };
    // A command or ACP worker reports no cost, so a task or plan cost limit cannot stop it.
    // The warning is printed once per attempt by `start`, `run` and `serve` (ADR 0048
    // decisions 7 and 12).
    let cost_limited = task.max_cost_usd.or(config.limits.max_cost_usd).is_some()
        || plan.item.max_cost_usd.is_some();
    let no_cost_warning = if cost_limited {
        match &worker {
            WorkerLine::Command { .. } => Some(format!(
                "{task_id}: worker.command reports no cost; the cost limit does not apply"
            )),
            WorkerLine::Acp { .. } => Some(format!(
                "{task_id}: worker.acp reports no cost; the cost limit does not apply"
            )),
            WorkerLine::Radian { .. } => None,
        }
    } else {
        None
    };
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
        // The session the claim decided on, so `show` and the arc TUI can open a running
        // worker's journal before the wrapper has written `identity.json` or
        // `execution.json` (ADR 0048 decision 4). The worker is here for the same reason: a
        // later configuration change must not re-classify a running attempt.
        "session_id": session_id,
        "worker": worker.record(),
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
        allow_noop: task.effective_allow_noop(&config),
        commit_dirty: flow.commit_dirty,
        brief: task.body.clone(),
        worker,
        no_cost_warning,
    })
}

/// Claims a new attempt for `radian arc reverify` (ADR 0048 decision 11): no worker runs and
/// nothing is pushed.
///
/// The task must be `review.rework_pending` or `review.awaiting_human`, and its rework reason
/// must be one a re-run can fix: a failed verification stage, or the integration stage failed
/// with `command_failed` or `timeout`. A reject note, a delivery failure, an integration
/// failure with any other reason, and any other reason are refused (exit 3). A
/// `review.verifying` task is accepted when its gate is stuck (`no_record` or `died` for its
/// current attempt, with no gate running). The claim keeps `head_sha`, gives the attempt a new
/// number, `attempt_id` and `token`, clears the old approval, and records an attempt result
/// whose class is [`REVERIFY_CLASS`]. No `result.jsonl` is written: no worker ran.
pub fn claim_reverify(store: &Store, task_id: &str) -> Result<ReverifyClaim, Error> {
    claim_reverify_by(store, task_id, ReverifyBy::Owner)
}

/// Who claims a worker-less attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverifyBy {
    /// `radian arc reverify`: the rework reason must be one a re-run can fix (a failed
    /// verification stage, or an integration command failure or timeout).
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
    store.refuse_cancelled(&task.plan)?;
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

/// Whether `radian arc reverify` may re-run the gate for `task`: its rework reason must be one
/// a re-run can fix, a failed verification stage or an integration command failure or timeout
/// (ADR 0048 decisions 9 and 11). A reject note, a paused task, a delivery failure, and any
/// other recorded reason are refused; a task with no recorded reason is accepted (the
/// decision's "if any").
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
        let reason = value.get("reason").and_then(Value::as_str);
        if action == Some("verify") && stage.is_some_and(is_verification_stage) {
            return Ok(());
        }
        if stage.is_some_and(|stage| {
            reason.is_some_and(|reason| crate::review::reverifiable_failure(stage, reason))
        }) {
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

/// How many tasks are in `doing`: the repository total, and how many belong to each flow. A
/// claim compares the total with `[limits] concurrency` and the flow's own count with
/// `[flows.<name>] concurrency` (ADR 0048 decision 13).
#[derive(Debug, Default, Clone)]
pub(crate) struct DoingCounts {
    /// Every task in `doing`.
    pub total: u64,
    /// The `doing` tasks of each flow name.
    by_flow: BTreeMap<String, u64>,
}

impl DoingCounts {
    /// How many `doing` tasks belong to `flow`.
    pub(crate) fn in_flow(&self, flow: &str) -> u64 {
        self.by_flow.get(flow).copied().unwrap_or(0)
    }

    /// Counts one more task of `flow`.
    pub(crate) fn add(&mut self, flow: &str) {
        self.total += 1;
        *self.by_flow.entry(flow.to_owned()).or_insert(0) += 1;
    }
}

/// Counts the tasks in `doing`, by flow. A `doing` task's flow is its `flow` field, `default`
/// when absent; it is fixed for the attempt's life because the field can change only before
/// the first attempt (ADR 0048 decision 6).
///
/// # Errors
///
/// A task file that cannot be read. The claim and a scheduling pass both read every file, so a
/// claim never counts a `doing` task it could not see.
pub(crate) fn doing_counts(store: &Store) -> Result<DoingCounts, Error> {
    let mut counts = DoingCounts::default();
    for id in store.task_ids()? {
        let task = store.load_task(&id)?.item;
        if task.state == TaskState::Doing {
            counts.add(task.flow.as_deref().unwrap_or(super::config::DEFAULT_FLOW));
        }
    }
    Ok(counts)
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
    // A model wait belongs to a `Result` that carries `model_error` (ADR 0048 decision 9): a
    // command worker's stdout may carry one, but its failures keep the ordinary retry rules,
    // and the radian worker's ACP response carries its error as text without a `kind`.
    let command_worker = worker_is_command(store, task_id, attempt);
    let model_error = result_model_error(store, task_id, attempt);
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
        // Every applied attempt adds its cost to the task's counters (decision 12): the
        // budget counter and the total that `reset-budget` leaves alone. An unknown cost (a
        // command worker that reported none) adds nothing, cannot reach a budget, and marks
        // the task's totals as incomplete for good: a later attempt that does report a cost
        // must not turn them into numbers that leave this one out.
        if cost.known {
            task.add_cost(cost.cost_usd);
        } else {
            task.cost_unknown = true;
        }
        let reached_verification = matches!(next, Next::Verifying);
        // An unavailable model, or a used-up usage window, is not the request's fault: the
        // attempt waits on the model's window and uses no retry (ADR 0048 decision 9). A
        // budget pause still wins, and then the attempt is not a model wait.
        let mut model_wait = signal.is_none()
            && class == Some(Class::Model)
            && model_error.is_some()
            && !command_worker;
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
                let plan_reached = cost.known && budget.plan_reached(cost.cost_usd);
                if (cost.known && budget.task_reached(&task)) || plan_reached {
                    task.state = TaskState::Todo;
                    task.phase = "paused_budget".to_owned();
                    task.retry_at = None;
                    model_wait = false;
                    pause_plan = plan_reached;
                } else if model_wait {
                    let error = model_error
                        .as_ref()
                        .expect("a model wait read its model error");
                    task.state = TaskState::Todo;
                    task.phase = "retry_wait".to_owned();
                    // The count of waits is not the retry count: a run of model waits does
                    // not use up `max_attempts`, and the next attempt that is not one ends
                    // the run (ADR 0048 decision 9).
                    let at = model_wait_retry_at(
                        consecutive_model_waits(store, task_id, attempt),
                        error,
                    );
                    task.retry_at = Some(at.clone());
                    retry_at = Some(at);
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
            task: task_id.to_owned(),
            attempt_id: finish
                .map(|finish| finish.attempt_id.clone())
                .unwrap_or_else(|| format!("{task_id}-{attempt}")),
            outcome,
            class,
            state: task.state.as_str().to_owned(),
            phase: task.phase.clone(),
            retry_at,
            reason,
            model_wait,
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
        // A command worker that reported no `cost` leaves both sides null: the attempt's
        // cost is unknown, it added nothing, and a reader tells it from a real zero
        // (decision 12).
        "cost_usd": cost.known.then_some(applied.cost_usd),
        "cost_known": cost.known,
        "session_cost_usd": cost.session_cost_usd,
        "state": applied.state,
        "phase": applied.phase,
        "retry_at": applied.retry_at,
        // A model wait records the session's own `model_error` object, as a review verdict
        // does; every other applied result leaves it null (ADR 0048 decision 9).
        "model_wait": applied.model_wait,
        "model_error": if applied.model_wait {
            model_error.as_ref()
        } else {
            None
        },
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
        task: task.id.clone(),
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
        model_wait: false,
        cost_usd: 0.0,
        ignored: Some(reason),
    }
}

/// The retry time for an attempt that failed: 10 s for the first retry, doubled per retry
/// since the task's last lead or review rework, capped at 5 minutes.
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
    /// Whether the attempt's stream carries a `cost` field at all. A `Result` without one (a
    /// command worker that reports none) leaves the attempt's cost unknown (ADR 0048
    /// decisions 7 and 12); a `cost` that is null or carries no `micros` is a reported zero,
    /// the shape `radian exec` writes when it recorded no fee.
    known: bool,
}

fn attempt_cost(store: &Store, task: &str, attempt: u32) -> AttemptCost {
    let this = result_cost(store, task, attempt);
    let shared_session = attempt > 1
        && attempt_session_id(store, task, attempt).is_some_and(|session| {
            attempt_session_id(store, task, attempt - 1).as_deref() == Some(session.as_str())
        });
    let previous = shared_session
        .then(|| result_cost(store, task, attempt - 1))
        .flatten()
        .and_then(|cost| cost.micros);
    let micros = this.as_ref().and_then(|cost| cost.micros);
    let delta = micros.unwrap_or(0).saturating_sub(previous.unwrap_or(0));
    AttemptCost {
        cost_usd: delta as f64 / 1_000_000.0,
        session_cost_usd: micros.map(|micros| micros as f64 / 1_000_000.0),
        known: this.as_ref().is_some_and(|cost| cost.present),
    }
}

/// The worker session of one attempt. A reader takes the first record that names one: the
/// wrapper's `execution.json`, the wrapper's `identity.json`, then the claim's `start.json`
/// (ADR 0048 decision 4). `None` only when none of them does: an old attempt, or a worker-less
/// `reverify` attempt. This is how a later reader finds a worker's journal, which is named by
/// the session id, not by the task.
pub fn attempt_session_id(store: &Store, task: &str, attempt: u32) -> Option<String> {
    [
        run_dir(store, task, attempt).join("execution.json"),
        attempt_dir(store, task, attempt).join("identity.json"),
        attempt_dir(store, task, attempt).join(START_FILE),
    ]
    .into_iter()
    .find_map(|path| session_id_in(&path))
}

/// The `session_id` one record names, when the file exists and parses.
fn session_id_in(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The notes the wrapper recorded for one attempt (ADR 0048 decision 7): a line such as
/// `worker-brief.md skipped: <reason>` or `verdict copy skipped: <path>: <reason>`. The wrapper
/// writes them to `execution.json`; an attempt that ended before that record was written leaves
/// none, and so does a worker-less `reverify` attempt. `show` prints every one.
pub fn attempt_notes(store: &Store, task: &str, attempt: u32) -> Vec<String> {
    [
        run_dir(store, task, attempt).join("execution.json"),
        run_dir(store, task, attempt).join("attempt_finished.json"),
    ]
    .into_iter()
    .find_map(|path| notes_in(&path))
    .unwrap_or_default()
}

/// The reason the current attempt ended without reaching the gate, for `show`: the applied
/// result's `reason`, else the wrapper's own `failure` from `attempt_finished.json`. A pushed
/// attempt and a passing one leave none. A delivery failure has both this and `rework_reason`;
/// `show` prints the one field, not both.
pub fn attempt_failure(store: &Store, task: &str, attempt: u32) -> Option<String> {
    let dir = run_dir(store, task, attempt);
    for file in [ATTEMPT_RESULT_FILE, ATTEMPT_FINISHED_FILE] {
        let Ok(text) = fs::read_to_string(dir.join(file)) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let reason = value
            .get("reason")
            .or_else(|| value.get("failure"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty());
        if let Some(reason) = reason {
            return Some(reason.to_owned());
        }
    }
    None
}

/// The `model_error` object of an attempt whose last `Result` says the model was unavailable or
/// its usage window was used up (ADR 0048 decision 9). `None` for any other kind, a `Result`
/// without one, no `Result` at all, and a command worker's record.
fn result_model_error(store: &Store, task: &str, attempt: u32) -> Option<Value> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join("result.jsonl")).ok()?;
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("Result") {
            continue;
        }
        // The last `Result` decides, as it does for the outcome and the cost: a `Result`
        // without a model error ends the search.
        let error = value.get("model_error")?;
        let kind = error.get("kind").and_then(Value::as_str)?;
        return crate::review::MODEL_WAIT_KINDS
            .contains(&kind)
            .then(|| error.clone());
    }
    None
}

/// Whether the attempt's recorded worker is a command worker (ADR 0048 decision 7): its
/// `execution.json`, then `identity.json`, then the claim's `start.json`, the same precedence
/// the session id uses. An attempt that names no worker is not a command worker: it keeps the
/// radian worker's rules.
fn worker_is_command(store: &Store, task: &str, attempt: u32) -> bool {
    [
        run_dir(store, task, attempt).join("execution.json"),
        attempt_dir(store, task, attempt).join("identity.json"),
        attempt_dir(store, task, attempt).join(START_FILE),
    ]
    .into_iter()
    .find_map(|path| worker_kind_in(&path))
    .is_some_and(|kind| kind == "command")
}

fn worker_kind_in(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("worker")?
        .get("kind")?
        .as_str()
        .map(str::to_owned)
}

/// How many attempts before `attempt` ended in a model wait, newest first, in a row (ADR 0048
/// decision 9). The current attempt's own record is written after this read; the next attempt
/// that is not a model wait breaks the run.
fn consecutive_model_waits(store: &Store, task: &str, attempt: u32) -> u32 {
    let mut run = 0u32;
    let mut number = attempt;
    while number > 1 {
        number -= 1;
        let path = run_dir(store, task, number).join(ATTEMPT_RESULT_FILE);
        let Ok(text) = fs::read_to_string(path) else {
            break;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            break;
        };
        if value.get("model_wait").and_then(Value::as_bool) != Some(true) {
            break;
        }
        run += 1;
    }
    run
}

/// The `retry_at` of a model wait (ADR 0048 decision 9): the review wait's step for `run` waits
/// in a row, the session's own `retry_after_secs` when it is longer, and never more than 60
/// minutes.
fn model_wait_retry_at(prior_waits: u32, model_error: &Value) -> String {
    let step = crate::review::wait_step_secs(prior_waits + 1);
    let seconds = model_error
        .get("retry_after_secs")
        .and_then(Value::as_u64)
        .map_or(step, |secs| secs.max(step))
        .min(crate::review::REVIEW_WAIT_CAP_SECS);
    let at = chrono::Utc::now() + chrono::Duration::seconds(seconds as i64);
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The current attempt's recorded model error, for `show` (ADR 0048 decision 9): the
/// `model_error` object of its `attempt_result.json`, or `None` when the attempt did not wait
/// on its model.
pub fn attempt_model_error(store: &Store, task: &str, attempt: u32) -> Option<Value> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join(ATTEMPT_RESULT_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("model_error")
        .filter(|error| !error.is_null())
        .cloned()
}

/// The `notes` array one record carries, when the file exists, parses, and holds one.
fn notes_in(path: &Path) -> Option<Vec<String>> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let notes: Vec<String> = value
        .get("notes")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    (!notes.is_empty()).then_some(notes)
}

/// The `cost` the last `Result` of one attempt's jsonl stream carries. `None` when the stream
/// has no `Result` at all (a crash, a kill), which is an unknown cost.
fn result_cost(store: &Store, task: &str, attempt: u32) -> Option<ResultCost> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join("result.jsonl")).ok()?;
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("Result") {
            continue;
        }
        let cost = value.get("cost");
        return Some(ResultCost {
            present: cost.is_some(),
            micros: cost
                .and_then(|cost| cost.get("micros"))
                .and_then(Value::as_u64),
        });
    }
    None
}

/// What one `Result` said about cost.
struct ResultCost {
    /// The `cost` key is there. A `Result` without it is an unknown cost.
    present: bool,
    /// `cost.micros`, when it is a number.
    micros: Option<u64>,
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

/// Charges one attempt's `review` stage cost to the task and checks the budgets (ADR 0048
/// decision 12). `cost_usd` is the summed `review_cost_micros` of the attempt's review verdict
/// in US dollars; a review that reported no cost charges 0 and is still checked. `attempt` is
/// the attempt's label (`<task>-<n>`), the id its verdicts are written under.
///
/// When the task's own limit (its `max_cost_usd`, else `[limits] max_cost_usd`) or the plan's
/// `max_cost_usd` is reached, the task moves to `todo.paused_budget` in the same transaction,
/// the caller writes no automatic rework, and a plan budget also sets the plan's `auto` to
/// `off`. Returns whether the task was paused.
///
/// The charge is made only while the task is `review.verifying`: once the verdicts are
/// applied the task has moved, so a second application charges nothing. The charge is
/// recorded in `runs/<task>/review_cost.json` together with the counters it produced, before
/// the task write, so a pass that dies in between is recovered rather than doubled: the next
/// application sees whether the counters already carry the charge and adds it only when the
/// task write did not land.
///
/// # Errors
///
/// The task file changed outside arc, or a file could not be read or written.
pub fn charge_review_cost(
    store: &Store,
    task_id: &str,
    attempt: &str,
    cost_usd: f64,
) -> Result<bool, Error> {
    let (paused, pause_plan) = {
        let _lock = store.lock_state()?;
        let loaded = store.load_task(task_id)?;
        require_unchanged(task_id, loaded.changed_outside_arc)?;
        let mut task = loaded.item;
        if task.state != TaskState::Review || task.phase != "verifying" {
            return Ok(false);
        }
        let landed = read_review_cost(store, task_id).is_some_and(|record| {
            record.attempt == attempt
                && task.cost_usd == Some(record.cost_after)
                && task.total_cost_usd == Some(record.total_after)
        });
        if landed {
            // The task write already carries this attempt's charge; only the state move is
            // left. The budgets are checked again so a charge whose task write landed without
            // its pause (a review that reported no cost) still pauses here.
            let (task_reached, plan_reached) = review_budget_reached(store, &task, task_id)?;
            let paused = task_reached || plan_reached;
            if paused {
                pause_for_budget(&mut task);
                let version = store.next_version(Kind::Task, task_id);
                store.write_item(Kind::Task, task_id, &task.to_text(), version)?;
            }
            (paused, plan_reached)
        } else {
            // Either no record or one from a pass that died before the task write: charge,
            // record the counters, then write the task.
            apply_review_charge(store, &mut task, task_id, attempt, cost_usd)?
        }
    };
    if pause_plan {
        let plan_id = store.load_task(task_id)?.item.plan;
        let _ = store.change_plan(&plan_id, |plan| {
            plan.auto = "off".to_owned();
            Ok(())
        });
    }
    Ok(paused)
}

/// Adds `cost_usd` to the task's counters and writes the record and the task, in that order.
/// The record holds the counters the charge produced, so a later application can tell whether
/// the task write landed. Returns whether the task paused and whether the plan budget was
/// reached.
fn apply_review_charge(
    store: &Store,
    task: &mut model::TaskFile,
    task_id: &str,
    attempt: &str,
    cost_usd: f64,
) -> Result<(bool, bool), Error> {
    task.add_cost(cost_usd);
    let (task_reached, plan_reached) = review_budget_reached(store, task, task_id)?;
    let paused = task_reached || plan_reached;
    if paused {
        pause_for_budget(task);
    }
    let record = ReviewCharge {
        attempt: attempt.to_owned(),
        cost_usd,
        cost_after: task.cost_usd.unwrap_or(0.0),
        total_after: task.total_cost_usd.unwrap_or(0.0),
    };
    // The record goes first, because a repeated application must never charge twice. A task
    // write that fails removes the record again; a pass that dies before the write leaves a
    // record whose counters the task does not carry, and the next application charges again.
    write_review_cost(store, task_id, &record)?;
    let version = store.next_version(Kind::Task, task_id);
    if let Err(error) = store.write_item(Kind::Task, task_id, &task.to_text(), version) {
        // The cost did not land: let the next application charge this attempt.
        clear_review_cost(store, task_id);
        return Err(error);
    }
    Ok((paused, plan_reached))
}

/// Whether the task's own budget (its `max_cost_usd`, else `[limits] max_cost_usd`) and the
/// plan's budget are used up by the task's current counters. A plan file or a sibling task
/// that cannot be read counts as no cost, as the attempt-time check does.
fn review_budget_reached(
    store: &Store,
    task: &model::TaskFile,
    task_id: &str,
) -> Result<(bool, bool), Error> {
    let config = store.config()?;
    let task_limit = task.max_cost_usd.or(config.limits.max_cost_usd);
    let task_reached = task_limit.is_some_and(|limit| task.cost_usd.unwrap_or(0.0) >= limit);
    let plan = store.load_plan(&task.plan)?.item;
    let other_costs: f64 = plan
        .tasks
        .iter()
        .filter(|id| id.as_str() != task_id)
        .map(|id| {
            store
                .load_task(id)
                .map(|task| task.item.cost_usd.unwrap_or(0.0))
                .unwrap_or(0.0)
        })
        .sum();
    let plan_reached = plan
        .max_cost_usd
        .is_some_and(|limit| other_costs + task.cost_usd.unwrap_or(0.0) >= limit);
    Ok((task_reached, plan_reached))
}

fn pause_for_budget(task: &mut model::TaskFile) {
    task.state = TaskState::Todo;
    task.phase = "paused_budget".to_owned();
    task.retry_at = None;
}

fn review_cost_path(store: &Store, task: &str) -> PathBuf {
    store.root().join("runs").join(task).join(REVIEW_COST_FILE)
}

/// One attempt's review charge and the task counters it left behind.
struct ReviewCharge {
    attempt: String,
    cost_usd: f64,
    cost_after: f64,
    total_after: f64,
}

/// The attempt whose review cost arc charged and the counters that charge produced, when it
/// recorded one.
fn read_review_cost(store: &Store, task: &str) -> Option<ReviewCharge> {
    let text = fs::read_to_string(review_cost_path(store, task)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(ReviewCharge {
        attempt: value.get("attempt").and_then(Value::as_str)?.to_owned(),
        cost_usd: value.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        cost_after: value.get("cost_after").and_then(Value::as_f64)?,
        total_after: value.get("total_after").and_then(Value::as_f64)?,
    })
}

fn write_review_cost(store: &Store, task: &str, record: &ReviewCharge) -> Result<(), Error> {
    let path = review_cost_path(store, task);
    let dir = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let text = serde_json::to_string_pretty(&json!({
        "attempt": record.attempt,
        "cost_usd": record.cost_usd,
        "cost_after": record.cost_after,
        "total_after": record.total_after,
    }))
    .map_err(|error| Error::Failed(format!("cannot encode {}: {error}", path.display())))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", temporary.display())))?;
    fs::rename(&temporary, &path)
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))
}

/// Forgets the recorded charge; the next application charges the attempt again. Used when the
/// task write itself failed, so the record does not claim a charge that never landed.
fn clear_review_cost(store: &Store, task: &str) {
    let _ = fs::remove_file(review_cost_path(store, task));
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

/// `reset-budget`'s one write: the budget counter goes back to `0` and the total stays. An
/// older file without `total_cost_usd` gets it materialized from its `cost_usd` first, so the
/// total cannot fall back to the zeroed counter (ADR 0048 decision 12).
fn reset_counted_cost(task: &mut model::TaskFile) {
    task.total_cost_usd = Some(task.running_total());
    task.cost_usd = Some(0.0);
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
            store.refuse_cancelled(id)?;
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
                    reset_counted_cost(task);
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
            store.refuse_cancelled(&task.item.plan)?;
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
                reset_counted_cost(task);
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

/// Whether the task's current attempt is a worker-less reverify attempt (ADR 0048 decisions 9,
/// 10 and 11). The reverify claim writes the attempt's `attempt_result.json` with class
/// [`REVERIFY_CLASS`] before the gate runs, so a failed reverify is told apart from a failed
/// worker attempt by what the store already records, never by timing or log text.
///
/// # Errors
///
/// The task file could not be read.
pub fn is_reverify_attempt(store: &Store, task_id: &str) -> Result<bool, Error> {
    let task = store.load_task(task_id)?.item;
    let path = run_dir(store, task_id, task.attempt).join(ATTEMPT_RESULT_FILE);
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    Ok(value.get("class").and_then(Value::as_str) == Some(REVERIFY_CLASS))
}

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
    let mut record = json!({
        "kind": "auto_rework",
        "task": task_id,
        "plan": plan.item.id,
        "count": count,
        "max_auto_rework": max,
        "text": text,
        "at": super::plan_ops::utc_now(),
        // The plan's `auto` is still on: this record announces a pause that has not been
        // applied yet, and the marker stays until the plan write below has happened. A read
        // in between, the channel's first read included, is the pause's reason (decision 11).
        "pending": true,
    });
    let dir = run_dir(store, task_id, task.attempt);
    write_pause_record(&dir, &record);
    let plan_id = plan.item.id.clone();
    let changed = store.change_plan(&plan_id, |plan| {
        plan.auto = "off".to_owned();
        Ok(())
    });
    // The record no longer announces a pause to come, whether the plan write applied it or
    // failed: in the second case no pause is left to report, so no later read may treat the
    // record's text as a pause's reason.
    record["pending"] = json!(false);
    write_pause_record(&dir, &record);
    changed?;
    Ok(Some(Paused {
        task: task_id.to_owned(),
        plan: plan_id,
        count,
        max_auto_rework: max,
        text,
    }))
}

/// Writes one attempt's `paused.json`. A path arc cannot write never fails the pause itself:
/// the plan still pauses and the reason is still in the returned [`Paused`]; only the reason
/// `show` prints from the record is lost.
fn write_pause_record(dir: &Path, record: &Value) {
    if fs::create_dir_all(dir).is_ok()
        && let Ok(body) = serde_json::to_string_pretty(record)
    {
        let _ = fs::write(dir.join(PAUSED_FILE), format!("{body}\n"));
    }
}

/// The plan `arc` paused through `task`'s current attempt: the `plan` field of the
/// `paused.json` record the automatic rework limit writes. `None` when the attempt has no such
/// record, or it names no plan.
///
/// The record stays on the attempt it was written for, so a later attempt (a rework, a
/// reverify) no longer reports the plan as paused by it. The arc TUI's board reads this to
/// decide whether every task of the plan waits for the owner.
#[must_use]
pub fn paused_plan(store: &Store, task: &str, attempt: u32) -> Option<String> {
    let text = fs::read_to_string(run_dir(store, task, attempt).join(PAUSED_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("plan").and_then(Value::as_str).map(str::to_owned)
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

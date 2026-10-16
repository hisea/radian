//! The lead's task tools (ADR 0030 slice 6, part a): one `ToolProvider` over the
//! [`TaskRegistry`] from slice 4.
//!
//! A **lead** is any interactive session that was handed a task builder; the tools are mounted
//! from the start (ADR 0030 item 5) and the permission layer decides whether they
//! work: a mode gate denies them while the session's mode is `general`, and their `allow` rules
//! answer in `lead` (ADR 0038 decision 1). `/mode` never adds or removes a tool, so the model's
//! tool list is the same in both modes; only the refusal changes. A task session never gets this
//! provider (depth limit 2, ADR 0030 O13).
//!
//! The tools are exactly the lead's part-a surface (ADR 0030 O4, revision 6, revision 7):
//!
//! - `spawn_task` — opens a task agent (ADR 0030 O2/O3). It carries **no** edge-policy field:
//!   the policy command comes only from the user's or a trusted project's configuration, never
//!   from a model tool argument (ADR 0030 item 3).
//! - `wait_task` / `wait_any` — block until a task raises a question or a still-ask approval,
//!   or finishes its turn (ADR 0030: those events arrive **here**, never in the
//!   lead's inbox), or the wait times out (`still_running`). A timeout never cancels the task.
//! - `answer_task`, `deny_task_approval`, `escalate_approval` — the lead's only responses; there
//!   is deliberately no `approve` (ADR 0030 R9).
//! - `task_status`, `list_tasks` — the read-only observer (ADR 0023 U6).
//! - `cancel_task`, `remove_task`.
//!
//! `spawn_task` and `remove_task` are `Exclusive`; the rest are `Shared` (ADR 0030 O4).
//!
//! **What is not here (part b):** `max_tasks` queueing, `heavy_slots`, stall events,
//! `send_agent`, `steer_task` and the verify runner.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    Message, ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure, ToolOutput,
    ToolProvider, ToolSession, ToolSpec,
};
use radian_permissions::ApprovalPolicy;
use radian_session::TurnOutcome;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::observer::{JournalObserver, SessionObserver};
use crate::session::{SessionCommand, SessionFacade};
use crate::task::{EndCommit, Isolation, SpawnRequest, TaskRecord, TaskRegistry};
use crate::task_pending::{TaskPendingContent, TaskPendingView};

/// The committed assistant text a wait result carries is cut here (ADR 0030 R2).
pub const TASK_SUMMARY_LIMIT_CHARS: usize = 4000;

/// Every other bounded text (a `git status`/`git diff --stat`) is cut here.
pub const TASK_DETAIL_LIMIT_CHARS: usize = 4000;

/// The wait a call gets when it names no `timeout_secs` (ADR 0030 R9's 10-minute R9
/// timeout, reused as the lead's own patience).
pub const DEFAULT_WAIT_TIMEOUT_SECS: f64 = 600.0;

/// Parses a `timeout_secs` argument (ADR 0030 O4: seconds, fractions allowed) into a duration.
/// A negative, non-finite or unrepresentable value is an `invalid_arguments` failure. There is no
/// artificial upper bound; the only limit is how large a `Duration` can be.
/// The longest a lead wait may block (24 hours). A model-supplied `timeout_secs` is clamped to
/// this, so `Instant::now() + timeout` can never overflow — a huge value (e.g. `1e15`) would
/// otherwise panic the host, and a model argument must never panic the host. The `still_running`
/// result reports the clamped value.
pub const MAX_WAIT_TIMEOUT_SECS: f64 = 24.0 * 60.0 * 60.0;

/// Parses a `timeout_secs` argument (ADR 0030 O4: seconds, fractions allowed) into a duration,
/// clamped to [`MAX_WAIT_TIMEOUT_SECS`]. Returns the duration and the effective (clamped) seconds,
/// which is what a `still_running` result reports. A negative or non-finite value is an
/// `invalid_arguments` failure.
fn wait_timeout(secs: Option<f64>) -> Result<(Duration, f64), ToolFailure> {
    let secs = secs.unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS);
    if !secs.is_finite() || secs < 0.0 {
        return Err(invalid(format!(
            "`timeout_secs` must be a finite, non-negative number of seconds, not {secs}"
        )));
    }
    let secs = secs.min(MAX_WAIT_TIMEOUT_SECS);
    Ok((Duration::from_secs_f64(secs), secs))
}

/// The lead tool names, in the order the provider advertises them. The host adds these to the
/// session profile as `allow` rules (ADR 0030 slice 6); the mode gate decides whether they run.
pub const LEAD_TOOL_NAMES: [&str; 10] = [
    "spawn_task",
    "wait_task",
    "wait_any",
    "answer_task",
    "deny_task_approval",
    "escalate_approval",
    "task_status",
    "list_tasks",
    "cancel_task",
    "remove_task",
];

fn fail(code: &str, message: impl Into<String>) -> ToolFailure {
    ToolFailure::new(code, message)
}

fn invalid(message: impl Into<String>) -> ToolFailure {
    fail("invalid_arguments", message)
}

/// Deserializes one call's arguments, refusing any field the tool does not name (so a model
/// cannot smuggle `policy` into `spawn_task`, ADR 0030 item 3).
fn parse<T: DeserializeOwned>(call: &ToolCall) -> Result<T, ToolFailure> {
    serde_json::from_value(call.arguments.clone()).map_err(|error| invalid(error.to_string()))
}

/// Cuts `text` to at most `limit` characters (never bytes), returning whether it cut.
fn truncate_chars(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_owned(), false);
    }
    let cut: String = text.chars().take(limit).collect();
    (cut, true)
}

/// One bounded text field: the value (with a truncation marker appended when it was cut) and
/// whether it was cut. The marker lives in the text so a reader that only has the text is told.
fn bounded_field(text: &str, limit: usize) -> (Value, bool) {
    let (mut cut, truncated) = truncate_chars(text, limit);
    if truncated {
        cut.push_str("\n… [truncated]");
    }
    (Value::String(cut), truncated)
}

/// A host `git` call in `dir`, trimmed, or `None` on any failure. Every call goes through the
/// shared helper, so it carries the no-filter overrides (ADR 0039 decision 3).
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

/// The mounted tool specs.
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "spawn_task".into(),
            description: "Start a task agent: a second session with its own git worktree on \
                          `radian/<id>` (or `isolation: shared` for a read-only task in this \
                          workspace), which runs the brief and reports back through `wait_task`/\
                          `wait_any`. Returns the task id and where it runs. The task agent's \
                          inputs are never `User`, so it can never borrow your permissions."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "brief": { "type": "string", "description": "The task's first input; the whole job." },
                    "id": { "type": "string", "description": "Optional task id ([A-Za-z0-9._-]); generated when omitted." },
                    "isolation": {
                        "type": "string",
                        "enum": ["worktree", "shared"],
                        "description": "`worktree` (default) gives the task its own worktree; `shared` is read-only in this workspace."
                    },
                    "label": { "type": "string", "description": "A short label for the task." },
                    "approval": {
                        "type": "string",
                        "enum": ["auto", "on-ask"],
                        "description": "The task's approval default after the hard-deny list: `auto` (default) or `on-ask`."
                    }
                },
                "required": ["brief"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Exclusive,
        },
        ToolSpec {
            name: "wait_task".into(),
            description: "Block until task `id` raises a question or a still-ask approval, or \
                          finishes its turn (including its end-of-task commit), or the timeout \
                          elapses (`still_running`). A timeout never cancels the task."
                .into(),
            input_schema: wait_task_schema(),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "wait_any".into(),
            description:
                "Block until any task in `ids` raises a question or approval, or finishes \
                          its turn; the first one wins and the others keep running. Tasks not in \
                          `ids` are ignored (their events stay pending). On timeout it returns \
                          `still_running`. A timeout never cancels a task."
                    .into(),
            input_schema: wait_any_schema(),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "answer_task".into(),
            description: "Answer a task's `ask_user` question, using the `request_id` a \
                          `wait_task`/`wait_any` question carried."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "request_id": { "type": "string" },
                    "answers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "selected": { "type": "array", "items": { "type": "string" } },
                                "text": { "type": "string" },
                                "other": { "type": "string" },
                                "note": { "type": "string" }
                            },
                            "required": ["id"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["id", "request_id", "answers"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "deny_task_approval".into(),
            description: "Refuse a task's pending approval. This only ever narrows; there is no \
                          way to approve on the human's behalf — use `escalate_approval` to ask."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "request_id": { "type": "string" },
                    "reason": { "type": "string", "description": "Why it is refused; shown to the task." }
                },
                "required": ["id", "request_id"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "escalate_approval".into(),
            description: "Hand a task's pending approval to the human with your summary and \
                          recommendation. Only the human's answer resolves it. A lead never \
                          approves."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "request_id": { "type": "string" },
                    "summary": { "type": "string", "description": "What the task wants and why, in your words." },
                    "recommendation": { "type": "string", "description": "What you advise the human to do." }
                },
                "required": ["id", "request_id", "summary", "recommendation"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "task_status".into(),
            description: "One task's committed state: turn state, current tool, the last event \
                          and last write times, usage, the last committed assistant text (bounded) \
                          and its change summary (bounded)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "list_tasks".into(),
            description: "Every live task with its state and a bounded change summary, so you can \
                          spot overlapping files and pick a merge order."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "cancel_task".into(),
            description: "Cancel a task's current turn (idempotent). Its worktree is kept.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Shared,
        },
        ToolSpec {
            name: "remove_task".into(),
            description: "Close a task and remove its worktree. Refused when the worktree has \
                          uncommitted changes unless `force` is set."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "force": { "type": "boolean", "description": "Discard uncommitted changes." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            concurrency: ToolConcurrency::Exclusive,
        },
    ]
}

/// The `wait_task` schema.
fn wait_task_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "timeout_secs": {
                "type": "number",
                "minimum": 0,
                "description": "How long to wait, in seconds (fractions allowed). Defaults to \
                                600 (10 minutes); clamped to 24 hours."
            }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

/// The `wait_any` schema. `ids` is required and non-empty.
fn wait_any_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ids": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "The task ids to watch; the first of them to finish or ask wins."
            },
            "timeout_secs": {
                "type": "number",
                "minimum": 0,
                "description": "How long to wait, in seconds (fractions allowed). Defaults to \
                                600 (10 minutes); clamped to 24 hours."
            }
        },
        "required": ["ids"],
        "additionalProperties": false
    })
}

/// The lead's tool provider: one per lead session, sharing the session's [`TaskRegistry`].
pub struct LeadTools {
    registry: Arc<TaskRegistry>,
}

impl LeadTools {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

impl ToolProvider for LeadTools {
    type Session = LeadSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(LeadSession {
            registry: Arc::clone(&self.registry),
            specs: specs(),
        })
    }
}

/// One lead session's view of the task tools. It holds only the registry and the specs; the
/// registry owns the tasks, their worktrees and the pending queue.
pub struct LeadSession {
    registry: Arc<TaskRegistry>,
    specs: Vec<ToolSpec>,
}

impl LeadSession {
    /// A lead tool session over an existing registry, **without** opening the session's driver
    /// (ADR 0030 slice 8): the control port runs the same `spawn_task`/`wait_task`/
    /// `task_status` handlers the model's lead tools use. The host's [`LeadTools`] provider
    /// builds the same thing in its `open_session`.
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self {
            specs: specs(),
            registry,
        }
    }
}

impl ToolSession for LeadSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        _call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        // The lead tools need the registry, which the plain `ToolSession` trait cannot carry, so
        // the host provider runs them through [`run`] — exactly like `git_commit`.
        Err(fail(
            "unknown_tool",
            "the lead tools are executed by the host tool provider, not this session",
        ))
    }
}

/// Runs one lead tool call. The host provider calls this directly; the trait `execute` above is
/// never used (it needs the registry, which `ToolSession` cannot carry).
pub async fn run(
    lead: &LeadSession,
    call: &ToolCall,
    cx: ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    match call.name.as_str() {
        "spawn_task" => lead.spawn(call).await,
        "wait_task" => lead.wait_task(call, &cx).await,
        "wait_any" => lead.wait_any(call, &cx).await,
        "answer_task" => lead.answer_task(call),
        "deny_task_approval" => lead.deny_task_approval(call),
        "escalate_approval" => lead.escalate_approval(call),
        "task_status" => lead.task_status(call),
        "list_tasks" => lead.list_tasks(call),
        "cancel_task" => lead.cancel_task(call),
        "remove_task" => lead.remove_task(call).await,
        other => Err(fail(
            "unknown_tool",
            format!("`{other}` is not one of the lead tools"),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    brief: String,
    id: Option<String>,
    isolation: Option<String>,
    label: Option<String>,
    approval: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    id: String,
    timeout_secs: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitAnyArgs {
    ids: Vec<String>,
    timeout_secs: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerArgs {
    id: String,
    request_id: String,
    answers: Vec<radian_coding::QuestionReply>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DenyArgs {
    id: String,
    request_id: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EscalateArgs {
    id: String,
    request_id: String,
    summary: String,
    recommendation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    id: String,
    force: Option<bool>,
}

impl LeadSession {
    async fn spawn(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: SpawnArgs = parse(call)?;
        if args.brief.trim().is_empty() {
            return Err(invalid("`brief` must not be empty"));
        }
        let id = args.id.unwrap_or_else(crate::generate_session_id);
        let isolation = match args.isolation.as_deref() {
            None | Some("worktree") => Isolation::Worktree,
            Some("shared") => Isolation::Shared,
            Some(other) => {
                return Err(invalid(format!(
                    "`isolation` is `worktree` or `shared`, not {other:?}"
                )));
            }
        };
        let approval = match args.approval.as_deref() {
            None => None,
            Some("auto") => Some(ApprovalPolicy::Auto),
            Some("on-ask") => Some(ApprovalPolicy::OnAsk),
            Some(other) => {
                return Err(invalid(format!(
                    "`approval` is `auto` or `on-ask`, not {other:?}"
                )));
            }
        };
        let mut request = SpawnRequest::new(id.clone());
        request.isolation = isolation;
        request.label = args.label;
        request.brief = Some(args.brief);
        request.approval = approval;
        // ADR 0030 item 3: the edge policy command is never a model argument, so
        // `spawn_task` has no way to set one and refuses the field (deny_unknown_fields).
        request.policy = None;
        let record = self
            .registry
            .spawn(request)
            .await
            .map_err(|error| fail("spawn_failed", error.to_string()))?;
        let structured = record_json(&record);
        let worktree = record
            .worktree
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the shared workspace".to_owned());
        Ok(ToolOutput {
            content: vec![agent_core::ContentPart::text(format!(
                "spawned task `{}` ({}) at {worktree}",
                record.id,
                record.isolation.as_str()
            ))],
            structured: Some(structured),
        })
    }

    async fn wait_task(
        &self,
        call: &ToolCall,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let args: WaitArgs = parse(call)?;
        let record = self
            .registry
            .get(&args.id)
            .ok_or_else(|| fail("no_such_task", format!("no task `{}`", args.id)))?;
        let (timeout, timeout_secs) = wait_timeout(args.timeout_secs)?;
        let deadline = Instant::now() + timeout;
        // Subscribe before the first check: an event raised between the check and the wait is
        // never lost (the watch version advanced after the receiver captured it).
        let mut pending = self.registry.pending().subscribe();
        let mut turns = record.subscribe_turns();
        loop {
            if let Some(view) = self.registry.pending_next_for(&args.id) {
                return Ok(event_output(&view));
            }
            if !record.running() && record.last_outcome().is_some() {
                return self.finish(&record).await;
            }
            tokio::select! {
                biased;
                _ = cx.cancellation.cancelled() => {
                    return Err(fail("cancelled", "the wait was cancelled"));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(still_running(&args.id, timeout_secs));
                }
                _ = pending.changed() => {}
                _ = turns.changed() => {}
            }
        }
    }

    async fn wait_any(
        &self,
        call: &ToolCall,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let args: WaitAnyArgs = parse(call)?;
        if args.ids.is_empty() {
            return Err(invalid("`ids` must not be empty"));
        }
        let (timeout, timeout_secs) = wait_timeout(args.timeout_secs)?;
        let deadline = Instant::now() + timeout;
        // Every id must exist before the wait starts (ADR 0030 O4): an unknown id is a failure,
        // not a silent skip. Duplicates are fine; the watch set is deduped.
        let mut records: Vec<Arc<TaskRecord>> = Vec::new();
        for id in &args.ids {
            let record = self
                .registry
                .get(id)
                .ok_or_else(|| fail("no_such_task", format!("no task `{id}`")))?;
            if !records.iter().any(|known| known.id == *id) {
                records.push(record);
            }
        }
        let mut pending = self.registry.pending().subscribe();
        let mut turns: Vec<watch::Receiver<u64>> = records
            .iter()
            .map(|record| record.subscribe_turns())
            .collect();
        loop {
            // Only the `ids` set: a question another task raised stays pending for a later wait.
            if let Some(view) = self.registry.pending_next_in(&args.ids) {
                return Ok(event_output(&view));
            }
            if let Some(record) = records
                .iter()
                .find(|record| !record.running() && record.last_outcome().is_some())
            {
                return self.finish(record).await;
            }
            tokio::select! {
                biased;
                _ = cx.cancellation.cancelled() => {
                    return Err(fail("cancelled", "the wait was cancelled"));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(any_still_running(timeout_secs));
                }
                _ = pending.changed() => {}
                _ = wait_any_turn(&mut turns) => {}
            }
        }
    }

    fn answer_task(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: AnswerArgs = parse(call)?;
        self.registry
            .answer(&args.id, &args.request_id, args.answers)
            .map_err(|error| fail("no_such_request", error.to_string()))?;
        Ok(ToolOutput::text(format!(
            "answered `{}` for task `{}`",
            args.request_id, args.id
        )))
    }

    fn deny_task_approval(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: DenyArgs = parse(call)?;
        self.registry
            .deny_approval(&args.id, &args.request_id, args.reason)
            .map_err(|error| fail("no_such_request", error.to_string()))?;
        Ok(ToolOutput::text(format!(
            "denied `{}` for task `{}`",
            args.request_id, args.id
        )))
    }

    fn escalate_approval(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: EscalateArgs = parse(call)?;
        self.registry
            .escalate_approval(
                &args.id,
                &args.request_id,
                &args.summary,
                &args.recommendation,
            )
            .map_err(|error| fail("no_such_request", error.to_string()))?;
        Ok(ToolOutput::text(format!(
            "escalated `{}` for task `{}` to the human",
            args.request_id, args.id
        )))
    }

    fn task_status(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: StatusArgs = parse(call)?;
        let record = self
            .registry
            .get(&args.id)
            .ok_or_else(|| fail("no_such_task", format!("no task `{}`", args.id)))?;
        let status = observed_status(&record);
        Ok(ToolOutput {
            content: vec![agent_core::ContentPart::text(format!(
                "task `{}`: {}",
                record.id,
                status["state"].as_str().unwrap_or("unknown")
            ))],
            structured: Some(status),
        })
    }

    fn list_tasks(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let _: ListArgs = parse(call)?;
        let tasks: Vec<Value> = self
            .registry
            .ids()
            .iter()
            .filter_map(|id| self.registry.get(id))
            .map(|record| observed_status(&record))
            .collect();
        Ok(ToolOutput {
            content: vec![agent_core::ContentPart::text(format!(
                "{} task(s)",
                tasks.len()
            ))],
            structured: Some(json!({ "tasks": tasks })),
        })
    }

    fn cancel_task(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: CancelArgs = parse(call)?;
        let record = self
            .registry
            .get(&args.id)
            .ok_or_else(|| fail("no_such_task", format!("no task `{}`", args.id)))?;
        record
            .session
            .command(SessionCommand::Cancel)
            .map_err(|error| fail("cancel_failed", error.to_string()))?;
        Ok(ToolOutput::text(format!("cancelled task `{}`", args.id)))
    }

    async fn remove_task(&self, call: &ToolCall) -> Result<ToolOutput, ToolFailure> {
        let args: RemoveArgs = parse(call)?;
        self.registry
            .remove(&args.id, args.force.unwrap_or(false))
            .await
            .map_err(|error| fail("remove_failed", error.to_string()))?;
        Ok(ToolOutput::text(format!("removed task `{}`", args.id)))
    }

    /// Builds the `finished` result: the committed summary (bounded) plus the host-side facts
    /// the runtime fills (ADR 0030 R2): the end-of-task commit, HEAD and cleanliness.
    async fn finish(&self, record: &Arc<TaskRecord>) -> Result<ToolOutput, ToolFailure> {
        let end = record.await_end_commit().await;
        let outcome = record.last_outcome().unwrap_or(TurnOutcome::Unknown);
        let observed = Observed::read(record.session.journal_path());
        let (summary, summary_truncated) = match observed.last_assistant {
            Some(text) => {
                let (mut cut, truncated) = truncate_chars(&text, TASK_SUMMARY_LIMIT_CHARS);
                if truncated {
                    cut.push_str("\n… [truncated]");
                }
                (Some(cut), truncated)
            }
            None => (None, false),
        };
        let (head, branch, clean, status, status_truncated) = worktree_facts(record, &end);
        let no_commit = match &end {
            Some(EndCommit::Committed(_)) => false,
            // No turn ended, no worktree, or nothing to commit: there is no new commit.
            Some(EndCommit::None) | None => true,
            Some(EndCommit::Failed(_)) => true,
        };
        let end_error = match &end {
            Some(EndCommit::Failed(message)) => Some(message.clone()),
            _ => None,
        };
        let mut structured = json!({
            "task": record.id,
            "event": "finished",
            "outcome": outcome,
            "isolation": record.isolation.as_str(),
            "worktree": record.worktree.as_ref().map(|p| p.display().to_string()),
            "branch": branch,
            "head": head,
            "clean": clean,
            "no_commit": no_commit,
            "summary": summary,
            "summary_truncated": summary_truncated,
            "status": status,
            "status_truncated": status_truncated,
        });
        if let Some(error) = end_error {
            structured["end_commit_error"] = Value::String(error);
        }
        if record.isolation == Isolation::Shared {
            structured["note"] =
                Value::String("shared isolation: no branch, no end-of-task commit".into());
        }
        let short = structured["head"]
            .as_str()
            .map(|head| head.get(..8).unwrap_or(head).to_owned())
            .unwrap_or_else(|| "-".to_owned());
        Ok(ToolOutput {
            content: vec![agent_core::ContentPart::text(format!(
                "task `{}` finished ({outcome:?}); head {short}, {}",
                record.id,
                if clean == Some(true) {
                    "clean"
                } else {
                    "dirty or unknown"
                }
            ))],
            structured: Some(structured),
        })
    }
}

/// The `wait_any` wake: resolves when any of the per-task turn watches changes.
async fn wait_any_turn(turns: &mut [watch::Receiver<u64>]) {
    if turns.is_empty() {
        // No tasks: wait forever (the timeout and cancellation arms still fire).
        std::future::pending::<()>().await;
        return;
    }
    let waits: Vec<_> = turns
        .iter_mut()
        .map(|receiver| {
            Box::pin(async move {
                let _ = receiver.changed().await;
            })
        })
        .collect();
    let _ = futures_util::future::select_all(waits).await;
}

/// The observer's committed facts for one task, or the empty default when its journal cannot be
/// read yet.
#[derive(Default)]
struct Observed {
    state: String,
    last_tool: Option<String>,
    last_event_ts: Option<String>,
    last_write_ts: Option<String>,
    last_assistant: Option<String>,
}

impl Observed {
    fn read(path: &Path) -> Self {
        let Ok(mut observer) = JournalObserver::open(path) else {
            return Self {
                state: "unknown".into(),
                ..Self::default()
            };
        };
        let _ = observer.poll();
        let (state, last_tool) = {
            let read = observer.projection();
            (
                read.turn().label().to_owned(),
                read.last_tool().map(|tool| tool.name.clone()),
            )
        };
        let last_assistant = observer
            .history()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant { content, .. } => {
                    let text = content
                        .iter()
                        .filter_map(|part| match part {
                            agent_core::ContentPart::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            });
        Self {
            state,
            last_tool,
            last_event_ts: observer.last_event_ts().map(str::to_owned),
            last_write_ts: observer.last_write_ts().map(str::to_owned),
            last_assistant,
        }
    }
}

/// The `task_status`/`list_tasks` payload for one task: the observer's committed facts plus a
/// bounded `git diff --stat` relative to the branch point (ADR 0030 R8).
fn observed_status(record: &Arc<TaskRecord>) -> Value {
    let observed = Observed::read(record.session.journal_path());
    let (summary, summary_truncated) = match &observed.last_assistant {
        Some(text) => {
            let (cut, truncated) = truncate_chars(text, TASK_SUMMARY_LIMIT_CHARS);
            (Value::String(cut), truncated)
        }
        None => (Value::Null, false),
    };
    let diff = record.worktree.as_ref().and_then(|worktree| {
        let start = record.start_commit.as_deref()?;
        git_text(worktree, &["diff", "--stat", start])
    });
    let (diff_stat, diff_truncated) = match diff {
        Some(text) if !text.is_empty() => bounded_field(&text, TASK_DETAIL_LIMIT_CHARS),
        _ => (Value::Null, false),
    };
    json!({
        "task": record.id,
        "state": observed.state,
        "running": record.running(),
        "outcome": record.last_outcome(),
        "last_tool": observed.last_tool,
        "last_event": observed.last_event_ts,
        "last_write": observed.last_write_ts,
        "summary": summary,
        "summary_truncated": summary_truncated,
        "isolation": record.isolation.as_str(),
        "worktree": record.worktree.as_ref().map(|p| p.display().to_string()),
        "branch": record.branch,
        "label": record.label,
        "diff_stat": diff_stat,
        "diff_stat_truncated": diff_truncated,
    })
}

/// The host-side worktree facts a `finished` result carries: HEAD, branch, cleanliness and the
/// bounded `git status --porcelain`. The end-of-task commit's own values win when it ran.
fn worktree_facts(
    record: &Arc<TaskRecord>,
    end: &Option<EndCommit>,
) -> (Option<String>, Option<String>, Option<bool>, Value, bool) {
    let Some(worktree) = record.worktree.as_ref() else {
        return (None, None, None, Value::Null, false);
    };
    let (mut head, mut branch) = (
        git_text(worktree, &["rev-parse", "HEAD"]),
        record.branch.clone(),
    );
    if let Some(EndCommit::Committed(committed)) = end {
        head = Some(committed.commit.clone());
        branch = Some(committed.branch.clone());
    }
    let status = git_text(
        worktree,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .unwrap_or_default();
    let clean = Some(status.is_empty());
    let (status, truncated) = bounded_field(&status, TASK_DETAIL_LIMIT_CHARS);
    (head, branch, clean, status, truncated)
}

/// The `spawn_task` result payload.
fn record_json(record: &Arc<TaskRecord>) -> Value {
    json!({
        "task": record.id,
        "isolation": record.isolation.as_str(),
        "worktree": record.worktree.as_ref().map(|p| p.display().to_string()),
        "branch": record.branch,
        "label": record.label,
    })
}

/// One `wait_task`/`wait_any` event: a question or a still-ask approval, with its structured
/// content (ADR 0030). Nothing here goes to the lead's inbox.
fn event_output(view: &TaskPendingView) -> ToolOutput {
    match &view.content {
        TaskPendingContent::Question(request) => {
            let questions = serde_json::to_value(&request.questions).unwrap_or(Value::Null);
            let first = request
                .questions
                .first()
                .map(|question| question.prompt.clone())
                .unwrap_or_default();
            let (first, _) = truncate_chars(&first, TASK_DETAIL_LIMIT_CHARS);
            ToolOutput {
                content: vec![agent_core::ContentPart::text(format!(
                    "task `{}` asks: {first}",
                    view.task_id
                ))],
                structured: Some(json!({
                    "task": view.task_id,
                    "event": "question",
                    "request_id": view.request_id,
                    "questions": questions,
                })),
            }
        }
        TaskPendingContent::Approval(request) => {
            let (arguments, _) =
                bounded_field(&request.arguments.to_string(), TASK_DETAIL_LIMIT_CHARS);
            ToolOutput {
                content: vec![agent_core::ContentPart::text(format!(
                    "task `{}` needs an approval for `{}`",
                    view.task_id, request.tool
                ))],
                structured: Some(json!({
                    "task": view.task_id,
                    "event": "approval",
                    "request_id": view.request_id,
                    "tool": request.tool,
                    "resource": request.resource,
                    "resolved_resource": request.resolved_resource,
                    "arguments": arguments,
                    "reason": request.reason,
                })),
            }
        }
    }
}

/// A `wait_task` timeout: the task is untouched and still running.
fn still_running(task: &str, timeout_secs: f64) -> ToolOutput {
    ToolOutput {
        content: vec![agent_core::ContentPart::text(format!(
            "task `{task}` is still running (waited {timeout_secs} s)"
        ))],
        structured: Some(json!({
            "task": task,
            "event": "still_running",
            "timeout_secs": timeout_secs,
        })),
    }
}

/// A `wait_any` timeout: no task in `ids` finished or asked within the wait.
fn any_still_running(timeout_secs: f64) -> ToolOutput {
    ToolOutput {
        content: vec![agent_core::ContentPart::text(format!(
            "no task finished or asked (waited {timeout_secs} s)"
        ))],
        structured: Some(json!({
            "event": "still_running",
            "timeout_secs": timeout_secs,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_specs_carry_the_lead_tool_names_and_the_right_concurrency() {
        let specs = specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, LEAD_TOOL_NAMES);
        for spec in &specs {
            let expected = if matches!(spec.name.as_str(), "spawn_task" | "remove_task") {
                ToolConcurrency::Exclusive
            } else {
                ToolConcurrency::Shared
            };
            assert_eq!(spec.concurrency, expected, "{}", spec.name);
        }
    }

    #[test]
    fn spawn_task_refuses_a_policy_argument() {
        // ADR 0030 item 3: the edge policy command is never a model argument.
        let call = ToolCall {
            id: "c1".into(),
            name: "spawn_task".into(),
            arguments: json!({"brief": "x", "policy": "/bin/sh -c evil"}),
        };
        let refused = parse::<SpawnArgs>(&call).is_err();
        assert!(refused, "a policy argument must be refused");
        // The same call without `policy` parses.
        let ok = ToolCall {
            id: "c2".into(),
            name: "spawn_task".into(),
            arguments: json!({"brief": "x"}),
        };
        assert!(parse::<SpawnArgs>(&ok).is_ok());
    }

    #[test]
    fn a_summary_over_the_limit_is_cut_and_says_so() {
        let long = "x".repeat(TASK_SUMMARY_LIMIT_CHARS + 10);
        let (cut, truncated) = truncate_chars(&long, TASK_SUMMARY_LIMIT_CHARS);
        assert!(truncated);
        assert_eq!(cut.chars().count(), TASK_SUMMARY_LIMIT_CHARS);
        let (text, truncated) = bounded_field(&long, TASK_SUMMARY_LIMIT_CHARS);
        assert!(truncated);
        assert!(text.as_str().unwrap().contains("[truncated]"));
    }

    #[test]
    fn a_huge_timeout_is_clamped_to_a_day() {
        let (duration, secs) = wait_timeout(Some(1e15)).expect("finite");
        assert_eq!(secs, MAX_WAIT_TIMEOUT_SECS);
        assert_eq!(duration, Duration::from_secs(86_400));
        // The default and a fraction are not perturbed.
        let (duration, secs) = wait_timeout(None).expect("the default");
        assert_eq!(secs, DEFAULT_WAIT_TIMEOUT_SECS);
        assert_eq!(duration, Duration::from_secs(600));
        let (_, secs) = wait_timeout(Some(0.02)).expect("a fraction");
        assert_eq!(secs, 0.02);
    }
}

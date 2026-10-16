//! The task-side pending queue (ADR 0030 slice 5, revision 6).
//!
//! A task agent never appears in the human's queue. Its `ask_user` questions and the approvals
//! that are still `ask` after the three layers (ADR 0030 R1) become **pending task
//! events** here. The task's tool call blocks until the lead resolves the event:
//!
//! - [`TaskPendingQueue::answer`] answers a question;
//! - [`TaskPendingQueue::deny_approval`] refuses an approval (it only ever narrows);
//! - [`TaskPendingQueue::escalate_approval`] hands the approval to the **human** queue
//!   (ADR 0023 U7) with the lead's summary and recommendation; the human's answer resolves the
//!   task.
//!
//! There is deliberately **no operation that approves an approval on the lead's behalf**
//! (ADR 0012: approval belongs to the human; ADR 0030 R9). The only way a task
//! approval becomes `allow` is the human answering the escalated request.
//!
//! # The R9 timeout
//!
//! The timeout (default 10 minutes, configurable with [`TaskPendingQueue::set_timeout`]) starts
//! when the lead **takes** the event ([`TaskPendingQueue::next`]), not when the task raises it
//! (ADR 0030: it is measured from the moment the event is handed over).
//! On timeout the approval is denied (a question fails as unanswered) and the task is told.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::future::BoxFuture;
use radian_coding::{PromptError, Prompter, QuestionAnswer, QuestionReply, QuestionRequest};
use radian_permissions::{
    ApprovalError, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Approver,
};
use radian_sandbox::Sandbox;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::sync::{oneshot, watch};

use crate::pending::{PendingRequest, PendingResponse};
use crate::session::SessionQueue;

/// The default R9 timeout: 10 minutes (ADR 0030 R9 item 2, revision 6 decision 3).
pub const DEFAULT_PENDING_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The default timeout for one edge-policy command run (ADR 0030 R1 layer 2): a
/// policy command that hangs is `ask`, not a hung turn.
pub const DEFAULT_POLICY_TIMEOUT: Duration = Duration::from_secs(5);

/// Which kind of task event is pending. Mirrors the structured content so a lead can branch
/// without matching the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPendingKind {
    Question,
    Approval,
}

/// The structured content of a pending task event (ADR 0030): the
/// question, or the approval's tool, arguments and the task's reason.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskPendingContent {
    Question(QuestionRequest),
    Approval(ApprovalRequest),
}

impl TaskPendingContent {
    pub fn kind(&self) -> TaskPendingKind {
        match self {
            TaskPendingContent::Question(_) => TaskPendingKind::Question,
            TaskPendingContent::Approval(_) => TaskPendingKind::Approval,
        }
    }
}

/// One pending task event as the lead sees it through [`TaskPendingQueue::next`].
#[derive(Debug, Clone, PartialEq)]
pub struct TaskPendingView {
    pub task_id: String,
    pub request_id: String,
    pub content: TaskPendingContent,
}

impl TaskPendingView {
    pub fn kind(&self) -> TaskPendingKind {
        self.content.kind()
    }
}

/// Why a resolution operation did not complete a task event: it was never raised (or a
/// question/approval was answered with the wrong operation), or it was already resolved,
/// escalated or abandoned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no pending {kind} for task `{task_id}` with request id `{request_id}`")]
pub struct TaskPendingError {
    pub kind: &'static str,
    pub task_id: String,
    pub request_id: String,
}

/// What a task's blocked call is waiting for.
#[derive(Debug, Clone, PartialEq)]
enum Resolution {
    Approval(ApprovalOutcome),
    Question(Result<QuestionAnswer, PromptError>),
}

struct Entry {
    task_id: String,
    request_id: String,
    content: TaskPendingContent,
    reply: Option<oneshot::Sender<Resolution>>,
    /// The lead has taken this event (`next` returned it); the R9 timeout is running.
    taken: bool,
}

struct State {
    entries: VecDeque<Entry>,
    last_issued: u64,
}

/// The shared state a spawned timeout task holds. `Arc` so the task outlives the `&self` borrow
/// that started it.
struct Inner {
    /// The **human** queue (ADR 0023 U7). Only `escalate_approval` writes here.
    human: Option<Arc<SessionQueue>>,
    state: Mutex<State>,
    timeout_millis: AtomicU64,
    /// Bumped on every change that could alter the head or the count, so a lead's `wait_*`
    /// (slice 6) can wake instead of polling.
    version: watch::Sender<u64>,
}

/// The task-side pending queue. One per [`crate::task::TaskRegistry`], shared by every task it
/// spawned; entries carry the task id.
pub struct TaskPendingQueue {
    inner: Arc<Inner>,
}

impl TaskPendingQueue {
    /// Builds a queue. `human` is where [`TaskPendingQueue::escalate_approval`] sends an
    /// escalated approval; `None` refuses escalation.
    pub fn new(human: Option<Arc<SessionQueue>>) -> Arc<Self> {
        let (version, _) = watch::channel(0u64);
        Arc::new(Self {
            inner: Arc::new(Inner {
                human,
                state: Mutex::new(State {
                    entries: VecDeque::new(),
                    last_issued: 0,
                }),
                timeout_millis: AtomicU64::new(DEFAULT_PENDING_TIMEOUT.as_millis() as u64),
                version,
            }),
        })
    }

    /// Sets the R9 timeout (ADR 0030). A zero timeout fires immediately.
    pub fn set_timeout(&self, timeout: Duration) {
        self.inner
            .timeout_millis
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
    }

    /// The current R9 timeout.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.inner.timeout_millis.load(Ordering::SeqCst))
    }

    /// A receiver that changes once per queue change (coalesced): the lead's `wait_*` reads the
    /// next event, then waits on this instead of polling.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.inner
            .version
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn next_id(&self, state: &mut State) -> String {
        state.last_issued += 1;
        format!("task-req-{}", state.last_issued)
    }

    /// Raises a question and waits for the lead to answer (or for the timeout).
    pub async fn question(
        &self,
        task_id: String,
        request: QuestionRequest,
    ) -> Result<QuestionAnswer, PromptError> {
        let (reply, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            let request_id = self.next_id(&mut state);
            state.entries.push_back(Entry {
                task_id,
                request_id: request_id.clone(),
                content: TaskPendingContent::Question(request),
                reply: Some(reply),
                taken: false,
            });
        }
        self.bump();
        match rx.await {
            Ok(Resolution::Question(result)) => result,
            // The queue (or the session) was dropped, or an approval resolution reached a
            // question: the lead is gone.
            _ => Err(PromptError::Gone),
        }
    }

    /// Raises an approval and waits for the lead to deny, escalate (the human decides), or for
    /// the timeout. There is no path here that allows on the lead's behalf.
    pub async fn approval(
        &self,
        task_id: String,
        request: ApprovalRequest,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        let (reply, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            let request_id = self.next_id(&mut state);
            state.entries.push_back(Entry {
                task_id,
                request_id,
                content: TaskPendingContent::Approval(request),
                reply: Some(reply),
                taken: false,
            });
        }
        self.bump();
        match rx.await {
            Ok(Resolution::Approval(outcome)) => Ok(outcome),
            _ => Err(ApprovalError("the task approval was abandoned".into())),
        }
    }

    /// Takes the next not-yet-taken event, marks it taken, and starts its R9 timeout
    /// (ADR 0030). Returns `None` when nothing is waiting. Entries whose
    /// waiting end is gone are dropped first, so a cancelled task's event never blocks the lead.
    pub fn next(&self) -> Option<TaskPendingView> {
        let view = {
            let mut state = self.lock();
            retain_open(&mut state);
            let position = state.entries.iter().position(|entry| !entry.taken)?;
            let entry = &mut state.entries[position];
            entry.taken = true;
            TaskPendingView {
                task_id: entry.task_id.clone(),
                request_id: entry.request_id.clone(),
                content: entry.content.clone(),
            }
        };
        self.bump();
        self.start_timeout(&view.task_id, &view.request_id);
        Some(view)
    }

    /// The head of the queue without taking it (a read-only peek for a UI or a test).
    pub fn peek(&self) -> Option<TaskPendingView> {
        let mut state = self.lock();
        let changed = retain_open(&mut state);
        let view = state
            .entries
            .iter()
            .find(|entry| !entry.taken)
            .map(|entry| TaskPendingView {
                task_id: entry.task_id.clone(),
                request_id: entry.request_id.clone(),
                content: entry.content.clone(),
            });
        if changed {
            drop(state);
            self.bump();
        }
        view
    }

    /// The number of live events that have not been taken.
    pub fn len(&self) -> usize {
        let mut state = self.lock();
        let changed = retain_open(&mut state);
        let len = state.entries.iter().filter(|entry| !entry.taken).count();
        if changed {
            drop(state);
            self.bump();
        }
        len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn start_timeout(&self, task_id: &str, request_id: &str) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return; // no runtime: the caller is not driving async work, so no timeout
        };
        let inner = Arc::clone(&self.inner);
        let task_id = task_id.to_owned();
        let request_id = request_id.to_owned();
        let timeout = self.timeout();
        handle.spawn(async move {
            tokio::time::sleep(timeout).await;
            timeout_resolve(&inner, &task_id, &request_id);
        });
    }

    /// Answers a pending question. `answers` are checked by the `ask_user` tool, not here.
    pub fn answer(
        &self,
        task_id: &str,
        request_id: &str,
        answers: Vec<QuestionReply>,
    ) -> Result<(), TaskPendingError> {
        resolve(
            &self.inner,
            task_id,
            request_id,
            Resolution::Question(Ok(QuestionAnswer::new(request_id, answers))),
        )
    }

    /// Refuses a pending approval: it only ever narrows (ADR 0030 R9 item 2). There
    /// is no `approve`.
    pub fn deny_approval(
        &self,
        task_id: &str,
        request_id: &str,
        reason: Option<String>,
    ) -> Result<(), TaskPendingError> {
        resolve(
            &self.inner,
            task_id,
            request_id,
            Resolution::Approval(ApprovalOutcome::Deny {
                reason: Some(reason.unwrap_or_else(|| "denied by the lead".into())),
            }),
        )
    }

    /// Hands a pending approval to the **human** queue with the lead's summary and
    /// recommendation (ADR 0030 R9 item 2). The lead cannot approve; only the
    /// human's answer resolves the task.
    pub fn escalate_approval(
        &self,
        task_id: &str,
        request_id: &str,
        summary: &str,
        recommendation: &str,
    ) -> Result<(), TaskPendingError> {
        let Some(human) = self.inner.human.clone() else {
            return Err(TaskPendingError {
                kind: "human queue",
                task_id: task_id.to_owned(),
                request_id: request_id.to_owned(),
            });
        };
        // Check the runtime first: a failure after the entry is taken would leave the task
        // blocked with the event out of the queue.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Err(TaskPendingError {
                kind: "approval",
                task_id: task_id.to_owned(),
                request_id: request_id.to_owned(),
            });
        };
        // Take the entry out with its waiting end, so only the human's answer resolves it. The
        // entry leaves the task queue here; an escalated request is no longer the lead's.
        let reply = {
            let mut state = self.lock();
            let Some(position) = state.entries.iter().position(|entry| {
                entry.task_id == task_id
                    && entry.request_id == request_id
                    && entry.reply.is_some()
                    && entry.content.kind() == TaskPendingKind::Approval
            }) else {
                return Err(TaskPendingError {
                    kind: "approval",
                    task_id: task_id.to_owned(),
                    request_id: request_id.to_owned(),
                });
            };
            let request = match &state.entries[position].content {
                TaskPendingContent::Approval(request) => request.clone(),
                TaskPendingContent::Question(_) => unreachable!("kind checked"),
            };
            let escalated = ApprovalRequest::new(
                request.request_id.clone(),
                request.call_id.clone(),
                request.tool.clone(),
                request.resource.clone(),
                request.resolved_resource.clone(),
                request.arguments.clone(),
                Some(format!(
                    "{summary}\n\nlead recommendation: {recommendation}"
                )),
            );
            let mut entry = state.entries.remove(position).expect("just found");
            drop(state);
            let (_, answer) = human.push(task_id.to_owned(), PendingRequest::Approval(escalated));
            entry.reply.take().map(|reply| (reply, answer))
        };
        let Some((reply, answer)) = reply else {
            return Err(TaskPendingError {
                kind: "approval",
                task_id: task_id.to_owned(),
                request_id: request_id.to_owned(),
            });
        };
        // Forward the human's answer to the task, off the lead's thread.
        handle.spawn(async move {
            let outcome = match answer.wait().await {
                Ok(PendingResponse::Approval(outcome)) => outcome,
                _ => ApprovalOutcome::Deny {
                    reason: Some("the human queue was abandoned".into()),
                },
            };
            let _ = reply.send(Resolution::Approval(outcome));
        });
        self.bump();
        Ok(())
    }
}

/// Completes one entry and wakes the task, or reports that there is nothing to complete.
fn resolve(
    inner: &Arc<Inner>,
    task_id: &str,
    request_id: &str,
    resolution: Resolution,
) -> Result<(), TaskPendingError> {
    let entry = {
        let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let position = state.entries.iter().position(|entry| {
            entry.task_id == task_id
                && entry.request_id == request_id
                && entry.reply.is_some()
                && matches!(
                    (&entry.content, &resolution),
                    (TaskPendingContent::Approval(_), Resolution::Approval(_))
                        | (TaskPendingContent::Question(_), Resolution::Question(_))
                )
        });
        match position {
            Some(position) => state.entries.remove(position),
            None => {
                return Err(TaskPendingError {
                    kind: match resolution {
                        Resolution::Approval(_) => "approval",
                        Resolution::Question(_) => "question",
                    },
                    task_id: task_id.to_owned(),
                    request_id: request_id.to_owned(),
                });
            }
        }
    };
    let result = match entry.and_then(|mut entry| entry.reply.take()) {
        Some(reply) => reply.send(resolution).map_err(|_| ()),
        None => Err(()),
    };
    inner
        .version
        .send_modify(|version| *version = version.wrapping_add(1));
    match result {
        Ok(()) => Ok(()),
        Err(()) => Err(TaskPendingError {
            kind: "request",
            task_id: task_id.to_owned(),
            request_id: request_id.to_owned(),
        }),
    }
}

/// The R9 timeout completing one taken entry (ADR 0030 R9 item 2, revision 6
/// decision 3): an approval is denied, a question fails as unanswered, and the task is told.
/// A no-op when the entry is already resolved, escalated or gone.
fn timeout_resolve(inner: &Arc<Inner>, task_id: &str, request_id: &str) {
    let resolution = {
        let state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(position) = state.entries.iter().position(|entry| {
            entry.task_id == task_id && entry.request_id == request_id && entry.reply.is_some()
        }) else {
            return;
        };
        match &state.entries[position].content {
            TaskPendingContent::Approval(_) => Resolution::Approval(ApprovalOutcome::Deny {
                reason: Some("the task approval timed out (ADR 0030 R9)".into()),
            }),
            TaskPendingContent::Question(_) => Resolution::Question(Err(PromptError::Failed(
                "the lead did not answer before the timeout (ADR 0030 R9)".into(),
            ))),
        }
    };
    let _ = resolve(inner, task_id, request_id, resolution);
}

/// Drops entries whose waiting end is gone; returns whether anything was dropped.
fn retain_open(state: &mut State) -> bool {
    let before = state.entries.len();
    state
        .entries
        .retain(|entry| entry.reply.as_ref().is_some_and(|reply| !reply.is_closed()));
    before != state.entries.len()
}

// ---------------------------------------------------------------------------
// The approver and the prompter a task session is handed.
// ---------------------------------------------------------------------------

/// An optional edge policy command (ADR 0030 R1 layer 2): it is run with the pending
/// request JSON on stdin and answers `allow`, `deny` or `ask` on stdout. `name` is what the
/// audit records as the policy (`approver = "policy"`, ADR 0006 §5.9).
#[derive(Debug, Clone)]
pub struct PolicyCommand {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub timeout: Duration,
}

impl PolicyCommand {
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            timeout: DEFAULT_POLICY_TIMEOUT,
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// The three-layer resolver a task session uses as its `Approver` (ADR 0030 R1).
///
/// Layer 1 (hard-deny) is already structural: those are `deny` rules on the task profile, so the
/// permission hook returns `Deny` before this is ever called, and nothing here can relax them.
/// Layer 2 is the optional [`PolicyCommand`]. Layer 3 is the task default: `auto` allows, and
/// `on-ask` raises a pending task event; a policy that says `ask` (or times out, exits non-zero,
/// or prints something unparseable) always raises the event, never `auto`.
pub struct TaskApprover {
    queue: Arc<TaskPendingQueue>,
    task_id: String,
    mode: ApprovalPolicy,
    policy: Option<PolicyCommand>,
    sandbox: Option<Sandbox>,
}

impl TaskApprover {
    /// `mode` is the task's default after the edge policy: `Auto` or `OnAsk`. Any other value is
    /// treated as `OnAsk` (fail closed: never silently allow).
    pub fn new(
        queue: Arc<TaskPendingQueue>,
        task_id: impl Into<String>,
        mode: ApprovalPolicy,
        policy: Option<PolicyCommand>,
        sandbox: Option<Sandbox>,
    ) -> Self {
        Self {
            queue,
            task_id: task_id.into(),
            mode,
            policy,
            sandbox,
        }
    }
}

impl Approver for TaskApprover {
    fn decide(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async move {
            if let Some(policy) = &self.policy {
                match run_policy(policy, &request, self.sandbox.as_ref()).await {
                    PolicyVerdict::Allow => {
                        return Ok(ApprovalOutcome::Policy {
                            allow: true,
                            name: policy.name.clone(),
                            reason: Some("allowed by the edge policy".into()),
                        });
                    }
                    PolicyVerdict::Deny(reason) => {
                        return Ok(ApprovalOutcome::Policy {
                            allow: false,
                            name: policy.name.clone(),
                            reason: Some(reason),
                        });
                    }
                    // `ask`, a timeout, a non-zero exit or unparseable output: still ask.
                    PolicyVerdict::Ask => {}
                }
            } else if self.mode == ApprovalPolicy::Auto {
                return Ok(ApprovalOutcome::AllowOnce);
            }
            self.queue
                .approval(self.task_id.clone(), request)
                .await
                .map_err(|_| ApprovalError("the task approval was abandoned".into()))
        })
    }
}

/// The question channel a task session uses: `ask_user` becomes a pending task event, never a
/// human prompt (ADR 0030 R9 item 1).
pub struct TaskPrompter {
    queue: Arc<TaskPendingQueue>,
    task_id: String,
}

impl TaskPrompter {
    pub fn new(queue: Arc<TaskPendingQueue>, task_id: impl Into<String>) -> Self {
        Self {
            queue,
            task_id: task_id.into(),
        }
    }
}

impl Prompter for TaskPrompter {
    fn ask(&self, request: QuestionRequest) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>> {
        Box::pin(async move { self.queue.question(self.task_id.clone(), request).await })
    }
}

/// What a policy command said.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PolicyVerdict {
    Allow,
    Deny(String),
    Ask,
}

/// The JSON shape a policy command reads on stdin. `ApprovalRequest` is not `Serialize`, and the
/// wire shape is deliberately explicit (not the internal struct).
#[derive(Serialize)]
struct PolicyRequest<'a> {
    request_id: &'a str,
    call_id: &'a str,
    tool: &'a str,
    resource: &'a str,
    resolved_resource: &'a str,
    arguments: &'a Value,
    reason: Option<&'a str>,
}

/// Runs the edge policy command under the task's sandbox (ADR 0030 R1 layer 2) and
/// maps its stdout. Any failure — a spawn error, a timeout, a non-zero exit, an unreadable
/// stdout, or a word other than `allow`/`deny`/`ask` — is `Ask` (never `allow`).
async fn run_policy(
    policy: &PolicyCommand,
    request: &ApprovalRequest,
    sandbox: Option<&Sandbox>,
) -> PolicyVerdict {
    let payload = match serde_json::to_vec(&PolicyRequest {
        request_id: &request.request_id,
        call_id: &request.call_id,
        tool: &request.tool,
        resource: &request.resource,
        resolved_resource: &request.resolved_resource,
        arguments: &request.arguments,
        reason: request.reason.as_deref(),
    }) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(_) => return PolicyVerdict::Ask,
    };
    let mut command = match sandbox {
        Some(sandbox) => match sandbox.command(&policy.program, policy.args.iter()) {
            Ok(command) => command,
            Err(_) => return PolicyVerdict::Ask,
        },
        None => {
            let mut command = std::process::Command::new(&policy.program);
            command.args(&policy.args);
            command
        }
    };
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match TokioCommand::from(command).kill_on_drop(true).spawn() {
        Ok(child) => child,
        Err(_) => return PolicyVerdict::Ask,
    };
    // Keep the write end open across the wait: a command that blocks on a later read is the
    // deterministic "hangs" case (no `sleep` in the test). The payload ends in `\n`, so a
    // one-line `read` still returns.
    let mut stdin = child.stdin.take();
    if let Some(stdin) = stdin.as_mut() {
        let _ = stdin.write_all(&payload).await;
        let _ = stdin.flush().await;
    }
    let output = match tokio::time::timeout(policy.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        // A timeout: the child is killed by `kill_on_drop` when the future is dropped.
        _ => return PolicyVerdict::Ask,
    };
    drop(stdin);
    if !output.status.success() {
        return PolicyVerdict::Ask;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "allow" => PolicyVerdict::Allow,
        "deny" => PolicyVerdict::Deny("denied by the edge policy".into()),
        _ => PolicyVerdict::Ask,
    }
}

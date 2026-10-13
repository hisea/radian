use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentSession};
use crate::budget::{self, BudgetStatus};
use crate::event::{AgentEvent, EventSink};
use crate::hooks::{HookContext, HookError, HookPoint, Hooks, ToolDecision};
use crate::journal::{JournalError, JournalRecord, TranscriptJournal};
use crate::message::{ContentPart, Message, MessageMeta, ToolCall, ToolResult};
use crate::model::{Model, ModelContext, ModelRequest, ModelResponse, StopReason};
use crate::outcome::{CancelReason, IncompleteReason, RunError, RunOutcome, RunStatus, Step};
use crate::preflight::ToolRegistry;
use crate::steering::{SteerMode, SteeringHandle};
use crate::tool::{ToolConcurrency, ToolContext, ToolProvider, ToolSession};

/// Default number of `Shared` tool calls that may run at once.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;

/// Default time a tool gets to wind down after cancellation before its future is dropped.
pub const DEFAULT_TOOL_CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct TurnOptions {
    /// When reached, the turn is cancelled through the normal path and ends as
    /// `Cancelled(DeadlineExceeded)`. Use this instead of wrapping `step()` in a timeout.
    pub deadline: Option<Instant>,
    pub steer_mode: SteerMode,
    /// 1 runs every tool call serially.
    pub max_parallel_tools: usize,
    /// Overrides `AgentConfig::max_model_calls` for this turn.
    pub max_model_calls: Option<u32>,
    /// After cancellation, how long a running tool may keep going to clean up (for MCP:
    /// send the cancel notification, then stop and reap the server). After this, the tool's
    /// future is dropped.
    pub tool_cancel_grace: Duration,
}

impl Default for TurnOptions {
    fn default() -> Self {
        Self {
            deadline: None,
            steer_mode: SteerMode::default(),
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            max_model_calls: None,
            tool_cancel_grace: DEFAULT_TOOL_CANCEL_GRACE,
        }
    }
}

/// Cancels a running turn from any task. Keep awaiting `step()`/`run()` afterwards; the turn
/// finishes as `Cancelled` once in-flight work has stopped.
#[derive(Debug, Clone)]
pub struct CancellationHandle(CancellationToken);

impl CancellationHandle {
    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Explicit cancellation or the turn deadline, whichever comes first.
#[derive(Clone)]
struct Interrupt {
    token: CancellationToken,
    deadline: Option<Instant>,
}

impl Interrupt {
    fn check(&self) -> Option<CancelReason> {
        if self.token.is_cancelled() {
            Some(CancelReason::Requested)
        } else if self.deadline.is_some_and(|d| Instant::now() >= d) {
            Some(CancelReason::DeadlineExceeded)
        } else {
            None
        }
    }

    async fn wait(&self) -> CancelReason {
        match self.deadline {
            Some(deadline) => tokio::select! {
                biased;
                () = self.token.cancelled() => CancelReason::Requested,
                () = tokio::time::sleep_until(deadline) => CancelReason::DeadlineExceeded,
            },
            None => {
                self.token.cancelled().await;
                CancelReason::Requested
            }
        }
    }
}

enum Raced<T> {
    Done(T),
    Interrupted(CancelReason),
}

async fn race<T>(interrupt: &Interrupt, work: impl Future<Output = T>) -> Raced<T> {
    tokio::select! {
        biased;
        reason = interrupt.wait() => Raced::Interrupted(reason),
        value = work => Raced::Done(value),
    }
}

/// The tool calls of one model response and their results, which are written to the
/// transcript in call order as soon as every earlier result is known.
struct Batch {
    calls: Vec<ToolCall>,
    slots: Vec<Option<ToolResult>>,
    committed: usize,
}

impl Batch {
    fn new(calls: Vec<ToolCall>) -> Self {
        let slots = calls.iter().map(|_| None).collect();
        Self {
            calls,
            slots,
            committed: 0,
        }
    }

    fn is_resolved(&self, i: usize) -> bool {
        i < self.committed || self.slots[i].is_some()
    }

    fn set(&mut self, i: usize, result: ToolResult) {
        if !self.is_resolved(i) {
            self.slots[i] = Some(result);
        }
    }

    fn commit_ready(&mut self, transcript: &mut Vec<Message>) {
        while let Some(result) = self.slots.get_mut(self.committed).and_then(Option::take) {
            transcript.push(Message::Tool {
                result,
                meta: MessageMeta::default(),
            });
            self.committed += 1;
        }
    }

    /// The next result in call order that is ready to leave the batch, if any. The caller
    /// commits it through the journal; `commit_ready` above stays for the Drop path, which
    /// must not touch the journal (D7).
    fn next_ready(&mut self) -> Option<ToolResult> {
        let result = self.slots.get_mut(self.committed).and_then(Option::take)?;
        self.committed += 1;
        Some(result)
    }

    fn unresolved(&self) -> Vec<usize> {
        (self.committed..self.calls.len())
            .filter(|&i| self.slots[i].is_none())
            .collect()
    }

    fn is_complete(&self) -> bool {
        self.committed == self.calls.len()
    }
}

/// One user turn. Borrows its `AgentSession`, so a session runs one turn at a time.
pub struct ActiveRun<'s, M: Model, P: ToolProvider, H: Hooks> {
    session: &'s mut AgentSession<M, P, H>,
    agent: Arc<Agent<M, P, H>>,
    options: TurnOptions,
    interrupt: Interrupt,
    steering: SteeringHandle,
    turn_start: usize,
    /// The turn's user input, committed to the journal and transcript at the first `step()`
    /// (D9): `start_turn` stays synchronous, so the input cannot be journaled there.
    pending_input: Option<Message>,
    model_calls: u32,
    max_model_calls: u32,
    started: bool,
    batch: Option<Batch>,
    outcome: Option<RunOutcome>,
}

impl<'s, M: Model, P: ToolProvider, H: Hooks> ActiveRun<'s, M, P, H> {
    pub(crate) fn new(
        session: &'s mut AgentSession<M, P, H>,
        options: TurnOptions,
        input: Message,
    ) -> Self {
        let agent = Arc::clone(&session.agent);
        let max_model_calls = options
            .max_model_calls
            .unwrap_or(agent.config.max_model_calls);
        // The input is still pending; once committed it lands at this index and everything
        // from here on belongs to this turn.
        let turn_start = session.transcript.len();
        Self {
            interrupt: Interrupt {
                token: CancellationToken::new(),
                deadline: options.deadline,
            },
            session,
            agent,
            options,
            steering: SteeringHandle::default(),
            turn_start,
            pending_input: Some(input),
            model_calls: 0,
            max_model_calls,
            started: false,
            batch: None,
            outcome: None,
        }
    }

    pub fn cancellation_handle(&self) -> CancellationHandle {
        CancellationHandle(self.interrupt.token.clone())
    }

    pub fn steering_handle(&self) -> SteeringHandle {
        self.steering.clone()
    }

    /// Set once the turn reaches a terminal state, including `Failed` and `Poisoned`.
    pub fn outcome(&self) -> Option<&RunOutcome> {
        self.outcome.as_ref()
    }

    pub fn model_calls(&self) -> u32 {
        self.model_calls
    }

    pub fn transcript(&self) -> &[Message] {
        &self.session.transcript
    }

    /// One model response and the tool calls it requested.
    ///
    /// The returned future must not be dropped once polled: that poisons the turn and the
    /// session. To stop early, call `CancellationHandle::cancel()` and keep awaiting.
    pub async fn step(&mut self, events: &dyn EventSink) -> Result<Step, RunError> {
        self.ensure_live()?;
        let mut guard = StepGuard {
            run: self,
            events,
            armed: true,
        };
        let result = guard.step_once().await;
        guard.armed = false;
        result
    }

    /// Calls `step()` until the turn ends. The same drop rule applies.
    pub async fn run(&mut self, events: &dyn EventSink) -> Result<RunOutcome, RunError> {
        loop {
            if let Step::Finished(outcome) = self.step(events).await? {
                return Ok(outcome);
            }
        }
    }

    fn ensure_live(&self) -> Result<(), RunError> {
        match &self.outcome {
            Some(o) if o.status == RunStatus::Poisoned => Err(RunError::Poisoned),
            Some(_) => Err(RunError::Finished),
            None if self.session.poisoned => Err(RunError::Poisoned),
            None => Ok(()),
        }
    }

    fn hook_context(&self) -> HookContext {
        HookContext {
            cancellation: self.interrupt.token.clone(),
            model_calls: self.model_calls,
        }
    }

    /// Commits each steering message through the journal (S3): if the j-th fails, the first
    /// j-1 are journaled and in memory, the rest are not, and memory still equals the journal.
    /// The failed message and those behind it go back to the queue, so the turn's outcome and
    /// `history_for_resume` hand them to the host.
    async fn inject_steering(&mut self, events: &dyn EventSink) -> Result<(), RunError> {
        let mut pending: VecDeque<Message> = self.steering.drain().into();
        let mut applied = 0;
        while let Some(message) = pending.pop_front() {
            if let Err(e) = self.session.commit(message.clone(), events).await {
                pending.push_front(message);
                self.steering.requeue_front(pending.into());
                if applied > 0 {
                    events.emit(AgentEvent::SteeringApplied { count: applied });
                }
                return Err(self.fail_journal(e, events));
            }
            applied += 1;
        }
        if applied > 0 {
            events.emit(AgentEvent::SteeringApplied { count: applied });
        }
        Ok(())
    }

    /// Answers calls that will not run, all with the same error, through the journal.
    async fn resolve_unexecuted(
        &mut self,
        calls: &[ToolCall],
        code: &str,
        message: &str,
        events: &dyn EventSink,
    ) -> Result<(), JournalError> {
        for (index, call) in calls.iter().enumerate() {
            events.emit(AgentEvent::ToolFinished {
                index,
                call_id: call.id.clone(),
                is_error: true,
            });
            self.session
                .commit(
                    Message::Tool {
                        result: ToolResult::error(&call.id, code, message),
                        meta: MessageMeta::default(),
                    },
                    events,
                )
                .await?;
        }
        Ok(())
    }

    fn finish(&mut self, status: RunStatus, events: &dyn EventSink) -> Step {
        let unapplied_steering = self.steering.close();
        let final_message = self.session.transcript[self.turn_start..]
            .iter()
            .rev()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .cloned();
        let outcome = RunOutcome {
            status: status.clone(),
            final_message,
            unapplied_steering,
            model_calls: self.model_calls,
        };
        self.outcome = Some(outcome.clone());
        events.emit(AgentEvent::RunFinished { status });
        self.agent.hooks.on_turn_end(&self.hook_context(), &outcome);
        Step::Finished(outcome)
    }

    fn fail(&mut self, error: RunError, events: &dyn EventSink) -> RunError {
        self.finish(RunStatus::Failed, events);
        error
    }

    /// Ends the turn after a failed journal append: the session refuses further turns until
    /// the host recovers it (D6). Memory and events were left untouched by `commit`.
    fn fail_journal(&mut self, error: JournalError, events: &dyn EventSink) -> RunError {
        self.session.recovery_required = true;
        self.finish(RunStatus::Failed, events);
        let outcome = self.outcome.as_ref().expect("finish sets the outcome");
        self.session
            .stranded_steering
            .extend(outcome.unapplied_steering.iter().cloned());
        RunError::Journal(error)
    }

    fn fail_hook(
        &mut self,
        point: HookPoint,
        error: HookError,
        events: &dyn EventSink,
    ) -> RunError {
        events.emit(AgentEvent::HookFailed {
            hook: point,
            message: error.to_string(),
        });
        self.fail(RunError::Hook { point, error }, events)
    }

    /// Runs synchronously when an in-flight step future is dropped. Every open tool call gets
    /// an `interrupted` result so the transcript stays valid for `history_for_resume`.
    fn poison(&mut self, events: &dyn EventSink) {
        if let Some(mut batch) = self.batch.take() {
            for i in batch.unresolved() {
                let id = batch.calls[i].id.clone();
                batch.set(
                    i,
                    ToolResult::error(
                        id,
                        "interrupted",
                        "The run was interrupted while this call was pending. \
                         It may or may not have taken effect.",
                    ),
                );
            }
            batch.commit_ready(&mut self.session.transcript);
        }
        self.session.poisoned = true;
        self.finish(RunStatus::Poisoned, events);
        let outcome = self.outcome.as_ref().expect("finish sets the outcome");
        self.session
            .stranded_steering
            .extend(outcome.unapplied_steering.iter().cloned());
    }
}

impl<M: Model, P: ToolProvider, H: Hooks> Drop for ActiveRun<'_, M, P, H> {
    /// A turn dropped between steps has a consistent transcript; only its unapplied steering
    /// needs saving.
    fn drop(&mut self) {
        if self.outcome.is_none() {
            let unapplied = self.steering.close();
            self.session.stranded_steering.extend(unapplied);
        }
    }
}

/// One text part, or no parts at all when the accumulated string is empty. An interrupted
/// prefix keeps `content` and `reasoning` structurally the same shape: absent when nothing
/// was emitted on that channel.
fn parts_of(text: String) -> Vec<ContentPart> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(text)]
    }
}

/// Forwards events to the host sink while accumulating, separately, the visible text and the
/// reasoning this model call emitted through `ModelTextDelta`/`ModelReasoningDelta` (F2,
/// R5). A fresh one per call, so one call's text or reasoning can never leak into the next;
/// only the core-observed stream is counted, not what any adapter buffered.
struct StreamTap<'a> {
    host: &'a dyn EventSink,
    text: Mutex<String>,
    reasoning: Mutex<String>,
}

impl StreamTap<'_> {
    /// The visible text and the reasoning, each in emission order. They are never mixed.
    fn take(&self) -> (String, String) {
        (
            std::mem::take(&mut self.text.lock().unwrap()),
            std::mem::take(&mut self.reasoning.lock().unwrap()),
        )
    }
}

impl EventSink for StreamTap<'_> {
    fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::ModelTextDelta { text } => self.text.lock().unwrap().push_str(text),
            AgentEvent::ModelReasoningDelta { text } => {
                self.reasoning.lock().unwrap().push_str(text);
            }
            _ => {}
        }
        self.host.emit(event);
    }
}

/// Poisons the turn if the step future is dropped before it completes.
struct StepGuard<'r, 's, M: Model, P: ToolProvider, H: Hooks> {
    run: &'r mut ActiveRun<'s, M, P, H>,
    events: &'r dyn EventSink,
    armed: bool,
}

impl<M: Model, P: ToolProvider, H: Hooks> Drop for StepGuard<'_, '_, M, P, H> {
    fn drop(&mut self) {
        if self.armed {
            self.run.poison(self.events);
        }
    }
}

impl<M: Model, P: ToolProvider, H: Hooks> StepGuard<'_, '_, M, P, H> {
    async fn step_once(&mut self) -> Result<Step, RunError> {
        let events = self.events;
        let agent = Arc::clone(&self.run.agent);
        let interrupt = self.run.interrupt.clone();

        // The input lands in journal and memory before anything can end the turn, matching
        // the old behavior where `start_turn` had already pushed it (D9).
        if let Some(input) = self.run.pending_input.take()
            && let Err(e) = self.run.session.commit(input, events).await
        {
            return Err(self.run.fail_journal(e, events));
        }
        if !self.run.started {
            self.run.started = true;
            events.emit(AgentEvent::TurnStarted);
        }
        if let Some(reason) = interrupt.check() {
            return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
        }
        self.run.inject_steering(events).await?;
        if self.run.model_calls >= self.run.max_model_calls {
            return Ok(self.run.finish(RunStatus::BudgetExceeded, events));
        }

        self.run.model_calls += 1;
        let hcx = self.run.hook_context();
        let mut request = ModelRequest {
            system_prompt: agent.config.system_prompt.clone(),
            messages: self.run.session.transcript.clone(),
            tools: self.run.session.registry.specs().to_vec(),
            settings: agent.config.model_settings.clone(),
        };
        match race(
            &interrupt,
            agent.hooks.before_model_call(&hcx, &mut request),
        )
        .await
        {
            Raced::Interrupted(reason) => {
                return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
            }
            Raced::Done(Err(e)) => {
                return Err(self.run.fail_hook(HookPoint::BeforeModelCall, e, events));
            }
            Raced::Done(Ok(())) => {}
        }

        // Context budget (ADR 0015 projection, ADR 0018 semantics): the hook has run, so
        // this is the request that would be sent. A provider-reported `input` from an earlier
        // request in this session is the baseline; without one the projection is a local
        // estimate. No configured budget means nothing is claimed.
        let request_size = if agent.config.context_budget.is_some() {
            Some(budget::request_size(&request))
        } else {
            None
        };
        if let Some((config, size)) = agent.config.context_budget.zip(request_size) {
            let report = budget::project(&config, self.run.session.budget_baseline, size);
            // ADR 0018: over the input budget `B` the request is still sent. Automatic
            // compaction triggers here (compact-core slice); this slice only records the
            // status for the host's status bar.
            self.run.session.budget_status = if budget::over_budget(&report) {
                BudgetStatus::Over(report)
            } else {
                BudgetStatus::Ok(report)
            };
        } else {
            self.run.session.budget_status = BudgetStatus::Disabled;
        }

        events.emit(AgentEvent::ModelRequestStarted {
            model_call: self.run.model_calls,
            budget: self.run.session.context_budget_status().report().cloned(),
        });
        let tap = StreamTap {
            host: events,
            text: Mutex::new(String::new()),
            reasoning: Mutex::new(String::new()),
        };
        let mcx = ModelContext {
            cancellation: interrupt.token.child_token(),
            events: &tap,
        };
        let response = match race(&interrupt, agent.model.complete(request, mcx)).await {
            Raced::Interrupted(reason) => {
                // The model future is gone; whatever it emitted is in the tap. Any non-empty
                // prefix — visible text **or** reasoning — becomes an interrupted assistant
                // message through the journal before the turn converges (F3/F5, revised F4:
                // "empty" means both are empty). The append itself is never cancelled (D3)
                // and runs no hooks: this is not a complete response.
                let (text, reasoning) = tap.take();
                if !text.is_empty() || !reasoning.is_empty() {
                    let message = Message::Assistant {
                        content: parts_of(text),
                        reasoning: parts_of(reasoning),
                        tool_calls: Vec::new(),
                        provider_payload: None,
                        interrupted: true,
                        meta: MessageMeta::default(),
                    };
                    if let Err(e) = self.run.session.commit(message, events).await {
                        return Err(self.run.fail_journal(e, events));
                    }
                }
                return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
            }
            Raced::Done(Err(e)) => return Err(self.run.fail(RunError::Model(e), events)),
            Raced::Done(Ok(response)) => response,
        };
        events.emit(AgentEvent::ModelResponseReceived {
            stop_reason: response.stop_reason.clone(),
            tool_calls: response.tool_calls.len(),
            usage: response.usage.clone(),
        });
        // Remember the provider's own `input` as the next projection's baseline (ADR 0015
        // C1). `None` means the provider did not report it; we never substitute zero.
        if let Some(size) = request_size
            && let Some(input) = response.usage.as_ref().and_then(|usage| usage.input)
        {
            self.run.session.budget_baseline = Some((input, size));
        }
        match race(
            &interrupt,
            agent.hooks.after_model_response(&hcx, &response),
        )
        .await
        {
            Raced::Interrupted(reason) => {
                return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
            }
            Raced::Done(Err(e)) => {
                return Err(self.run.fail_hook(HookPoint::AfterModelResponse, e, events));
            }
            Raced::Done(Ok(())) => {}
        }

        let ModelResponse {
            content,
            reasoning,
            tool_calls,
            provider_payload,
            stop_reason,
            usage,
        } = response;
        let assistant = Message::Assistant {
            content,
            reasoning,
            tool_calls: tool_calls.clone(),
            provider_payload,
            interrupted: false,
            meta: MessageMeta::default(),
        };
        // Journaled before any tool of this response is dispatched (S4).
        if let Err(e) = self.run.session.commit(assistant, events).await {
            return Err(self.run.fail_journal(e, events));
        }
        // The provider's usage follows its message (J5) and is journaled the same strict way:
        // a failure is a journal failure, not a tolerated gap (D6/J6).
        if let Some(usage) = usage
            && let Some(journal) = self.run.session.journal.as_ref()
            && let Err(e) = journal.append(JournalRecord::ModelUsage { usage }).await
        {
            return Err(self.run.fail_journal(e, events));
        }

        if !tool_calls.is_empty() {
            return self.dispatch_tools(&agent, tool_calls, stop_reason).await;
        }
        let status = match stop_reason {
            StopReason::EndTurn => {
                // Steering that arrived during the final answer keeps the turn going,
                // which covers what other runtimes call follow-up messages.
                if self.run.steering.has_pending()
                    && self.run.model_calls < self.run.max_model_calls
                {
                    return Ok(Step::Continue);
                }
                RunStatus::Completed
            }
            StopReason::PauseTurn => return Ok(Step::Continue),
            StopReason::ToolUse => RunStatus::Incomplete(IncompleteReason::ToolUseWithoutCalls),
            StopReason::MaxTokens => RunStatus::Incomplete(IncompleteReason::MaxTokens),
            StopReason::ContentFiltered => RunStatus::Incomplete(IncompleteReason::ContentFiltered),
            StopReason::Refusal => RunStatus::Refused,
            StopReason::Other(reason) => {
                RunStatus::Incomplete(IncompleteReason::UnknownStopReason(reason))
            }
        };
        Ok(self.run.finish(status, events))
    }

    async fn dispatch_tools(
        &mut self,
        agent: &Agent<M, P, H>,
        calls: Vec<ToolCall>,
        stop_reason: StopReason,
    ) -> Result<Step, RunError> {
        let events = self.events;
        let run = &mut *self.run;
        match stop_reason {
            StopReason::MaxTokens => {
                if let Err(e) = run
                    .resolve_unexecuted(
                        &calls,
                        "truncated",
                        "The response was cut off at the token limit, so this call was not run. \
                         Send it again if it is still needed.",
                        events,
                    )
                    .await
                {
                    return Err(run.fail_journal(e, events));
                }
                return Ok(Step::Continue);
            }
            StopReason::ContentFiltered => {
                if let Err(e) = run
                    .resolve_unexecuted(
                        &calls,
                        "content_filtered",
                        "The response was filtered, so this call was not run.",
                        events,
                    )
                    .await
                {
                    return Err(run.fail_journal(e, events));
                }
                return Ok(run.finish(
                    RunStatus::Incomplete(IncompleteReason::ContentFiltered),
                    events,
                ));
            }
            _ => {}
        }
        if run.model_calls >= run.max_model_calls {
            if let Err(e) = run
                .resolve_unexecuted(
                    &calls,
                    "budget_exhausted",
                    "The model call budget is used up, so this call was not run.",
                    events,
                )
                .await
            {
                return Err(run.fail_journal(e, events));
            }
            return Ok(run.finish(RunStatus::BudgetExceeded, events));
        }

        let hcx = run.hook_context();
        run.batch = Some(Batch::new(calls.clone()));
        // Disjoint field borrows: the running tool futures hold `tools` for their lifetime,
        // so the journal, transcript and registry travel separately and meet again inside
        // `commit_parts`.
        let session = &mut *run.session;
        let cx = BatchCx::<P, H> {
            tools: &session.tools,
            registry: &session.registry,
            journal: session.journal.as_ref(),
            transcript: &mut session.transcript,
            hooks: &agent.hooks,
            batch: run.batch.as_mut().expect("batch was just set"),
            steering: &run.steering,
            options: &run.options,
            interrupt: &run.interrupt,
            events,
            hcx,
        };
        let interrupted = match cx.run(calls).await {
            Ok(interrupted) => interrupted,
            Err(e) => return Err(run.fail_journal(e, events)),
        };
        debug_assert!(run.batch.as_ref().is_none_or(Batch::is_complete));
        run.batch = None;
        Ok(match interrupted {
            Some(reason) => run.finish(RunStatus::Cancelled(reason), events),
            None => Step::Continue,
        })
    }
}

/// Disjoint borrows of the turn and session needed to run one batch.
struct BatchCx<'a, P: ToolProvider, H: Hooks> {
    tools: &'a P::Session,
    registry: &'a ToolRegistry,
    journal: Option<&'a Arc<dyn TranscriptJournal>>,
    transcript: &'a mut Vec<Message>,
    hooks: &'a H,
    batch: &'a mut Batch,
    steering: &'a SteeringHandle,
    options: &'a TurnOptions,
    interrupt: &'a Interrupt,
    events: &'a dyn EventSink,
    hcx: HookContext,
}

/// Why a batch stopped before its plan played out.
enum BatchStop {
    Interrupted(CancelReason),
    Journal(JournalError),
}

impl<P: ToolProvider, H: Hooks> BatchCx<'_, P, H> {
    /// Validates, runs hooks, then schedules. Returns the reason if the turn was interrupted;
    /// either way every call ends with a result in the transcript. A journal failure ends
    /// the batch at once (D6): later appends are not attempted and memory keeps the prefix.
    async fn run(mut self, calls: Vec<ToolCall>) -> Result<Option<CancelReason>, JournalError> {
        let plan = match self.plan(calls).await {
            Ok(plan) => plan,
            Err(BatchStop::Interrupted(reason)) => {
                self.resolve_rest("cancelled", "The turn was cancelled before this call ran.")
                    .await?;
                return Ok(Some(reason));
            }
            Err(BatchStop::Journal(e)) => return Err(e),
        };
        let interrupted = self.schedule(plan).await?;
        if interrupted.is_some() {
            self.resolve_rest("cancelled", "The turn was cancelled before this call ran.")
                .await?;
        }
        Ok(interrupted)
    }

    /// Preflight and `before_tool_call`, one call at a time in order.
    async fn plan(
        &mut self,
        calls: Vec<ToolCall>,
    ) -> Result<Vec<Option<(ToolCall, ToolConcurrency)>>, BatchStop> {
        let mut plan = Vec::with_capacity(calls.len());
        for (i, call) in calls.into_iter().enumerate() {
            plan.push(None);
            if let Err(result) = self.registry.check(&call) {
                if let Err(e) = self.resolve(i, result).await {
                    return Err(BatchStop::Journal(e));
                }
                continue;
            }
            let decision = match race(
                self.interrupt,
                self.hooks.before_tool_call(&self.hcx, &call),
            )
            .await
            {
                Raced::Interrupted(reason) => return Err(BatchStop::Interrupted(reason)),
                Raced::Done(decision) => decision,
            };
            match decision {
                Ok(ToolDecision::Allow) => {
                    let concurrency = self.registry.check(&call).expect("checked above");
                    plan[i] = Some((call, concurrency));
                }
                Ok(ToolDecision::Rewrite { arguments }) => {
                    let call = ToolCall { arguments, ..call };
                    match self.registry.check(&call) {
                        Ok(concurrency) => plan[i] = Some((call, concurrency)),
                        Err(result) => {
                            if let Err(e) = self.resolve(i, result).await {
                                return Err(BatchStop::Journal(e));
                            }
                        }
                    }
                }
                Ok(ToolDecision::Deny { reason }) => {
                    if let Err(e) = self
                        .resolve(i, ToolResult::error(&call.id, "denied", reason))
                        .await
                    {
                        return Err(BatchStop::Journal(e));
                    }
                }
                Ok(ToolDecision::Respond(mut result)) => {
                    result.call_id = call.id;
                    if let Err(e) = self.resolve(i, result).await {
                        return Err(BatchStop::Journal(e));
                    }
                }
                Err(e) => {
                    self.events.emit(AgentEvent::HookFailed {
                        hook: HookPoint::BeforeToolCall,
                        message: e.to_string(),
                    });
                    if let Err(e) = self
                        .resolve(
                            i,
                            ToolResult::error(
                                &call.id,
                                "hook_error",
                                format!("A policy check failed: {e}"),
                            ),
                        )
                        .await
                    {
                        return Err(BatchStop::Journal(e));
                    }
                }
            }
        }
        Ok(plan)
    }

    /// Runs planned calls. Consecutive `Shared` calls form a group that runs concurrently;
    /// an `Exclusive` call waits for everything before it and runs alone.
    async fn schedule(
        &mut self,
        mut plan: Vec<Option<(ToolCall, ToolConcurrency)>>,
    ) -> Result<Option<CancelReason>, JournalError> {
        let n = plan.len();
        let mut i = 0;
        while i < n {
            let Some((_, concurrency)) = &plan[i] else {
                i += 1;
                continue;
            };
            if let Some(reason) = self.interrupt.check() {
                return Ok(Some(reason));
            }
            let end = match concurrency {
                ToolConcurrency::Exclusive => i + 1,
                ToolConcurrency::Shared => {
                    let mut j = i;
                    while j < n
                        && plan[j]
                            .as_ref()
                            .is_none_or(|(_, c)| *c == ToolConcurrency::Shared)
                    {
                        j += 1;
                    }
                    j
                }
            };
            let group: Vec<(usize, ToolCall)> = (i..end)
                .filter_map(|k| plan[k].take().map(|(call, _)| (k, call)))
                .collect();
            if let Some(reason) = self.run_group(group).await? {
                return Ok(Some(reason));
            }
            i = end;
        }
        Ok(None)
    }

    async fn run_group(
        &mut self,
        group: Vec<(usize, ToolCall)>,
    ) -> Result<Option<CancelReason>, JournalError> {
        let limit = self.options.max_parallel_tools.max(1);
        let skip_on_steer = self.options.steer_mode == SteerMode::SkipRemaining;
        let (tools, hooks, events, interrupt) =
            (self.tools, self.hooks, self.events, self.interrupt);
        let cancel = interrupt.token.child_token();
        let mut queue = group.into_iter();
        let mut running = FuturesUnordered::new();
        let mut interrupted = None;
        let mut journal_failed = None;
        loop {
            while interrupted.is_none() && journal_failed.is_none() && running.len() < limit {
                let Some((index, call)) = queue.next() else {
                    break;
                };
                if skip_on_steer && self.steering.has_pending() {
                    let result = ToolResult::error(
                        &call.id,
                        "skipped_by_steering",
                        "The user sent a new message, so this call was not run.",
                    );
                    journal_failed = self.resolve(index, result).await.err();
                    continue;
                }
                // E3: the marker is durable before the tool's own execution may begin. On
                // failure the call is not dispatched (E4) and the batch stops.
                if let Some(journal) = self.journal {
                    let record = JournalRecord::ToolExecutionStarted {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    };
                    if let Err(e) = journal.append(record).await {
                        journal_failed = Some(e);
                        continue;
                    }
                }
                // Observers learn of the start only once it is durable.
                events.emit(AgentEvent::ToolStarted {
                    index,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                });
                running.push(call_one(
                    tools,
                    hooks,
                    self.hcx.clone(),
                    events,
                    index,
                    call,
                    cancel.clone(),
                    self.options.tool_cancel_grace,
                ));
            }
            if running.is_empty() {
                break;
            }
            if journal_failed.is_some() {
                // The journal is broken, so nothing more is written; but the calls already
                // running must still hear the cancellation and get their grace period
                // instead of being dropped mid-flight. Their results are discarded: with no
                // `Settled`, recovery treats them as `outcome_unknown_after_crash`.
                cancel.cancel();
                while running.next().await.is_some() {}
                break;
            }
            tokio::select! {
                biased;
                reason = interrupt.wait(), if interrupted.is_none() => {
                    interrupted = Some(reason);
                    cancel.cancel();
                }
                // Awaiting a journal append inside this branch pauses the other branches:
                // the append is short, and this backpressure is the price of serializing
                // journal writes with tool completions (AC-A1-6).
                Some((index, result)) = running.next() => {
                    if let Err(e) = self.settle(index, result).await {
                        journal_failed = Some(e);
                    }
                }
            }
        }
        match journal_failed {
            // A failed append ends the turn; the not-yet-started calls behind it never run
            // and the batch's remaining slots stay unresolved in memory (D6, S3).
            Some(e) => Err(e),
            None => Ok(interrupted),
        }
    }

    /// Settles one executed call: the marker is appended first (E3), and only then does the
    /// result join the ready-prefix commit. Cancelled and hook-error results settle too
    /// (E5); they went through the same execution.
    async fn settle(&mut self, index: usize, result: ToolResult) -> Result<(), JournalError> {
        if let Some(journal) = self.journal {
            journal
                .append(JournalRecord::ToolExecutionSettled {
                    call_id: result.call_id.clone(),
                    result: result.clone(),
                })
                .await?;
        }
        self.resolve(index, result).await
    }

    /// Records one result and commits every result that has become ready, in call order,
    /// through the journal.
    async fn resolve(&mut self, index: usize, result: ToolResult) -> Result<(), JournalError> {
        self.events.emit(AgentEvent::ToolFinished {
            index,
            call_id: result.call_id.clone(),
            is_error: result.is_error,
        });
        self.batch.set(index, result);
        while let Some(result) = self.batch.next_ready() {
            crate::agent::commit_parts(
                self.journal,
                self.transcript,
                Message::Tool {
                    result,
                    meta: MessageMeta::default(),
                },
                self.events,
            )
            .await?;
        }
        Ok(())
    }

    async fn resolve_rest(&mut self, code: &str, message: &str) -> Result<(), JournalError> {
        for i in self.batch.unresolved() {
            let result = ToolResult::error(&self.batch.calls[i].id, code, message);
            self.resolve(i, result).await?;
        }
        Ok(())
    }
}

/// Executes one call and its `after_tool_call` hook.
///
/// On cancellation the tool sees `cx.cancellation` fire and gets `grace` to finish its own
/// cleanup; then its future is dropped. Either way the call's result is `cancelled`.
#[allow(clippy::too_many_arguments)]
async fn call_one<S: ToolSession, H: Hooks>(
    tools: &S,
    hooks: &H,
    hcx: HookContext,
    events: &dyn EventSink,
    index: usize,
    call: ToolCall,
    cancel: CancellationToken,
    grace: Duration,
) -> (usize, ToolResult) {
    let cancelled = |call: &ToolCall| {
        ToolResult::error(
            &call.id,
            "cancelled",
            "The turn was cancelled while this call was running.",
        )
    };
    // A call whose turn was cancelled before it was first polled never starts: its body
    // would only run to be cancelled at once.
    if cancel.is_cancelled() {
        return (index, cancelled(&call));
    }
    let cx = ToolContext {
        cancellation: cancel.clone(),
        events,
        index,
    };
    let execution = tools.execute(&call, cx);
    tokio::pin!(execution);
    let output = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            // Whatever the tool returns now, the turn is over; the result is `cancelled`.
            let _ = tokio::time::timeout(grace, &mut execution).await;
            return (index, cancelled(&call));
        }
        output = &mut execution => output,
    };
    let mut result = match output {
        Ok(output) => ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            structured: output.structured,
            is_error: false,
            error_code: None,
        },
        Err(failure) => ToolResult::error(&call.id, &failure.code, failure.message),
    };
    let hook = tokio::select! {
        biased;
        () = cancel.cancelled() => return (index, cancelled(&call)),
        hook = hooks.after_tool_call(&hcx, &call, &mut result) => hook,
    };
    if let Err(e) = hook {
        events.emit(AgentEvent::HookFailed {
            hook: HookPoint::AfterToolCall,
            message: e.to_string(),
        });
        // Fail closed: the unprocessed output must not reach the model.
        result = ToolResult::error(
            &call.id,
            "hook_error",
            format!("Result processing failed: {e}"),
        );
    }
    // The hook may edit the result, but not which call it answers.
    result.call_id = call.id.clone();
    (index, result)
}

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentSession};
use crate::event::{AgentEvent, EventSink};
use crate::hooks::{HookContext, HookError, HookPoint, Hooks, ToolDecision};
use crate::message::{Message, ToolCall, ToolResult};
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
            transcript.push(Message::Tool { result });
            self.committed += 1;
        }
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
    model_calls: u32,
    max_model_calls: u32,
    started: bool,
    batch: Option<Batch>,
    outcome: Option<RunOutcome>,
}

impl<'s, M: Model, P: ToolProvider, H: Hooks> ActiveRun<'s, M, P, H> {
    pub(crate) fn new(session: &'s mut AgentSession<M, P, H>, options: TurnOptions) -> Self {
        let agent = Arc::clone(&session.agent);
        let max_model_calls = options
            .max_model_calls
            .unwrap_or(agent.config.max_model_calls);
        // The user input was just pushed; it belongs to this turn.
        let turn_start = session.transcript.len() - 1;
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

    fn inject_steering(&mut self, events: &dyn EventSink) {
        let messages = self.steering.drain();
        if !messages.is_empty() {
            events.emit(AgentEvent::SteeringApplied {
                count: messages.len(),
            });
            self.session.transcript.extend(messages);
        }
    }

    /// Answers calls that will not run, all with the same error.
    fn resolve_unexecuted(
        &mut self,
        calls: &[ToolCall],
        code: &str,
        message: &str,
        events: &dyn EventSink,
    ) {
        for (index, call) in calls.iter().enumerate() {
            events.emit(AgentEvent::ToolFinished {
                index,
                call_id: call.id.clone(),
                is_error: true,
            });
            self.session.transcript.push(Message::Tool {
                result: ToolResult::error(&call.id, code, message),
            });
        }
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

        if !self.run.started {
            self.run.started = true;
            events.emit(AgentEvent::TurnStarted);
        }
        if let Some(reason) = interrupt.check() {
            return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
        }
        self.run.inject_steering(events);
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

        events.emit(AgentEvent::ModelRequestStarted {
            model_call: self.run.model_calls,
        });
        let mcx = ModelContext {
            cancellation: interrupt.token.child_token(),
            events,
        };
        let response = match race(&interrupt, agent.model.complete(request, mcx)).await {
            Raced::Interrupted(reason) => {
                return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
            }
            Raced::Done(Err(e)) => return Err(self.run.fail(RunError::Model(e), events)),
            Raced::Done(Ok(response)) => response,
        };
        events.emit(AgentEvent::ModelResponseReceived {
            stop_reason: response.stop_reason.clone(),
            tool_calls: response.tool_calls.len(),
        });
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
            tool_calls,
            provider_payload,
            stop_reason,
        } = response;
        self.run.session.transcript.push(Message::Assistant {
            content,
            tool_calls: tool_calls.clone(),
            provider_payload,
        });

        if !tool_calls.is_empty() {
            return Ok(self.dispatch_tools(&agent, tool_calls, stop_reason).await);
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
    ) -> Step {
        let events = self.events;
        let run = &mut *self.run;
        match stop_reason {
            StopReason::MaxTokens => {
                run.resolve_unexecuted(
                    &calls,
                    "truncated",
                    "The response was cut off at the token limit, so this call was not run. \
                     Send it again if it is still needed.",
                    events,
                );
                return Step::Continue;
            }
            StopReason::ContentFiltered => {
                run.resolve_unexecuted(
                    &calls,
                    "content_filtered",
                    "The response was filtered, so this call was not run.",
                    events,
                );
                return run.finish(
                    RunStatus::Incomplete(IncompleteReason::ContentFiltered),
                    events,
                );
            }
            _ => {}
        }
        if run.model_calls >= run.max_model_calls {
            run.resolve_unexecuted(
                &calls,
                "budget_exhausted",
                "The model call budget is used up, so this call was not run.",
                events,
            );
            return run.finish(RunStatus::BudgetExceeded, events);
        }

        let hcx = run.hook_context();
        run.batch = Some(Batch::new(calls.clone()));
        let cx = BatchCx {
            tools: &run.session.tools,
            registry: &run.session.registry,
            hooks: &agent.hooks,
            transcript: &mut run.session.transcript,
            batch: run.batch.as_mut().expect("batch was just set"),
            steering: &run.steering,
            options: &run.options,
            interrupt: &run.interrupt,
            events,
            hcx,
        };
        let interrupted = cx.run(calls).await;
        debug_assert!(run.batch.as_ref().is_some_and(Batch::is_complete));
        run.batch = None;
        match interrupted {
            Some(reason) => run.finish(RunStatus::Cancelled(reason), events),
            None => Step::Continue,
        }
    }
}

/// Disjoint borrows of the turn and session needed to run one batch.
struct BatchCx<'a, S, H> {
    tools: &'a S,
    registry: &'a ToolRegistry,
    hooks: &'a H,
    transcript: &'a mut Vec<Message>,
    batch: &'a mut Batch,
    steering: &'a SteeringHandle,
    options: &'a TurnOptions,
    interrupt: &'a Interrupt,
    events: &'a dyn EventSink,
    hcx: HookContext,
}

impl<S: ToolSession, H: Hooks> BatchCx<'_, S, H> {
    /// Validates, runs hooks, then schedules. Returns the reason if the turn was interrupted;
    /// either way every call ends with a result in the transcript.
    async fn run(mut self, calls: Vec<ToolCall>) -> Option<CancelReason> {
        let plan = match self.plan(calls).await {
            Ok(plan) => plan,
            Err(reason) => {
                self.resolve_rest("cancelled", "The turn was cancelled before this call ran.");
                return Some(reason);
            }
        };
        let interrupted = self.schedule(plan).await;
        if interrupted.is_some() {
            self.resolve_rest("cancelled", "The turn was cancelled before this call ran.");
        }
        interrupted
    }

    /// Preflight and `before_tool_call`, one call at a time in order.
    async fn plan(
        &mut self,
        calls: Vec<ToolCall>,
    ) -> Result<Vec<Option<(ToolCall, ToolConcurrency)>>, CancelReason> {
        let mut plan = Vec::with_capacity(calls.len());
        for (i, call) in calls.into_iter().enumerate() {
            plan.push(None);
            if let Err(result) = self.registry.check(&call) {
                self.resolve(i, result);
                continue;
            }
            let decision = match race(
                self.interrupt,
                self.hooks.before_tool_call(&self.hcx, &call),
            )
            .await
            {
                Raced::Interrupted(reason) => return Err(reason),
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
                        Err(result) => self.resolve(i, result),
                    }
                }
                Ok(ToolDecision::Deny { reason }) => {
                    self.resolve(i, ToolResult::error(&call.id, "denied", reason));
                }
                Ok(ToolDecision::Respond(mut result)) => {
                    result.call_id = call.id;
                    self.resolve(i, result);
                }
                Err(e) => {
                    self.events.emit(AgentEvent::HookFailed {
                        hook: HookPoint::BeforeToolCall,
                        message: e.to_string(),
                    });
                    self.resolve(
                        i,
                        ToolResult::error(
                            &call.id,
                            "hook_error",
                            format!("A policy check failed: {e}"),
                        ),
                    );
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
    ) -> Option<CancelReason> {
        let n = plan.len();
        let mut i = 0;
        while i < n {
            let Some((_, concurrency)) = &plan[i] else {
                i += 1;
                continue;
            };
            if let Some(reason) = self.interrupt.check() {
                return Some(reason);
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
            if let Some(reason) = self.run_group(group).await {
                return Some(reason);
            }
            i = end;
        }
        None
    }

    async fn run_group(&mut self, group: Vec<(usize, ToolCall)>) -> Option<CancelReason> {
        let limit = self.options.max_parallel_tools.max(1);
        let skip_on_steer = self.options.steer_mode == SteerMode::SkipRemaining;
        let (tools, hooks, events, interrupt) =
            (self.tools, self.hooks, self.events, self.interrupt);
        let cancel = interrupt.token.child_token();
        let mut queue = group.into_iter();
        let mut running = FuturesUnordered::new();
        let mut interrupted = None;
        loop {
            while interrupted.is_none() && running.len() < limit {
                let Some((index, call)) = queue.next() else {
                    break;
                };
                if skip_on_steer && self.steering.has_pending() {
                    let result = ToolResult::error(
                        &call.id,
                        "skipped_by_steering",
                        "The user sent a new message, so this call was not run.",
                    );
                    self.resolve(index, result);
                    continue;
                }
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
            tokio::select! {
                biased;
                reason = interrupt.wait(), if interrupted.is_none() => {
                    interrupted = Some(reason);
                    cancel.cancel();
                }
                Some((index, result)) = running.next() => self.resolve(index, result),
            }
        }
        interrupted
    }

    fn resolve(&mut self, index: usize, result: ToolResult) {
        self.events.emit(AgentEvent::ToolFinished {
            index,
            call_id: result.call_id.clone(),
            is_error: result.is_error,
        });
        self.batch.set(index, result);
        self.batch.commit_ready(self.transcript);
    }

    fn resolve_rest(&mut self, code: &str, message: &str) {
        for i in self.batch.unresolved() {
            let result = ToolResult::error(&self.batch.calls[i].id, code, message);
            self.resolve(i, result);
        }
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

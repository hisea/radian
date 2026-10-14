use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentSession};
use crate::budget::{self, BudgetReport, BudgetStatus, ContextBudget, RequestSize};
use crate::compaction;
use crate::event::NoopSink;
use crate::event::{AgentEvent, CompactionTrigger, EventSink};
use crate::hooks::{HookContext, HookError, HookPoint, Hooks, ToolDecision};
use crate::journal::{JournalError, JournalRecord, TranscriptJournal};
use crate::message::{ContentPart, Message, MessageMeta, ToolCall, ToolResult};
use crate::model::{Model, ModelContext, ModelError, ModelRequest, ModelResponse, StopReason};
use crate::outcome::{
    CancelReason, CompactionError, CompactionFailure, CompactionOutcome, IncompleteReason,
    RunError, RunOutcome, RunStatus, Step,
};
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

/// What `maybe_compact` decided for the pending request (ADR 0018).
enum CompactionDecision {
    /// The request stays as it was; it is sent (possibly still over `B`).
    NotNeeded,
    /// The transcript was replaced and the request rebuilt.
    Applied {
        size: RequestSize,
        report: BudgetReport,
    },
    /// The turn was cancelled while summarizing; no compaction was committed.
    Cancelled(CancelReason),
}

/// The borrows `maybe_compact` needs, bundled so the method keeps a small signature.
struct CompactCx<'a> {
    interrupt: &'a Interrupt,
    hcx: &'a HookContext,
    budget: &'a ContextBudget,
    events: &'a dyn EventSink,
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

/// The gain rule a compaction must satisfy (ADR 0018). The proactive budget path must reach
/// `B`; the overflow fallback must reach half of its reference; a manual `/compact` only has
/// to shrink the request.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Gain {
    Budget { input_budget: u64 },
    Half { reference: u64 },
    Any,
}

/// What one compaction run produced.
pub(crate) enum CompactionStep {
    Applied {
        revision: u64,
        tokens_before: u64,
        tokens_after: u64,
        below_target: bool,
        /// The message index the kept suffix starts at in the new history (`0` is the summary).
        first_kept: usize,
        size_after: RequestSize,
    },
    NothingToCompact,
    Cancelled(CancelReason),
}

/// The inputs one compaction run needs. Bundled so the shared routine keeps a small signature.
struct CompactionRun<'a> {
    interrupt: &'a Interrupt,
    /// The hook context for the post-compaction `before_model_call` re-run. `None` for a
    /// manual `/compact`: it is measured without injection and must not write anything.
    hcx: Option<&'a HookContext>,
    request: &'a mut ModelRequest,
    tokens_before: u64,
    keep: u64,
    /// The input budget `B` the summary ceiling scales with
    /// ([`compaction::summary_max_tokens`]); `None` means no window, so the ceiling is the
    /// fixed [`compaction::DEFAULT_SUMMARY_MAX_TOKENS`].
    summary_budget: Option<u64>,
    gain: Gain,
    trigger: CompactionTrigger,
    instructions: Option<String>,
    events: &'a dyn EventSink,
}

/// Plans a compaction that reserves the space the verbatim user messages will take (ADR 0018
/// revision 6).
///
/// The plan is computed, the retained list is computed for it, and the keep target is reduced
/// by the list's size — twice at most, because the list is bounded by `U + U₁` and the second
/// pass only ever makes the summarized prefix longer. Returns the final plan and the list that
/// goes with it, so the caller cannot pair a plan with a list from another pass.
fn plan_with_verbatim(
    transcript: &[Message],
    keep: u64,
    summary_budget: Option<u64>,
    previous: &[compaction::RetainedUserMessage],
) -> Result<(compaction::Plan, Vec<compaction::RetainedUserMessage>), compaction::PlanError> {
    let has_summary = transcript
        .first()
        .is_some_and(compaction::is_compaction_summary);
    let summarize_from = |first_kept: usize| -> Vec<Message> {
        if has_summary {
            transcript[1..first_kept].to_vec()
        } else {
            transcript[..first_kept].to_vec()
        }
    };
    // The reservation is `K - used`, not a running subtraction: the list is re-selected for
    // each candidate plan, so its size is measured against the original target every time.
    let target = keep;
    let mut keep = keep;
    let mut plan = compaction::plan_with_keep(transcript, keep, &compaction::message_tokens)?;
    let mut user_messages = compaction::select_user_messages(
        previous,
        &summarize_from(plan.first_kept),
        summary_budget,
    );
    for _ in 0..2 {
        let used: u64 = user_messages
            .iter()
            .map(|message| compaction::message_tokens(&Message::user(&message.text)))
            .sum();
        let next = target.saturating_sub(used);
        if next == keep {
            break;
        }
        keep = next;
        plan = compaction::plan_with_keep(transcript, keep, &compaction::message_tokens)?;
        user_messages = compaction::select_user_messages(
            previous,
            &summarize_from(plan.first_kept),
            summary_budget,
        );
    }
    Ok((plan, user_messages))
}

impl<M: Model, P: ToolProvider, H: Hooks> AgentSession<M, P, H> {
    /// A manual `/compact` (ADR 0018 §兜底、手动与关闭). The driver runs it between turns and
    /// supplies the cancellation token; cancelling aborts it without committing. Unlike the
    /// automatic paths it needs no configured budget and only requires `T_after < T_before`.
    pub async fn compact_now(
        &mut self,
        instructions: Option<String>,
        cancellation: &CancellationToken,
        events: &dyn EventSink,
    ) -> Result<CompactionOutcome, CompactionFailure> {
        let interrupt = Interrupt {
            token: cancellation.clone(),
            deadline: None,
        };
        let keep = match self.agent.config.context_budget {
            Some(config) => compaction::keep_target(config.input_budget_tokens),
            None => compaction::MAX_KEEP_TARGET,
        };
        // `T_before`: the request as it stands now, deliberately without running the hook (a
        // manual compaction must not write anything).
        let mut request = ModelRequest {
            system_prompt: self.agent.config.system_prompt.clone(),
            messages: self.transcript.clone(),
            tools: self.registry.specs().to_vec(),
            settings: self.agent.config.model_settings.clone(),
        };
        let bytes_per_token = self
            .agent
            .config
            .context_budget
            .map(|config| config.bytes_per_token)
            .unwrap_or(crate::budget::DEFAULT_BYTES_PER_TOKEN);
        let tokens_before = budget::request_size(&request).tokens(bytes_per_token);
        match self
            .compact_transcript(CompactionRun {
                interrupt: &interrupt,
                hcx: None,
                request: &mut request,
                tokens_before,
                keep,
                summary_budget: self
                    .agent
                    .config
                    .context_budget
                    .map(|config| config.input_budget_tokens),
                gain: Gain::Any,
                trigger: CompactionTrigger::Manual,
                instructions,
                events,
            })
            .await?
        {
            CompactionStep::Applied {
                revision,
                tokens_before,
                tokens_after,
                ..
            } => Ok(CompactionOutcome::Compacted {
                revision,
                tokens_before,
                tokens_after,
            }),
            CompactionStep::NothingToCompact => Ok(CompactionOutcome::NothingToCompact),
            CompactionStep::Cancelled(_) => {
                Err(CompactionFailure::Compaction(CompactionError::Cancelled))
            }
        }
    }

    /// The shared compaction routine (ADR 0018). Plans, summarizes (one controlled retry),
    /// checks the gain rule, and commits through `replace_transcript`. The proactive budget
    /// path, the provider-overflow fallback and a manual `/compact` all call it, so their
    /// splitting, summary, commit and revision rules cannot diverge.
    async fn compact_transcript(
        &mut self,
        run: CompactionRun<'_>,
    ) -> Result<CompactionStep, CompactionFailure> {
        let CompactionRun {
            interrupt,
            hcx,
            request,
            tokens_before,
            keep,
            summary_budget,
            gain,
            trigger,
            instructions,
            events,
        } = run;
        let revision = self.transcript_revision + 1;
        let source_revision = self.transcript_revision;
        let source_len = self.transcript.len();

        // ADR 0018: the user's words are carried verbatim, and the plan reserves
        // the space they will take. A compaction summary is always the first message of a
        // compacted history, so the rolling summary and the rolling verbatim list are read out
        // of it and excluded from the new summary input.
        let transcript = self.transcript.clone();
        let previous_summary = transcript
            .first()
            .and_then(compaction::compaction_summary_text);
        let previous_user_messages: Vec<compaction::RetainedUserMessage> = transcript
            .first()
            .map(compaction::compaction_user_messages)
            .unwrap_or_default();
        let (plan, user_messages) =
            match plan_with_verbatim(&transcript, keep, summary_budget, &previous_user_messages) {
                Ok(planned) => planned,
                Err(compaction::PlanError::NothingToCompact) => {
                    events.emit(AgentEvent::CompactionSkipped {
                        reason: "the history already fits the keep target".into(),
                    });
                    return Ok(CompactionStep::NothingToCompact);
                }
                Err(compaction::PlanError::InvalidHistory(error)) => {
                    events.emit(AgentEvent::CompactionFailed {
                        reason: error.to_string(),
                    });
                    return Err(CompactionFailure::Compaction(
                        CompactionError::InvalidSummary(error.to_string()),
                    ));
                }
            };

        events.emit(AgentEvent::CompactionStarted {
            revision,
            tokens_before,
            keep_target: keep,
            trigger,
        });

        let to_summarize: Vec<Message> = if previous_summary.is_some() {
            transcript[1..plan.first_kept].to_vec()
        } else {
            transcript[..plan.first_kept].to_vec()
        };
        // The summary ceiling scales with the window; no window
        // falls back to the fixed default.
        let summary_max = summary_budget
            .map(compaction::summary_max_tokens)
            .unwrap_or(compaction::DEFAULT_SUMMARY_MAX_TOKENS);

        let mut summary = None;
        // The first call summarizes the conversation. The one controlled retry (ADR 0018)
        // depends on why the first answer was invalid (dogfood7): an over-long answer gets a
        // **shortening** request that carries only that answer (not the conversation again),
        // while an empty answer just resends the original request.
        let mut summary_request = compaction::summary_request(
            previous_summary.as_deref(),
            &to_summarize,
            instructions.as_deref(),
            summary_max,
        );
        for attempt in 0..2 {
            let noop = NoopSink;
            let mcx = ModelContext {
                cancellation: interrupt.token.child_token(),
                events: &noop,
            };
            let response = match race(
                interrupt,
                self.agent.model.complete(summary_request.clone(), mcx),
            )
            .await
            {
                Raced::Interrupted(reason) => return Ok(CompactionStep::Cancelled(reason)),
                Raced::Done(Err(error)) => {
                    events.emit(AgentEvent::CompactionFailed {
                        reason: error.to_string(),
                    });
                    return Err(CompactionFailure::Compaction(CompactionError::Summary(
                        error,
                    )));
                }
                Raced::Done(Ok(response)) => response,
            };
            // ADR 0018 §计费: the real call is recorded immediately, commit or not, and it is
            // deliberately not a `ModelUsage` (it must not move the budget baseline).
            if let (Some(journal), Some(usage)) = (self.journal.as_ref(), response.usage.clone())
                && let Err(error) = journal
                    .append(JournalRecord::CompactionUsage {
                        revision,
                        model: compaction::COMPACTION_MODEL_CURRENT.to_owned(),
                        usage,
                    })
                    .await
            {
                self.recovery_required = true;
                return Err(CompactionFailure::Journal(error));
            }
            let text = text_of(&response.content);
            match compaction::check_summary(&text, summary_max) {
                Ok(checked) => {
                    summary = Some(checked);
                    break;
                }
                Err(error) if attempt == 0 => {
                    summary_request = match error {
                        compaction::SummaryError::TooLong { .. } => compaction::shorten_request(
                            text.trim(),
                            compaction::shorten_target(summary_max),
                        ),
                        // Empty (and the only other variant) resends the original request.
                        compaction::SummaryError::Empty => compaction::summary_request(
                            previous_summary.as_deref(),
                            &to_summarize,
                            instructions.as_deref(),
                            summary_max,
                        ),
                    };
                }
                Err(error) => {
                    events.emit(AgentEvent::CompactionFailed {
                        reason: error.to_string(),
                    });
                    return Err(CompactionFailure::Compaction(
                        CompactionError::InvalidSummary(error.to_string()),
                    ));
                }
            }
        }
        let summary = summary.expect("the loop sets it or returns");

        let new_history = match compaction::compacted_history(
            &summary,
            &user_messages,
            &transcript[plan.first_kept..],
        ) {
            Ok(history) => history,
            Err(error) => {
                events.emit(AgentEvent::CompactionFailed {
                    reason: error.to_string(),
                });
                return Err(CompactionFailure::Compaction(
                    CompactionError::InvalidSummary(error.to_string()),
                ));
            }
        };

        // Rebuild the request for the compacted view and, for the automatic paths, re-run the
        // hook (ADR 0018), so `T_after` describes exactly what would be sent.
        request.system_prompt = self.agent.config.system_prompt.clone();
        request.messages = new_history.clone();
        request.tools = self.registry.specs().to_vec();
        request.settings = self.agent.config.model_settings.clone();
        if let Some(hcx) = hcx {
            match race(interrupt, self.agent.hooks.before_model_call(hcx, request)).await {
                Raced::Interrupted(reason) => return Ok(CompactionStep::Cancelled(reason)),
                Raced::Done(Err(error)) => {
                    events.emit(AgentEvent::CompactionFailed {
                        reason: error.to_string(),
                    });
                    return Err(CompactionFailure::Hook {
                        point: HookPoint::BeforeModelCall,
                        error,
                    });
                }
                Raced::Done(Ok(())) => {}
            }
        }
        let size_after = budget::request_size(request);

        let bytes_per_token = self
            .agent
            .config
            .context_budget
            .map(|config| config.bytes_per_token)
            .unwrap_or(crate::budget::DEFAULT_BYTES_PER_TOKEN);
        let tokens_after = size_after.tokens(bytes_per_token);
        let enough = match gain {
            Gain::Budget { input_budget } => {
                tokens_after < tokens_before && tokens_after <= input_budget
            }
            Gain::Half { reference } => tokens_after.saturating_mul(2) <= reference,
            Gain::Any => tokens_after < tokens_before,
        };
        if !enough {
            let target = match gain {
                Gain::Budget { input_budget } => input_budget,
                Gain::Half { reference } => reference / 2,
                Gain::Any => tokens_before,
            };
            events.emit(AgentEvent::CompactionFailed {
                reason: format!(
                    "not enough gain: {tokens_before} -> {tokens_after} tokens, target {target}"
                ),
            });
            return Err(CompactionFailure::Compaction(
                CompactionError::Insufficient {
                    before: tokens_before,
                    after: tokens_after,
                    target,
                },
            ));
        }
        // `T_after <= B` but over the 75% target: commit and stop auto-compacting this turn.
        let below_target = matches!(
            gain,
            Gain::Budget { input_budget }
                if tokens_after.saturating_mul(4) > input_budget.saturating_mul(3)
        );

        // ADR 0018 §revision 与并发: the candidate only commits if the source view is still
        // what it was planned from. This cannot happen with one writer, but is checked and
        // tested as the ADR requires.
        #[cfg(feature = "testing")]
        if self.tamper_source_on_compaction {
            self.tamper_source_on_compaction = false;
            self.transcript_revision += 1;
        }
        if self.transcript_revision != source_revision || self.transcript.len() != source_len {
            events.emit(AgentEvent::CompactionFailed {
                reason: "the source view changed while the summary was made".into(),
            });
            return Err(CompactionFailure::Compaction(
                CompactionError::SourceChanged,
            ));
        }

        if let Some(journal) = self.journal.as_ref()
            && let Err(error) = journal
                .append(JournalRecord::ContextCompacted {
                    revision,
                    source_revision,
                    source_len,
                    summary: summary.clone(),
                    first_kept: plan.first_kept,
                    tokens_before,
                    tokens_after,
                    below_target,
                    model: compaction::COMPACTION_MODEL_CURRENT.to_owned(),
                })
                .await
        {
            self.recovery_required = true;
            return Err(CompactionFailure::Journal(error));
        }

        let first_kept = plan.first_kept;
        if let Err(error) = self.replace_transcript(new_history, revision, events).await {
            let failure = match error {
                crate::agent::ReplaceError::Journal(error) => CompactionFailure::Journal(error),
                other => CompactionFailure::Compaction(CompactionError::InvalidSummary(
                    other.to_string(),
                )),
            };
            events.emit(AgentEvent::CompactionFailed {
                reason: failure.to_string(),
            });
            return Err(failure);
        }

        events.emit(AgentEvent::CompactionFinished {
            revision,
            tokens_before,
            tokens_after,
            below_target,
        });
        Ok(CompactionStep::Applied {
            revision,
            tokens_before,
            tokens_after,
            below_target,
            first_kept,
            size_after,
        })
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
    /// Set once an automatic compaction in this turn came out below the 75% target (ADR
    /// 0018). The rest of the turn must not compact again; an over-budget request then fails
    /// as `InsufficientCompaction` instead.
    below_target_compaction: bool,
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
            below_target_compaction: false,
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

    /// Records a committed compaction against this turn's bookkeeping (ADR 0018 §B):
    /// `turn_start` tracks the first retained message of this turn in the new history (index 0
    /// is the summary), and a below-target compaction stops further automatic compaction this
    /// turn. Every compaction path calls this.
    fn note_compaction(&mut self, below_target: bool, first_kept: usize) {
        self.turn_start = if self.turn_start < first_kept {
            1
        } else {
            self.turn_start - first_kept + 1
        };
        self.below_target_compaction |= below_target;
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

/// The visible text of a message's or response's content parts, concatenated.
fn text_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
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
        let mut request_size = if agent.config.context_budget.is_some() {
            Some(budget::request_size(&request))
        } else {
            None
        };
        if let Some((config, size)) = agent.config.context_budget.zip(request_size) {
            let report = budget::project(&config, self.run.session.budget_baseline, size);
            let over = budget::over_budget(&report);
            let tokens_before = report.projected_tokens;
            self.run.session.budget_status = if over {
                BudgetStatus::Over(report)
            } else {
                BudgetStatus::Ok(report)
            };
            // ADR 0018: over the input budget `B` the request is normally compacted before it
            // is sent. `--no-auto-compact` sends it anyway; a previous below-target compaction
            // in this turn fails instead of compacting again.
            if over && agent.config.auto_compact {
                if self.run.below_target_compaction {
                    events.emit(AgentEvent::CompactionFailed {
                        reason: "a previous compaction this turn was below target".into(),
                    });
                    return Err(self
                        .run
                        .fail(RunError::Compaction(CompactionError::BelowTarget), events));
                }
                match self
                    .maybe_compact(
                        CompactCx {
                            interrupt: &interrupt,
                            hcx: &hcx,
                            budget: &config,
                            events,
                        },
                        &mut request,
                        tokens_before,
                    )
                    .await
                {
                    Ok(CompactionDecision::Cancelled(reason)) => {
                        return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
                    }
                    Ok(CompactionDecision::NotNeeded) => {}
                    Ok(CompactionDecision::Applied { size, report, .. }) => {
                        request_size = Some(size);
                        self.run.session.budget_status = BudgetStatus::Ok(report);
                    }
                    Err(RunError::Journal(e)) => {
                        return Err(self.run.fail_journal(e, events));
                    }
                    Err(other) => return Err(self.run.fail(other, events)),
                }
            }
        } else {
            self.run.session.budget_status = BudgetStatus::Disabled;
        }

        events.emit(AgentEvent::ModelRequestStarted {
            model_call: self.run.model_calls,
            budget: self.run.session.context_budget_status().report().cloned(),
        });
        // ADR 0018 §兜底: a provider-reported context overflow compacts to half the reference
        // and retries this request **once**. A model error commits no partial output (the tap
        // below is discarded), so the fallback is safe. `overflow_retried` bounds it to one.
        let mut overflow_retried = false;
        let response = loop {
            // `T_before`: the projection of the request about to be sent. The fallback needs
            // it even with no configured budget — the window-unknown path is exactly this one.
            let size = budget::request_size(&request);
            let bytes_per_token = agent
                .config
                .context_budget
                .map(|config| config.bytes_per_token)
                .unwrap_or(crate::budget::DEFAULT_BYTES_PER_TOKEN);
            let tokens_before = match (
                agent.config.context_budget,
                self.run.session.budget_baseline,
            ) {
                (Some(config), Some(baseline)) => {
                    budget::project(&config, Some(baseline), size).projected_tokens
                }
                _ => size.tokens(bytes_per_token),
            };

            let tap = StreamTap {
                host: events,
                text: Mutex::new(String::new()),
                reasoning: Mutex::new(String::new()),
            };
            let mcx = ModelContext {
                cancellation: interrupt.token.child_token(),
                events: &tap,
            };
            match race(&interrupt, agent.model.complete(request.clone(), mcx)).await {
                Raced::Interrupted(reason) => {
                    // The model future is gone; whatever it emitted is in the tap. Any
                    // non-empty prefix — visible text **or** reasoning — becomes an
                    // interrupted assistant message through the journal before the turn
                    // converges (F3/F5, revised F4: "empty" means both are empty). The append
                    // itself is never cancelled (D3) and runs no hooks.
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
                Raced::Done(Err(error)) => {
                    let ModelError::ContextOverflow(message) = &error else {
                        // ADR 0017: any other model error fails the turn, no compaction.
                        return Err(self.run.fail(RunError::Model(error), events));
                    };
                    if overflow_retried {
                        let error = ModelError::ContextOverflow(format!(
                            "{message} (compacted once and retried; still over the window)"
                        ));
                        return Err(self.run.fail(RunError::Model(error), events));
                    }
                    overflow_retried = true;
                    // `--no-auto-compact` disables the proactive path only; the fallback
                    // stays (ADR 0018 §关闭). The reference is `min(T_before, B)` when the
                    // window is known, otherwise `T_before`.
                    let reference = match agent.config.context_budget {
                        Some(config) => tokens_before.min(config.input_budget_tokens),
                        None => tokens_before,
                    };
                    let step = self
                        .run
                        .session
                        .compact_transcript(CompactionRun {
                            interrupt: &interrupt,
                            hcx: Some(&hcx),
                            request: &mut request,
                            tokens_before,
                            keep: compaction::MAX_KEEP_TARGET,
                            summary_budget: agent
                                .config
                                .context_budget
                                .map(|config| config.input_budget_tokens),
                            gain: Gain::Half { reference },
                            trigger: CompactionTrigger::Overflow,
                            instructions: None,
                            events,
                        })
                        .await;
                    match step {
                        Ok(CompactionStep::Applied {
                            below_target,
                            first_kept,
                            ..
                        }) => {
                            self.run.note_compaction(below_target, first_kept);
                            continue;
                        }
                        Ok(CompactionStep::NothingToCompact) => {
                            let error = ModelError::ContextOverflow(format!(
                                "{message} (nothing to compact)"
                            ));
                            return Err(self.run.fail(RunError::Model(error), events));
                        }
                        Ok(CompactionStep::Cancelled(reason)) => {
                            return Ok(self.run.finish(RunStatus::Cancelled(reason), events));
                        }
                        Err(CompactionFailure::Journal(error)) => {
                            return Err(self.run.fail_journal(error, events));
                        }
                        Err(CompactionFailure::Hook { point, error }) => {
                            return Err(self.run.fail_hook(point, error, events));
                        }
                        Err(CompactionFailure::Compaction(error)) => {
                            return Err(self.run.fail(RunError::Compaction(error), events));
                        }
                    }
                }
                Raced::Done(Ok(response)) => break response,
            }
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

    /// The proactive compaction for the pending request (ADR 0018 §B): the projection is over
    /// `B`, `auto_compact` is on, and no below-target compaction happened yet this turn. The
    /// work is shared with the overflow fallback and a manual `/compact` through
    /// [`AgentSession::compact_transcript`].
    async fn maybe_compact(
        &mut self,
        cx: CompactCx<'_>,
        request: &mut ModelRequest,
        tokens_before: u64,
    ) -> Result<CompactionDecision, RunError> {
        let CompactCx {
            interrupt,
            hcx,
            budget,
            events,
        } = cx;
        let step = self
            .run
            .session
            .compact_transcript(CompactionRun {
                interrupt,
                hcx: Some(hcx),
                request,
                tokens_before,
                keep: compaction::keep_target(budget.input_budget_tokens),
                summary_budget: Some(budget.input_budget_tokens),
                gain: Gain::Budget {
                    input_budget: budget.input_budget_tokens,
                },
                trigger: CompactionTrigger::Budget,
                instructions: None,
                events,
            })
            .await;
        match step {
            Ok(CompactionStep::Applied {
                below_target,
                first_kept,
                size_after,
                ..
            }) => {
                self.run.note_compaction(below_target, first_kept);
                let report = budget::project(budget, None, size_after);
                Ok(CompactionDecision::Applied {
                    size: size_after,
                    report,
                })
            }
            Ok(CompactionStep::NothingToCompact) => Ok(CompactionDecision::NotNeeded),
            Ok(CompactionStep::Cancelled(reason)) => Ok(CompactionDecision::Cancelled(reason)),
            Err(CompactionFailure::Journal(error)) => Err(self.run.fail_journal(error, events)),
            Err(CompactionFailure::Hook { point, error }) => {
                Err(self.run.fail_hook(point, error, events))
            }
            Err(CompactionFailure::Compaction(error)) => Err(RunError::Compaction(error)),
        }
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

use std::sync::Arc;

use serde_json::Value;

use crate::budget::{BudgetStatus, ContextBudget, RequestSize};
use crate::event::{AgentEvent, EventSink};
use crate::hooks::Hooks;
use crate::journal::{JournalError, JournalRecord, TranscriptJournal};
use crate::message::Message;
use crate::model::{Model, ModelRequest};
use crate::outcome::{ResumeState, RunError, RunOutcome, SessionError};
use crate::preflight::{ToolRegistry, validate_history};
use crate::tool::{ToolError, ToolProvider, ToolSession};
use crate::turn::{ActiveRun, TurnOptions};

/// Default cap on model requests per turn.
pub const DEFAULT_MAX_MODEL_CALLS: u32 = 32;

/// Why a host-requested transcript replacement was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("the session is unusable: it is poisoned, closed, or needs recovery")]
    Unusable,
    #[error("invalid history: {0}")]
    InvalidHistory(String),
    #[error("journal: {0}")]
    Journal(#[from] JournalError),
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    /// Model requests allowed per turn. A turn never runs tools it has no budget to report back.
    pub max_model_calls: u32,
    /// Passed through to the model adapter unchanged.
    pub model_settings: Value,
    /// Context window and threshold for the budget guard (ADR 0015). `None` disables the
    /// guard outright: the core does not guess a window, so no number is ever claimed.
    pub context_budget: Option<ContextBudget>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_model_calls: DEFAULT_MAX_MODEL_CALLS,
            model_settings: Value::Null,
            context_budget: None,
        }
    }
}

/// The injected model, tool provider and hooks. Share it with `Arc` and open sessions from it.
pub struct Agent<M, P, H = ()> {
    pub(crate) model: M,
    pub(crate) tools: P,
    pub(crate) hooks: H,
    pub(crate) config: AgentConfig,
}

impl<M: Model, P: ToolProvider> Agent<M, P, ()> {
    pub fn new(model: M, tools: P) -> Self {
        Self {
            model,
            tools,
            hooks: (),
            config: AgentConfig::default(),
        }
    }
}

impl<M: Model, P: ToolProvider, H: Hooks> Agent<M, P, H> {
    pub fn with_hooks<H2: Hooks>(self, hooks: H2) -> Agent<M, P, H2> {
        Agent {
            model: self.model,
            tools: self.tools,
            hooks,
            config: self.config,
        }
    }

    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub async fn open_session(self: &Arc<Self>) -> Result<AgentSession<M, P, H>, SessionError> {
        self.open_session_with_history(Vec::new()).await
    }

    /// Like [`Agent::open_session_with_history`], but every later transcript change of the
    /// session is first appended to `journal` and only applied to memory once that succeeds.
    /// The `history` itself is not re-journaled (D8): it is assumed to already be durable.
    pub async fn open_session_with_journal(
        self: &Arc<Self>,
        history: Vec<Message>,
        journal: Arc<dyn TranscriptJournal>,
    ) -> Result<AgentSession<M, P, H>, SessionError> {
        self.open_session_inner(history, Some(journal)).await
    }

    /// Validates `history`, then opens the tool session (for MCP, this starts the servers).
    /// Nothing half-open is returned: if the tool declarations are invalid, the tool session
    /// is closed before the error is returned.
    pub async fn open_session_with_history(
        self: &Arc<Self>,
        history: Vec<Message>,
    ) -> Result<AgentSession<M, P, H>, SessionError> {
        self.open_session_inner(history, None).await
    }

    async fn open_session_inner(
        self: &Arc<Self>,
        history: Vec<Message>,
        journal: Option<Arc<dyn TranscriptJournal>>,
    ) -> Result<AgentSession<M, P, H>, SessionError> {
        validate_history(&history)?;
        #[cfg(feature = "journal-selfcheck")]
        let mut journal = journal;
        #[cfg(feature = "journal-selfcheck")]
        // Sessions without their own journal get a recording one, so session end can prove
        // the journal would replay the transcript (AC-A1-2). Sessions with a host journal
        // cannot be read back here; their tests assert on the journal directly.
        let selfcheck = match &journal {
            None => {
                let recorder = Arc::new(crate::testing::RecordingJournal::new());
                journal = Some(Arc::clone(&recorder) as Arc<dyn TranscriptJournal>);
                Some(recorder)
            }
            Some(_) => None,
        };
        let mut tools = self.tools.open_session().await?;
        let registry = match ToolRegistry::new(tools.tool_specs()) {
            Ok(registry) => registry,
            Err(e) => {
                // The spec error is the one worth reporting; a close failure adds nothing.
                let _ = tools.close().await;
                return Err(e);
            }
        };
        Ok(AgentSession {
            agent: Arc::clone(self),
            tools,
            registry,
            transcript: history,
            journal,
            #[cfg(feature = "journal-selfcheck")]
            selfcheck,
            budget_baseline: None,
            budget_status: BudgetStatus::Disabled,
            poisoned: false,
            recovery_required: false,
            close_state: CloseState::Open,
            stranded_steering: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseState {
    Open,
    /// A close was attempted but has not succeeded. No new turns; `close()` retries.
    Closing,
    Closed,
}

/// One in-memory conversation. Holds the transcript and the open tool session, which lives
/// across turns. Turns run one at a time; `ActiveRun` borrows the session mutably.
///
/// Call `close().await` when done. Dropping the session instead releases tool resources
/// abruptly (for MCP, the server processes are killed).
pub struct AgentSession<M: Model, P: ToolProvider, H: Hooks> {
    pub(crate) agent: Arc<Agent<M, P, H>>,
    pub(crate) tools: P::Session,
    pub(crate) registry: ToolRegistry,
    pub(crate) transcript: Vec<Message>,
    /// Mounted at open time; `None` keeps today's journal-free behavior.
    pub(crate) journal: Option<Arc<dyn TranscriptJournal>>,
    pub(crate) poisoned: bool,
    /// Set when a journal append failed: memory and journal may differ, so the session
    /// refuses new turns until the host recovers it (D6).
    pub(crate) recovery_required: bool,
    pub(crate) close_state: CloseState,
    /// Steering from turns that ended without returning an outcome.
    pub(crate) stranded_steering: Vec<Message>,
    /// The recording journal this session was silently given, under `journal-selfcheck`.
    #[cfg(feature = "journal-selfcheck")]
    pub(crate) selfcheck: Option<Arc<crate::testing::RecordingJournal>>,
    /// The last provider-reported `input` and the request size that produced it, per
    /// session (ADR 0015 C1). It survives across turns, so a baseline projection has a
    /// real fact rather than restarting from an estimate each turn.
    pub(crate) budget_baseline: Option<(u64, RequestSize)>,
    /// The guard's last computation, host-visible through [`AgentSession::context_budget_status`].
    pub(crate) budget_status: BudgetStatus,
}

impl<M: Model, P: ToolProvider, H: Hooks> AgentSession<M, P, H> {
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    /// The context-budget guard's last computation for this session (ADR 0015 C1).
    /// `Disabled` means no number is claimed: no window is configured, or no request has
    /// been checked yet. `Ok`/`Blocked` carry a report whose `source` says whether the
    /// baseline was provider-reported or a local estimate.
    pub fn context_budget_status(&self) -> &BudgetStatus {
        &self.budget_status
    }

    /// Restores the context-budget baseline from a usage report read back from the journal
    /// (ADR 0015 C1, the resume path). Only the provider-reported `input` and the transcript
    /// prefix that produced it are durable; the exact request is not stored, so Core
    /// re-derives its size with the **current** system prompt, tools and settings over the
    /// first `prefix_len` transcript messages. A later configuration change therefore biases
    /// the recomputed baseline (recorded in ADR 0015).
    ///
    /// `prefix_len` is the transcript length at the moment that request was made, i.e. the
    /// index of the assistant message that the `ModelUsageRecorded` event follows. Returns
    /// `false` and leaves the baseline untouched when `prefix_len` exceeds the transcript
    /// (a replaced or shorter history), which keeps the projection a pure estimate instead
    /// of inventing a baseline.
    pub fn restore_budget_baseline(&mut self, prefix_len: usize, input_tokens: u64) -> bool {
        if prefix_len > self.transcript.len() {
            return false;
        }
        let request = ModelRequest {
            system_prompt: self.agent.config.system_prompt.clone(),
            messages: self.transcript[..prefix_len].to_vec(),
            tools: self.registry.specs().to_vec(),
            settings: self.agent.config.model_settings.clone(),
        };
        self.budget_baseline = Some((input_tokens, crate::budget::request_size(&request)));
        true
    }

    /// Replaces the transcript with host-supplied history and records the host's `revision`.
    ///
    /// The caller-supplied revision is opaque to Core: Core does not require it to increase;
    /// the host owns that version space. `&mut self` makes an in-flight turn impossible, so no
    /// redundant runtime "running" check is needed. The replacement is journaled as one
    /// `TranscriptReplaced` marker followed by messages indexed from zero, then atomically
    /// swapped into memory. A failed append leaves memory unchanged and requires recovery.
    ///
    /// Replacement messages intentionally do not emit `TranscriptAppended` one by one; a large
    /// replacement would otherwise flood the host. One `TranscriptReplaced` event follows the
    /// successful in-memory swap instead.
    pub async fn replace_transcript(
        &mut self,
        history: Vec<Message>,
        revision: u64,
        events: &dyn EventSink,
    ) -> Result<(), ReplaceError> {
        if self.recovery_required || self.poisoned || self.close_state != CloseState::Open {
            return Err(ReplaceError::Unusable);
        }
        validate_history(&history).map_err(|error| match error {
            SessionError::InvalidHistory(message) => ReplaceError::InvalidHistory(message),
            _ => unreachable!("validate_history only returns invalid-history errors"),
        })?;

        let len = history.len();
        if let Some(journal) = &self.journal {
            if let Err(error) = journal
                .append(JournalRecord::TranscriptReplaced { revision, len })
                .await
            {
                self.recovery_required = true;
                return Err(ReplaceError::Journal(error));
            }
            for (index, message) in history.iter().cloned().enumerate() {
                if let Err(error) = journal
                    .append(JournalRecord::Message { index, message })
                    .await
                {
                    self.recovery_required = true;
                    return Err(ReplaceError::Journal(error));
                }
            }
            // ADR 0019 §1: the commit record is the point of no return. It lands before the
            // memory swap, so a crash before it recovers the old history and a crash after
            // it recovers the new one.
            if let Err(error) = journal
                .append(JournalRecord::TranscriptReplacementCommitted { revision, len })
                .await
            {
                self.recovery_required = true;
                return Err(ReplaceError::Journal(error));
            }
        }
        self.transcript = history;
        events.emit(AgentEvent::TranscriptReplaced { revision, len });
        Ok(())
    }

    /// The single entry every transcript modification goes through: journal first, then
    /// memory, then the [`AgentEvent::TranscriptAppended`] event. A failed append changes
    /// neither memory nor events (S1). Without a journal this is just a push plus the event.
    ///
    /// The append is awaited to completion and never raced against cancellation (D3), and
    /// appends within a session are strictly serialized (D4) because callers await this
    /// before making the next change.
    pub(crate) async fn commit(
        &mut self,
        message: Message,
        events: &dyn EventSink,
    ) -> Result<(), JournalError> {
        commit_parts(self.journal.as_ref(), &mut self.transcript, message, events).await
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn start_turn(
        &mut self,
        input: impl Into<Message>,
    ) -> Result<ActiveRun<'_, M, P, H>, SessionError> {
        self.start_turn_with(input, TurnOptions::default())
    }

    /// Starts a turn with `input`. Does no I/O: with a journal mounted, the input is
    /// committed (journaled, then applied to memory) at the start of the turn's first
    /// `step()`, so `transcript()` does not show it until then (D9).
    pub fn start_turn_with(
        &mut self,
        input: impl Into<Message>,
        options: TurnOptions,
    ) -> Result<ActiveRun<'_, M, P, H>, SessionError> {
        if self.recovery_required {
            return Err(SessionError::RecoveryRequired);
        }
        if self.poisoned {
            return Err(SessionError::Poisoned);
        }
        if self.close_state != CloseState::Open {
            return Err(SessionError::Closed);
        }
        let input = input.into();
        if !input.is_user() {
            return Err(SessionError::NotUserMessage);
        }
        Ok(ActiveRun::new(self, options, input))
    }

    /// Starts a turn and runs it to the end. Owns nothing borrowed, so it suits `tokio::spawn`.
    pub async fn run_turn(
        &mut self,
        input: impl Into<Message>,
        options: TurnOptions,
        events: &dyn EventSink,
    ) -> Result<RunOutcome, RunError> {
        let mut run = self.start_turn_with(input, options)?;
        run.run(events).await
    }

    /// The transcript plus any steering that was never applied, for opening a new session
    /// after this one was poisoned.
    pub fn history_for_resume(&self) -> ResumeState {
        ResumeState {
            history: self.transcript.clone(),
            unapplied_steering: self.stranded_steering.clone(),
        }
    }

    /// Closes the tool session.
    ///
    /// The first call stops new turns, even if it fails or its future is dropped. The session
    /// counts as closed only once `ToolSession::close` succeeds; until then, calling `close()`
    /// again retries it. After a successful close, further calls return `Ok(())`.
    pub async fn close(&mut self) -> Result<(), ToolError> {
        if self.close_state == CloseState::Closed {
            return Ok(());
        }
        self.close_state = CloseState::Closing;
        self.tools.close().await?;
        self.close_state = CloseState::Closed;
        #[cfg(feature = "journal-selfcheck")]
        self.verify_journal_replays();
        Ok(())
    }

    /// True once `close()` has succeeded.
    pub fn is_closed(&self) -> bool {
        self.close_state == CloseState::Closed
    }

    /// Replays the session's recording journal and compares it with the live transcript
    /// (AC-A1-2). After a drop (`poisoned`), memory may only lead the journal with
    /// `interrupted` tool results (D7). Only compiled under `journal-selfcheck`.
    #[cfg(feature = "journal-selfcheck")]
    fn verify_journal_replays(&self) {
        // A test that is already failing must not abort the process from a panic inside
        // this Drop-time check.
        if std::thread::panicking() {
            return;
        }
        let Some(recorder) = &self.selfcheck else {
            return;
        };
        let all_records = recorder.records();
        if let Some(replacement) = all_records
            .iter()
            .rposition(|record| matches!(record, JournalRecord::TranscriptReplaced { .. }))
        {
            let replay = crate::journal::replay(&all_records[replacement..])
                .expect("selfcheck journal replacement must replay");
            let transcript = &self.transcript;
            assert!(
                replay.len() <= transcript.len(),
                "replacement journal has {} messages but transcript has {}",
                replay.len(),
                transcript.len()
            );
            assert_eq!(&transcript[..replay.len()], replay.as_slice());
            if self.poisoned {
                for message in &transcript[replay.len()..] {
                    assert!(
                        matches!(message, Message::Tool { result, .. }
                            if result.error_code.as_deref() == Some("interrupted")),
                        "after a drop, memory may only add `interrupted` results, found {message:?}"
                    );
                }
            } else {
                assert_eq!(
                    replay.len(),
                    transcript.len(),
                    "journal replay does not match the transcript"
                );
            }
            return;
        }
        let records: Vec<JournalRecord> = all_records
            .into_iter()
            .filter(|r| matches!(r, JournalRecord::Message { .. }))
            .collect();
        let replay: Vec<&Message> = records
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Message { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        let transcript = &self.transcript;
        // The opening history is never journaled (D8), so the replay lines up with the
        // transcript from the first record's index on.
        let base = match records.first() {
            Some(JournalRecord::Message { index, .. }) => *index,
            // No records: nothing was written through this recorder, nothing to compare.
            _ => return,
        };
        if self.poisoned {
            assert!(
                base + replay.len() <= transcript.len(),
                "journal ({} records at {base}) is not a prefix of the transcript ({} messages)",
                replay.len(),
                transcript.len()
            );
            for (i, recorded) in replay.iter().enumerate() {
                assert_eq!(
                    recorded,
                    &&transcript[base + i],
                    "journal record {} diverges from the transcript",
                    base + i
                );
            }
            for message in &transcript[base + replay.len()..] {
                assert!(
                    matches!(message, Message::Tool { result, .. }
                        if result.error_code.as_deref() == Some("interrupted")),
                    "after a drop, memory may only add `interrupted` results, found {message:?}"
                );
            }
        } else {
            assert_eq!(
                base + replay.len(),
                transcript.len(),
                "journal replay does not match the transcript"
            );
            for (i, recorded) in replay.iter().enumerate() {
                assert_eq!(
                    recorded,
                    &&transcript[base + i],
                    "journal record {} diverges from the transcript",
                    base + i
                );
            }
        }
    }
}

#[cfg(feature = "journal-selfcheck")]
impl<M: Model, P: ToolProvider, H: Hooks> Drop for AgentSession<M, P, H> {
    fn drop(&mut self) {
        self.verify_journal_replays();
    }
}

/// The one implementation of "journal, then memory, then event" (S1). `AgentSession::commit`
/// and the batch context share it; nothing else may touch a mounted session's transcript.
/// The pieces arrive separately because running tool futures hold `tools` across commits.
pub(crate) async fn commit_parts(
    journal: Option<&Arc<dyn TranscriptJournal>>,
    transcript: &mut Vec<Message>,
    message: Message,
    events: &dyn EventSink,
) -> Result<(), JournalError> {
    let index = transcript.len();
    if let Some(journal) = journal {
        journal
            .append(JournalRecord::Message {
                index,
                message: message.clone(),
            })
            .await?;
    }
    transcript.push(message.clone());
    events.emit(AgentEvent::TranscriptAppended { index, message });
    Ok(())
}

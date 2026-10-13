//! The UI state: transcript entries, the input buffer, scroll-follow logic, and key
//! handling. It contains no terminal I/O, so the whole thing is driven by tests in memory.

use agent_core::{AgentEvent, BudgetReport, ContentPart, Message, RunStatus, UsageSource};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A tool call waiting for the human. `reply` is the channel the permission layer awaits; the
/// answer is sent by the UI thread, so the waiting future never blocks it.
pub struct ApprovalPrompt {
    pub request: ApprovalRequest,
    pub reply: tokio::sync::oneshot::Sender<ApprovalOutcome>,
}

/// What kind of transcript line an entry is. The renderer maps this to a style; the tests
/// assert that reasoning and assistant text are distinguishable by style and position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    /// The model's separate reasoning channel (ADR 0014); never sent to a provider.
    Reasoning,
    ToolCall,
    ToolResult,
    Notice,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
    /// True while the entry is fed by live deltas and not yet a committed message.
    pub live: bool,
    /// The committed assistant message was an interrupted prefix (ADR 0003).
    pub interrupted: bool,
    pub is_error: bool,
}

impl Entry {
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            live: false,
            interrupted: false,
            is_error: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Running,
    Cancelled,
}

impl TurnState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The wording for a budget report. ADR 0015 C1: a reported number, a reported baseline plus
/// an estimated growth, and a pure estimate must be distinguishable, and a mixed value must
/// **not** be presented as a report.
pub fn budget_text(report: &BudgetReport) -> String {
    let window = format!(
        " of {} at {}%",
        report.window_tokens, report.threshold_percent
    );
    match report.source {
        UsageSource::Reported => format!("context {}{window} (reported)", report.projected_tokens),
        UsageSource::ReportedPlusEstimate => format!(
            "context {}{window} (reported {} + estimated {})",
            report.projected_tokens,
            report.baseline_input_tokens.unwrap_or(0),
            report.estimated_growth_tokens.unwrap_or(0)
        ),
        UsageSource::Estimated => {
            format!("context ~{}{window} (estimated)", report.projected_tokens)
        }
    }
}

/// The status bar's budget segment. A configured window with no report yet says the guard is
/// on; no window says it is off, and never invents a number (ADR 0015 C1's `Disabled` row).
pub fn budget_status_text(window: Option<u64>, report: Option<&BudgetReport>) -> String {
    match (window, report) {
        (None, _) => "context guard off (no window configured)".to_string(),
        (Some(_), Some(report)) => budget_text(report),
        (Some(window), None) => format!("context guard on ({window} tokens)"),
    }
}

/// What the event loop must do with the inbox after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Send {
    /// A new turn: `Inbox::followup`.
    Followup(String),
    /// Redirect the running turn: `Inbox::steer`.
    Steer(String),
    /// Cancel the running turn.
    Cancel,
    /// Continue once past a context-budget stop (ADR 0015 C5): the next turn's first request
    /// is allowed through, then the guard applies again.
    ContinueBudget,
    Quit,
}

pub struct App {
    pub model_name: String,
    pub session_id: String,
    pub entries: Vec<Entry>,
    pub input: String,
    pub scroll: u16,
    /// True while the view is pinned to the newest line. Scrolling up clears it; scrolling
    /// back to the bottom, or new content while it is set, pins it again.
    pub follow: bool,
    pub turn: TurnState,
    pub quit: bool,
    pub note: Option<String>,
    /// Set while a tool call waits for the human; keys answer it instead of typing.
    pub pending_approval: Option<ApprovalPrompt>,
    /// The configured context window, if any; `None` means the guard is off.
    pub context_window: Option<u64>,
    /// The most recent budget report, for the status bar.
    pub budget_report: Option<BudgetReport>,
    /// A budget stop waiting for the user's choice. The keys work only while the input box is
    /// empty, so ordinary typing is never swallowed.
    pub budget_stop: Option<BudgetReport>,
    pub total_lines: u16,
    pub view_height: u16,
    pub max_scroll: u16,
}

impl App {
    pub fn new(model_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            session_id: session_id.into(),
            entries: Vec::new(),
            input: String::new(),
            scroll: 0,
            follow: true,
            turn: TurnState::Idle,
            quit: false,
            note: None,
            pending_approval: None,
            context_window: None,
            budget_report: None,
            budget_stop: None,
            total_lines: 0,
            view_height: 0,
            max_scroll: 0,
        }
    }

    /// Records the configured context window (the guard is off when this is `None`).
    pub fn set_context_window(&mut self, window: Option<u64>) {
        self.context_window = window;
    }

    /// Records the transcript viewport. Called by the renderer with the measured transcript
    /// size, so scroll math works on wrapped lines without the app knowing how to wrap.
    pub fn set_viewport(&mut self, view_height: u16, total_lines: u16) {
        self.view_height = view_height;
        self.total_lines = total_lines;
        self.max_scroll = total_lines.saturating_sub(view_height);
        if self.follow {
            self.scroll = self.max_scroll;
        } else {
            self.scroll = self.scroll.min(self.max_scroll);
        }
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    fn set_scroll(&mut self, next: u16) {
        self.scroll = next.min(self.max_scroll);
        self.follow = self.scroll >= self.max_scroll;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let next = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            self.scroll.saturating_add(delta as u16)
        };
        self.set_scroll(next);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll;
        self.follow = true;
    }

    /// Shows an approval prompt. If one was already up it is denied rather than dropped: the
    /// permission layer awaits one call at a time, so this should not happen, and a silent
    /// drop would leave that call waiting forever.
    pub fn request_approval(&mut self, prompt: ApprovalPrompt) {
        if let Some(previous) = self.pending_approval.replace(prompt) {
            let _ = previous.reply.send(ApprovalOutcome::Deny {
                reason: Some("superseded by another approval request".into()),
            });
        }
    }

    /// Answers the pending prompt and clears it. A no-op when nothing is pending.
    pub fn answer_approval(&mut self, outcome: ApprovalOutcome) {
        if let Some(prompt) = self.pending_approval.take() {
            let _ = prompt.reply.send(outcome);
        }
    }

    /// Handles one key event and returns the inbox action it implies, if any.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Send> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Ctrl-C always means "cancel the turn / quit", even while a prompt is up: the
        // permission layer races the approval against cancellation, so the waiting call then
        // resolves as a denial and the prompt is cleared by `RunFinished`.
        if ctrl && key.code == KeyCode::Char('c') {
            return if self.turn == TurnState::Running {
                Some(Send::Cancel)
            } else {
                self.quit = true;
                Some(Send::Quit)
            };
        }
        // A budget stop offers continue/dismiss, but only while the input box is empty: a
        // typed message must never be swallowed by a prompt key.
        if self.budget_stop.is_some() && self.input.is_empty() {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    self.budget_stop = None;
                    return Some(Send::ContinueBudget);
                }
                KeyCode::Esc => {
                    self.budget_stop = None;
                    return None;
                }
                _ => {}
            }
        }
        // While a prompt is up, keys answer it and never type into the input box.
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.answer_approval(ApprovalOutcome::AllowOnce)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.answer_approval(ApprovalOutcome::AllowForSession)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.answer_approval(ApprovalOutcome::Deny { reason: None })
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.turn == TurnState::Running {
                    Some(Send::Cancel)
                } else {
                    self.quit = true;
                    Some(Send::Quit)
                }
            }
            // Newline: Ctrl-J and Alt-Enter work on every terminal; Shift-Enter needs the
            // enhanced keyboard protocol (see the README's manual-test list).
            KeyCode::Char('j') if ctrl => {
                self.input.push('\n');
                None
            }
            KeyCode::Enter if shift || alt => {
                self.input.push('\n');
                None
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                if text.trim().is_empty() {
                    return None;
                }
                if self.turn == TurnState::Running {
                    Some(Send::Steer(text))
                } else {
                    Some(Send::Followup(text))
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.push(c);
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::PageUp => {
                self.scroll_by(-(self.view_height.max(1) as i32));
                None
            }
            KeyCode::PageDown => {
                self.scroll_by(self.view_height.max(1) as i32);
                None
            }
            KeyCode::Up => {
                self.scroll_by(-1);
                None
            }
            KeyCode::Down => {
                self.scroll_by(1);
                None
            }
            KeyCode::Home => {
                self.set_scroll(0);
                None
            }
            KeyCode::End => {
                self.scroll_to_bottom();
                None
            }
            KeyCode::Esc => {
                if self.turn == TurnState::Running {
                    Some(Send::Cancel)
                } else {
                    self.quit = true;
                    Some(Send::Quit)
                }
            }
            _ => None,
        }
    }

    /// Applies a live event. Deltas build `live` entries; the committed message
    /// (`TranscriptAppended`) replaces them, so a dropped delta can never leave a duplicate.
    pub fn apply_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStarted | AgentEvent::ModelRequestStarted { .. } => {
                self.turn = TurnState::Running;
                // A new turn started, so a stop that was waiting for a choice is stale.
                self.budget_stop = None;
            }
            AgentEvent::ModelTextDelta { text } => self.append_live(EntryKind::Assistant, text),
            AgentEvent::ModelReasoningDelta { text } => {
                self.append_live(EntryKind::Reasoning, text)
            }
            AgentEvent::ToolStarted { name, .. } => {
                self.note = Some(format!("tool {name} running"));
            }
            AgentEvent::TranscriptAppended { message, .. } => self.push_message(message),
            AgentEvent::RunFinished { status } => {
                self.turn = match status {
                    RunStatus::Cancelled(_) => TurnState::Cancelled,
                    _ => TurnState::Idle,
                };
                self.note = None;
                // The turn is over, so no approval can still be waiting: a cancelled turn
                // already resolved the hook's `ask` as a denial. Dropping the prompt (and its
                // reply channel) leaves nothing behind.
                self.pending_approval = None;
                // A context-budget stop is a turn outcome the host must act on (ADR 0015 C5):
                // show the reason and the numbers, and offer continue/dismiss.
                if let RunStatus::ContextBudget(report) = status {
                    self.budget_report = Some(report.clone());
                    self.budget_stop = Some(report.clone());
                    self.entries.push(Entry::new(
                        EntryKind::Notice,
                        format!(
                            "stopped before the next request: {} — press c to continue once, \
                             Esc to dismiss",
                            budget_text(report)
                        ),
                    ));
                }
            }
            _ => {}
        }
    }

    fn append_live(&mut self, kind: EntryKind, text: &str) {
        match self.entries.last_mut() {
            Some(last) if last.live && last.kind == kind => last.text.push_str(text),
            _ => {
                let mut entry = Entry::new(kind, text);
                entry.live = true;
                self.entries.push(entry);
            }
        }
    }

    /// Appends a committed message as transcript entries. Used for live commits and for the
    /// history of a resumed session.
    pub fn push_message(&mut self, message: &Message) {
        // The committed message is the truth; discard the partial live view of it.
        self.entries.retain(|entry| !entry.live);
        match message {
            Message::User { content, .. } => {
                self.entries
                    .push(Entry::new(EntryKind::User, text_of(content)));
            }
            Message::Assistant {
                content,
                tool_calls,
                reasoning,
                interrupted,
                ..
            } => {
                let reasoning = text_of(reasoning);
                if !reasoning.is_empty() {
                    self.entries
                        .push(Entry::new(EntryKind::Reasoning, reasoning));
                }
                let mut entry = Entry::new(EntryKind::Assistant, text_of(content));
                entry.interrupted = *interrupted;
                self.entries.push(entry);
                if !tool_calls.is_empty() {
                    let calls = tool_calls
                        .iter()
                        .map(|call| format!("{}({})", call.name, call.arguments))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.entries.push(Entry::new(EntryKind::ToolCall, calls));
                }
            }
            Message::Tool { result, .. } => {
                let mut entry = Entry::new(EntryKind::ToolResult, result.text());
                entry.is_error = result.is_error;
                self.entries.push(entry);
            }
        }
    }
}

/// Concatenated text parts, for display.
pub fn text_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

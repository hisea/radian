//! The UI state: transcript entries, the input buffer, scroll-follow logic, and key
//! handling. It contains no terminal I/O, so the whole thing is driven by tests in memory.

use agent_core::{
    AgentEvent, BudgetReport, ContentPart, Message, ModelUsage, RunStatus, UsageSource,
};
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
    /// How many lines this entry wrapped to, for the width and text it was last measured at.
    /// The renderer keeps it so a frame only re-wraps entries that changed or are on screen.
    pub(crate) wrap_cache: WrapCache,
}

impl Entry {
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            live: false,
            interrupted: false,
            is_error: false,
            wrap_cache: WrapCache::default(),
        }
    }
}

/// A measured line count, keyed by what it depends on: the width, the text length (entry text
/// only ever grows while live, and a committed entry is a new `Entry`), and the interrupted
/// marker. A cache, not state: two entries that differ only here are equal.
#[derive(Debug, Clone, Default)]
pub(crate) struct WrapCache(Option<WrapKey>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WrapKey {
    pub width: usize,
    pub text_len: usize,
    pub interrupted: bool,
    pub lines: usize,
}

impl WrapCache {
    pub(crate) fn get(&self, width: usize, text_len: usize, interrupted: bool) -> Option<usize> {
        self.0
            .filter(|k| k.width == width && k.text_len == text_len && k.interrupted == interrupted)
            .map(|k| k.lines)
    }

    pub(crate) fn set(&mut self, key: WrapKey) {
        self.0 = Some(key);
    }
}

impl PartialEq for WrapCache {
    fn eq(&self, _: &Self) -> bool {
        true
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
    let usage = match report.source {
        UsageSource::Reported => format!("{} (reported)", report.projected_tokens),
        UsageSource::ReportedPlusEstimate => format!(
            "{} (reported {} + estimated {})",
            report.projected_tokens,
            report.baseline_input_tokens.unwrap_or(0),
            report.estimated_growth_tokens.unwrap_or(0)
        ),
        UsageSource::Estimated => format!("~{} (estimated)", report.projected_tokens),
    };
    format!("context {usage} / {}", report.input_budget_tokens)
}

/// The status bar's budget segment (ADR 0018): the projection `T` over the input budget `B`.
/// No window configured says it is off and never invents a number; a configured budget with no
/// request yet names `B`.
pub fn budget_status_text(input_budget: Option<u64>, report: Option<&BudgetReport>) -> String {
    match (input_budget, report) {
        (None, _) => "context budget off (no window configured)".to_string(),
        (Some(_), Some(report)) => budget_text(report),
        (Some(input_budget), None) => format!("context budget B {input_budget} (no request yet)"),
    }
}

/// The status bar's usage segment: the provider-reported input/output totals for this
/// session. Only reported numbers appear; a field the provider never reported is `-`, and a
/// session with no reports at all says `none` rather than inventing a zero.
pub fn usage_text(input: Option<u64>, output: Option<u64>) -> String {
    match (input, output) {
        (None, None) => "usage none".to_string(),
        (input, output) => format!(
            "usage {}/{}",
            input.map_or_else(|| "-".to_string(), |n| n.to_string()),
            output.map_or_else(|| "-".to_string(), |n| n.to_string())
        ),
    }
}

/// The status bar's MCP segment: `off` with no servers configured, `servers/…` once servers
/// are configured but the session has not opened, and the servers/tools the opened session
/// actually reported. `…` is the honest "not known yet"; it is never a guessed count.
pub fn mcp_text(configured: usize, open: Option<(usize, usize)>) -> String {
    match (configured, open) {
        (0, _) => "mcp off".to_string(),
        (configured, None) => format!("mcp {configured}/…"),
        (_, Some((servers, tools))) => format!("mcp {servers}/{tools}"),
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
    Quit,
}

pub struct App {
    pub model_name: String,
    pub session_id: String,
    pub entries: Vec<Entry>,
    pub input: String,
    /// The first visible transcript line. `usize`: a long session passes 65,535 wrapped lines
    /// (dogfooding D9), and a `u16` silently wrapped there.
    pub scroll: usize,
    /// True while the view is pinned to the newest line. Scrolling up clears it; scrolling
    /// back to the bottom, or new content while it is set, pins it again.
    pub follow: bool,
    pub turn: TurnState,
    pub quit: bool,
    pub note: Option<String>,
    /// Set while a tool call waits for the human; keys answer it instead of typing.
    pub pending_approval: Option<ApprovalPrompt>,
    /// The effective input budget `B`, if any; `None` means no budget is configured.
    pub context_window: Option<u64>,
    /// The most recent budget report, for the status bar.
    pub budget_report: Option<BudgetReport>,
    /// The permission tier's display name (ADR 0012), set by the host at startup.
    pub tier: Option<String>,
    /// Provider-reported usage totals for this session. `None` means the provider never
    /// reported that field; it is never filled with an estimate or a zero.
    pub usage_input: Option<u64>,
    pub usage_output: Option<u64>,
    /// How many MCP servers are configured, and the servers/tools the opened session
    /// reported (`None` until it opens). Plain numbers, so the app needs no MCP dependency.
    pub mcp_configured: usize,
    pub mcp_open: Option<(usize, usize)>,
    pub total_lines: usize,
    pub view_height: u16,
    pub max_scroll: usize,
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
            tier: None,
            usage_input: None,
            usage_output: None,
            mcp_configured: 0,
            mcp_open: None,
            total_lines: 0,
            view_height: 0,
            max_scroll: 0,
        }
    }

    /// Records the effective input budget `B` (no budget is configured when this is `None`).
    pub fn set_context_window(&mut self, input_budget: Option<u64>) {
        self.context_window = input_budget;
    }

    /// Records the permission tier's display name, for the status bar.
    pub fn set_tier(&mut self, tier: impl Into<String>) {
        self.tier = Some(tier.into());
    }

    /// Records the MCP state: how many servers are configured, and what the opened session
    /// reported (if it has opened).
    pub fn set_mcp(&mut self, configured: usize, open: Option<(usize, usize)>) {
        self.mcp_configured = configured;
        self.mcp_open = open;
    }

    /// Records the transcript viewport. Called by the renderer with the measured transcript
    /// size, so scroll math works on wrapped lines without the app knowing how to wrap.
    pub fn set_viewport(&mut self, view_height: u16, total_lines: usize) {
        self.view_height = view_height;
        self.total_lines = total_lines;
        self.max_scroll = total_lines.saturating_sub(usize::from(view_height));
        if self.follow {
            self.scroll = self.max_scroll;
        } else {
            self.scroll = self.scroll.min(self.max_scroll);
        }
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    fn set_scroll(&mut self, next: usize) {
        self.scroll = next.min(self.max_scroll);
        self.follow = self.scroll >= self.max_scroll;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let step = delta.unsigned_abs() as usize;
        let next = if delta < 0 {
            self.scroll.saturating_sub(step)
        } else {
            self.scroll.saturating_add(step)
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
            AgentEvent::TurnStarted => self.turn = TurnState::Running,
            // ADR 0018: the request carries its projection and budget `B`, for the status bar.
            AgentEvent::ModelRequestStarted { budget, .. } => {
                self.turn = TurnState::Running;
                self.budget_report = budget.clone();
            }
            AgentEvent::ModelTextDelta { text } => self.append_live(EntryKind::Assistant, text),
            AgentEvent::ModelReasoningDelta { text } => {
                self.append_live(EntryKind::Reasoning, text)
            }
            // Usage is the provider's own report; each present field is added to the session
            // total, and a field that was never reported stays `None` (never a zero). A
            // response without usage is not an event here at all.
            AgentEvent::ModelResponseReceived {
                usage: Some(usage), ..
            } => self.add_usage(usage),
            AgentEvent::ToolStarted { name, .. } => {
                self.note = Some(format!("tool {name} running"));
            }
            AgentEvent::TranscriptAppended { message, .. } => self.push_message(message),
            // ADR 0018: compaction is visible to the user as it happens.
            AgentEvent::CompactionStarted { tokens_before, .. } => {
                self.note = Some("compacting context".to_string());
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("context over the budget ({tokens_before} tokens); compacting…"),
                ));
            }
            AgentEvent::CompactionFinished {
                tokens_before,
                tokens_after,
                ..
            } => {
                self.note = None;
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("context compacted ({tokens_before} → {tokens_after} tokens)"),
                ));
            }
            AgentEvent::CompactionFailed { reason } => {
                self.note = None;
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("compaction failed: {reason}"),
                ));
            }
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
            }
            _ => {}
        }
    }

    /// Adds one provider-reported usage to the session totals. A field the provider did not
    /// report is left alone; it is never treated as zero.
    fn add_usage(&mut self, usage: &ModelUsage) {
        if let Some(input) = usage.input {
            self.usage_input = Some(self.usage_input.unwrap_or(0).saturating_add(input));
        }
        if let Some(output) = usage.output {
            self.usage_output = Some(self.usage_output.unwrap_or(0).saturating_add(output));
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

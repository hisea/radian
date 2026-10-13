//! The UI state: transcript entries, the input buffer, scroll-follow logic, and key
//! handling. It contains no terminal I/O, so the whole thing is driven by tests in memory.

use std::path::PathBuf;

use agent_core::{
    AgentEvent, BudgetReport, ContentPart, Message, ModelUsage, RunStatus, UsageSource,
};
use radian_host::memory::MemoryService;
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::{self, PendingCommand};
use crate::composer::Composer;
use crate::find::FindState;
use crate::keys::{Action, Keymap};
use crate::theme::{Theme, ThemeName};

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
    /// The input composer: text plus a cursor (ADR 0009 Phase 4).
    pub input: Composer,
    /// This session's submitted inputs, oldest first (sent messages and `/…` commands). Not
    /// persisted; a resume rebuilds it from the journal's user messages.
    pub history: Vec<String>,
    /// `Some(i)` while browsing `history[i]`; `None` while editing the current line.
    pub history_cursor: Option<usize>,
    /// The line that was being edited when browsing started, restored by `Down` past the end.
    pub history_stash: String,
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
    /// Set while a `/memory` write waits for the user's `y`; keys answer it instead of typing.
    pub pending_command: Option<PendingCommand>,
    /// Set by `/compact`; the event loop runs it on the runtime and shows the outcome. `Some`
    /// is a request, the inner value the optional instructions.
    pub pending_compact: Option<Option<String>>,
    /// Set by `/sessions` or `/export`; the event loop runs it with the session directory and
    /// the host's secrets and shows the result as notices.
    pub pending_session: Option<command::SessionOutcome>,
    /// The host's memory service, or `None` when this process has no memory root. `/memory`
    /// runs against the same service the agent's own tools use.
    pub memory: Option<MemoryService>,
    /// The color theme (config `tui.theme`, `NO_COLOR`).
    pub theme: Theme,
    /// The key bindings (config `tui.keybindings`), defaulting to the pre-config key table.
    pub keymap: Keymap,
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
    /// The workspace root, so `Tab` can complete `@` file references. `None` until the host
    /// sets it; without it `Tab` does nothing.
    pub workspace: Option<PathBuf>,
    /// The candidates from the last `Tab`, shown above the input box. `None` when no list is
    /// up; any key other than `Tab` dismisses it.
    pub completion: Option<Vec<String>>,
    /// The open in-transcript search box (Phase 5), or `None`. While it is open the input box
    /// is shown as `find: <query>` and typed keys edit the query instead of the message.
    pub find: Option<FindState>,
    /// The transcript width the renderer last used, so a find recomputes on a resize.
    pub transcript_width: usize,
}

impl App {
    pub fn new(model_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            session_id: session_id.into(),
            entries: Vec::new(),
            input: Composer::new(),
            history: Vec::new(),
            history_cursor: None,
            history_stash: String::new(),
            scroll: 0,
            follow: true,
            turn: TurnState::Idle,
            quit: false,
            note: None,
            pending_approval: None,
            pending_command: None,
            pending_compact: None,
            pending_session: None,
            memory: None,
            theme: Theme::default(),
            keymap: Keymap::default(),
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
            workspace: None,
            completion: None,
            find: None,
            transcript_width: 80,
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

    /// The host's memory service, for `/memory`. Set once at startup.
    pub fn set_memory(&mut self, memory: Option<MemoryService>) {
        self.memory = memory;
    }

    /// Applies the wired `tui.*` configuration. A bad keymap (a key bound to two actions) is
    /// an error the host turns into a startup refusal; unknown actions/keys are warnings.
    pub fn apply_config(
        &mut self,
        config: &radian_host::config::Config,
        no_color: bool,
    ) -> Result<Vec<String>, String> {
        let name = config.tui.theme.as_deref().and_then(ThemeName::parse);
        self.theme = Theme::resolve(name, no_color);
        let keymap = Keymap::from_config(&config.tui.keybindings).map_err(|e| e.to_string())?;
        let mut warnings = keymap.warnings.clone();
        self.keymap = keymap;
        warnings.extend(
            config
                .skipped
                .iter()
                .map(|key| format!("ignored key `{key}`")),
        );
        Ok(warnings)
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

    // ---------- in-transcript search (Phase 5) ----------

    /// Records the transcript width the renderer used. A width change re-wraps the transcript,
    /// so an open find box recomputes its matches — once per resize, never per frame.
    pub fn set_transcript_width(&mut self, width: usize) {
        if self.transcript_width == width {
            return;
        }
        self.transcript_width = width;
        if self.find.is_some() {
            self.find_recompute();
        }
    }

    /// The open find box, if any.
    pub fn find_state(&self) -> Option<&FindState> {
        self.find.as_ref()
    }

    /// Opens the find box anchored at the current viewport top (`Ctrl-F`). Refused while an
    /// approval or `/memory` confirmation prompt is up.
    pub fn find_open(&mut self) {
        if !self.input_editable() || self.find.is_some() {
            return;
        }
        self.find = Some(FindState::open(self.scroll));
    }

    /// Closes the find box; the viewport stays where it is (`Esc`).
    pub fn find_close(&mut self) {
        self.find = None;
    }

    /// The earlier match (`Enter`, or `Ctrl-F` again).
    pub fn find_prev(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.prev();
        }
        self.find_jump();
    }

    /// The later match (`Shift-Enter`, `Ctrl-G`).
    pub fn find_next(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.next();
        }
        self.find_jump();
    }

    fn find_recompute(&mut self) {
        let width = self.transcript_width;
        if let Some(find) = self.find.as_mut() {
            find.recompute(&self.entries, width);
        }
        self.find_jump();
    }

    fn find_push(&mut self, c: char) {
        if let Some(find) = self.find.as_mut() {
            find.query.push(c);
        }
        self.find_recompute();
    }

    fn find_backspace(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.query.pop();
        }
        self.find_recompute();
    }

    /// Centers the current match in the viewport and stops following the bottom. A search that
    /// matches nothing leaves the viewport alone.
    fn find_jump(&mut self) {
        let Some(line) = self.find.as_ref().and_then(FindState::current_line) else {
            return;
        };
        let height = usize::from(self.view_height.max(1));
        self.scroll = crate::find::centered_scroll(line, height, self.max_scroll);
        self.follow = false;
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
    ///
    /// The keys are looked up in the [`Keymap`] (config `tui.keybindings`); ordinary
    /// printable characters are inserted at the cursor and are never bindings, so typing can
    /// not be broken by a bad keymap. The approval and command-confirmation prompts are
    /// handled first and are deliberately not rebindable.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Send> {
        // While a prompt is up, its own keys answer it and never type into the input box.
        // `y`/`a`/`n` and `Esc` are hardcoded **before** the keymap, so a `tui.keybindings`
        // entry can never take them over (rebinding them could approve a call the user meant
        // to deny). Cancel (Ctrl-C) still works; every other key is ignored.
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.answer_approval(ApprovalOutcome::AllowOnce);
                    return None;
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.answer_approval(ApprovalOutcome::AllowForSession);
                    return None;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.answer_approval(ApprovalOutcome::Deny { reason: None });
                    return None;
                }
                _ => {}
            }
            if self.keymap.action_for(&key) == Some(Action::Cancel) {
                return self.cancel_or_quit();
            }
            return None;
        }
        // A `/memory` write waiting for `y`: same hardcoded keys, same cancel exception.
        if self.pending_command.is_some()
            && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
        {
            let pending = self.pending_command.take().expect("checked just above");
            self.run_confirmed(pending);
            return None;
        }
        if self.pending_command.is_some()
            && matches!(
                key.code,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc
            )
        {
            self.pending_command = None;
            self.push_notice("cancelled");
            return None;
        }
        if self.pending_command.is_some() {
            if self.keymap.action_for(&key) == Some(Action::Cancel) {
                return self.cancel_or_quit();
            }
            return None;
        }
        // Cancel/quit works even while a prompt is up: the permission layer races the
        // approval against cancellation, so the waiting call resolves as a denial.
        let action = self.keymap.action_for(&key);
        if action == Some(Action::Cancel) {
            return self.cancel_or_quit();
        }
        // The `@` completion list (Phase 4): the escape action closes it (and does not clear
        // the input); any key other than the completion action dismisses it, because typing
        // continues the word and the list would be stale.
        if action == Some(Action::Escape) && self.completion.is_some() {
            self.completion = None;
            return None;
        }
        if action != Some(Action::Complete) {
            self.completion = None;
        }
        // The find box (Phase 5) is a layer over the transcript: while it is open its keys are
        // looked up first, so `enter` is the earlier match there and the send key in the
        // transcript. `ctrl-f` is both "open the box" and "earlier match"; `esc` closes it.
        if self.find.is_some() {
            if let Some(action) = self.keymap.find_action_for(&key) {
                return self.dispatch(action);
            }
            match key.code {
                KeyCode::Backspace => {
                    self.find_backspace();
                    return None;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.find_push(c);
                    return None;
                }
                // PageUp/PageDown, Home/End and the arrows keep reaching the transcript
                // layer, so the box does not trap navigation.
                _ => {}
            }
        }
        if let Some(action) = action {
            return self.dispatch(action);
        }
        // Any other plain character is text, never a binding.
        if let KeyCode::Char(c) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.input.insert_char(c);
        }
        None
    }

    /// Cancels a running turn, or quits when idle.
    fn cancel_or_quit(&mut self) -> Option<Send> {
        if self.turn == TurnState::Running {
            Some(Send::Cancel)
        } else {
            self.quit = true;
            Some(Send::Quit)
        }
    }

    /// Runs one keymap action. The default bindings reproduce the pre-keymap key table.
    fn dispatch(&mut self, action: Action) -> Option<Send> {
        match action {
            Action::Send => {
                let text = std::mem::take(&mut self.input).text();
                self.history_cursor = None;
                self.history_stash.clear();
                if text.trim().is_empty() {
                    return None;
                }
                // Every submitted line (messages and `//` escapes and `/…` commands) joins the
                // session history; browsing edits never change it.
                self.history.push(text.clone());
                let trimmed = text.trim();
                // `//x` is the escape: send it as the ordinary message `/x`.
                if let Some(escaped) = trimmed.strip_prefix("//") {
                    let message = format!("/{escaped}");
                    return if self.turn == TurnState::Running {
                        Some(Send::Steer(message))
                    } else {
                        Some(Send::Followup(message))
                    };
                }
                // A `/…` line is a command: it never reaches the model or the journal.
                if trimmed.starts_with('/') {
                    self.run_slash_command(trimmed);
                    return None;
                }
                if self.turn == TurnState::Running {
                    Some(Send::Steer(text))
                } else {
                    Some(Send::Followup(text))
                }
            }
            Action::Newline => {
                self.input.insert_char('\n');
                None
            }
            Action::CursorLeft => {
                self.input.left();
                None
            }
            Action::CursorRight => {
                self.input.right();
                None
            }
            Action::WordLeft => {
                self.input.word_left();
                None
            }
            Action::WordRight => {
                self.input.word_right();
                None
            }
            Action::LineStart => {
                self.input.line_start();
                None
            }
            Action::LineEnd => {
                self.input.line_end();
                None
            }
            Action::HistoryPrev => {
                if self.input.is_first_line() {
                    self.history_up();
                } else {
                    self.input.up();
                }
                None
            }
            Action::HistoryNext => {
                if self.input.is_last_line() {
                    self.history_down();
                } else {
                    self.input.down();
                }
                None
            }
            Action::DeleteBefore => {
                self.input.backspace();
                None
            }
            Action::DeleteUnder => {
                self.input.delete();
                None
            }
            // Tab: complete the `@` file reference at the cursor (Phase 4). On any other
            // word it does nothing, so the key stays free for a future binding.
            Action::Complete => {
                self.complete_mention();
                None
            }
            // --- in-transcript search (Phase 5) ---
            // `find` opens the box; a second `find` (Ctrl-F by default) is the earlier match,
            // which is exactly the historical behavior.
            Action::Find => {
                if self.find.is_some() {
                    self.find_prev();
                } else {
                    self.find_open();
                }
                None
            }
            Action::FindPrev => {
                self.find_prev();
                None
            }
            Action::FindNext => {
                self.find_next();
                None
            }
            Action::FindClose => {
                self.find_close();
                None
            }
            Action::DeleteWord => {
                self.input.delete_word();
                None
            }
            Action::DeleteToLineStart => {
                self.input.kill_to_line_start();
                None
            }
            Action::DeleteToLineEnd => {
                self.input.kill_to_line_end();
                None
            }
            Action::ScrollPageUp => {
                self.scroll_by(-(self.view_height.max(1) as i32));
                None
            }
            Action::ScrollPageDown => {
                self.scroll_by(self.view_height.max(1) as i32);
                None
            }
            Action::ScrollLineUp => {
                self.scroll_by(-1);
                None
            }
            Action::ScrollLineDown => {
                self.scroll_by(1);
                None
            }
            Action::ScrollTop => {
                self.set_scroll(0);
                None
            }
            Action::ScrollBottom => {
                self.scroll_to_bottom();
                None
            }
            Action::Escape => {
                // A non-empty input is cleared first (kept in the history, so `Up` finds it);
                // an empty one cancels a running turn or quits.
                if !self.input.is_empty() {
                    let text = std::mem::take(&mut self.input).text();
                    self.history.push(text);
                    self.history_cursor = None;
                    self.history_stash.clear();
                    None
                } else {
                    self.cancel_or_quit()
                }
            }
            Action::Cancel => self.cancel_or_quit(),
        }
    }

    /// True while the input box is editable: no approval or command prompt is up, so the
    /// cursor belongs in the input box and keys type into it.
    pub fn input_editable(&self) -> bool {
        self.pending_approval.is_none() && self.pending_command.is_none()
    }

    /// `Up` on the first input line: the previous history entry. Starts browsing by stashing
    /// the current line, so `Down` past the newest entry restores it. False with no history.
    pub fn history_up(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next = match self.history_cursor {
            None => {
                self.history_stash = self.input.text();
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        self.input.set_text(&self.history[next]);
        true
    }

    /// `Down` on the last input line: the next history entry, or the stashed draft at the end.
    pub fn history_down(&mut self) -> bool {
        let Some(i) = self.history_cursor else {
            return false;
        };
        if i + 1 < self.history.len() {
            self.history_cursor = Some(i + 1);
            self.input.set_text(&self.history[i + 1]);
        } else {
            self.history_cursor = None;
            let stash = std::mem::take(&mut self.history_stash);
            self.input.set_text(&stash);
        }
        true
    }

    /// Prefills the history from a resumed session's user messages (ADR 0006 §5.5: the
    /// source rides on the message). Only what the user sent, in order.
    pub fn prefill_history(&mut self, messages: &[Message]) {
        for message in messages {
            if let Message::User { content, meta } = message
                && matches!(meta.source, None | Some(agent_core::InputSource::User))
            {
                self.history.push(text_of(content));
            }
        }
    }

    /// Runs one `/…` line: the notices become transcript entries, and a command that writes
    /// arms `pending_command` instead of running. The line is not sent anywhere.
    fn run_slash_command(&mut self, line: &str) {
        match command::dispatch(line, self.memory.as_ref()) {
            command::Outcome::Notices(notices) => {
                for notice in notices {
                    self.entries.push(Entry::new(EntryKind::Notice, notice));
                }
            }
            command::Outcome::Confirm(pending) => {
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("{} [y/N]", pending.prompt),
                ));
                self.pending_command = Some(pending);
            }
            // `/compact` is asynchronous: the event loop runs it on the runtime and pushes the
            // result as a notice when it finishes.
            command::Outcome::Compact(instructions) => {
                self.pending_compact = Some(instructions);
            }
            command::Outcome::Sessions(query) => {
                self.pending_session = Some(command::SessionOutcome::List(query));
            }
            command::Outcome::Export(request) => {
                self.pending_session = Some(command::SessionOutcome::Export(request));
            }
            command::Outcome::Fork => {
                self.pending_session = Some(command::SessionOutcome::Fork);
            }
            // `dispatch` only returns `Message` for an empty token list, already rejected.
            command::Outcome::Message => {}
        }
    }

    /// Runs a confirmed `/memory` command and shows its output.
    fn run_confirmed(&mut self, pending: PendingCommand) {
        let json = pending.command.json();
        let Some(service) = self.memory.as_ref() else {
            self.push_notice("memory is disabled (no RADIAN_HOME or HOME)");
            return;
        };
        match radian_host::memory::commands::execute(service, pending.command, true, json) {
            Ok(output) => {
                for notice in command::output_notices(&output) {
                    self.entries.push(Entry::new(EntryKind::Notice, notice));
                }
            }
            Err(error) => self.push_notice(error.message),
        }
    }

    /// Pushes a notice entry. Public so the event loop can show a `/compact` result.
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::new(EntryKind::Notice, text));
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

/// The `@`-mention slice needs exactly one `App` method. Kept in its own block at the end of
/// the file so it merges cleanly with the composer work that owns the rest of the input
/// handling.
impl App {
    /// Puts `text` back into the input box. An expansion failure must not lose the message
    /// the user typed.
    pub fn restore_input(&mut self, text: String) {
        self.input.set_text(&text);
    }

    /// Records the workspace root, so `Tab` can complete `@` file references against it.
    pub fn set_workspace(&mut self, workspace: impl Into<PathBuf>) {
        self.workspace = Some(workspace.into());
    }

    /// `Tab` on an `@` word (Phase 4):
    ///
    /// * one candidate: insert it; a directory keeps its trailing `/`, a file gets a trailing
    ///   space so the next word can be typed;
    /// * several: extend the word to their longest common prefix; a second `Tab` already at
    ///   that prefix shows the candidate list (the renderer draws `self.completion`);
    /// * none: leave the text alone;
    /// * the cursor is not on an `@` word: do nothing.
    ///
    /// Only the text between `@` and the cursor is replaced; text after the cursor is kept.
    fn complete_mention(&mut self) {
        let Some(workspace) = self.workspace.clone() else {
            self.completion = None;
            return;
        };
        let Some(completion) =
            crate::mention::complete(&self.input.text(), self.input.cursor(), &workspace)
        else {
            self.completion = None;
            return;
        };
        if completion.candidates.is_empty() {
            self.completion = None;
            return;
        }
        if completion.candidates.len() == 1 {
            let candidate = &completion.candidates[0];
            let insert = if candidate.ends_with('/') {
                candidate.clone()
            } else {
                format!("{candidate} ")
            };
            self.replace_range(completion.range, &insert);
            self.completion = None;
            return;
        }
        let chars: Vec<char> = self.input.text().chars().collect();
        let typed: String = chars[completion.range.clone()].iter().collect();
        if completion.common_prefix.chars().count() > typed.chars().count() {
            self.replace_range(completion.range, &completion.common_prefix);
            self.completion = None;
            return;
        }
        self.completion = Some(completion.candidates);
    }

    /// Replaces the char range `range` with `replacement`, leaving the cursor right after the
    /// replacement and the rest of the text unchanged.
    fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let chars: Vec<char> = self.input.text().chars().collect();
        let mut text: Vec<char> = Vec::with_capacity(chars.len() + replacement.len());
        text.extend_from_slice(&chars[..range.start]);
        text.extend(replacement.chars());
        let after = chars.len() - range.end;
        text.extend_from_slice(&chars[range.end..]);
        self.input.set_text(&text.into_iter().collect::<String>());
        for _ in 0..after {
            self.input.left();
        }
    }
}

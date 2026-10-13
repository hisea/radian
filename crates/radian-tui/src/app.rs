//! The UI state and the one function that changes it: [`App::update`] (ADR 0023 U3).
//!
//! `update` is **pure**: it never does I/O, never touches the runtime, never reads a file or a
//! directory, and never calls into a service. Everything that needs the outside world comes
//! back as an [`Effect`], which the event loop in `main.rs` runs; the result returns as another
//! [`Input`]. `tests/purity.rs` asserts the structure (no `std::fs`, no `MemoryService`, no
//! `tokio`, no `mention::expand`/`complete` call in this file).
//!
//! A key press is resolved to an [`Input`] by [`App::key_input`] (the keymap plus the current
//! layer: approval prompt, `/memory` confirmation, find box), and [`App::on_key`] is the thin
//! wrapper that resolves and updates.

use std::path::PathBuf;

use agent_core::{
    AgentEvent, BudgetReport, CompactionOutcome, ContentPart, Message, ModelUsage, RunStatus,
    UsageSource,
};
use radian_host::memory::commands::MemoryCommand;
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_session::RuntimeEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::{self, PendingCommand};
use crate::composer::Composer;
use crate::find::FindState;
use crate::keys::{Action, Keymap};
use crate::mention::{Completion, ExpandError, Expansion};
use crate::projection::StreamSnapshot;
use crate::theme::{Theme, ThemeName};

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

/// One input to [`App::update`]: an interaction intent, or a fact from the host.
///
/// [`Input::Action`] is the keymap's semantic action. [`Input::Char`] and
/// [`Input::FindBackspace`] are the two editing intents that are deliberately **not** bindings
/// (typing, and the find box's own backspace, which belongs to the box like the approval keys
/// belong to the prompt). Everything else is a host fact or the result of an [`Effect`].
#[derive(Debug)]
pub enum Input {
    /// A semantic action, from the keymap or synthesized by the host.
    Action(Action),
    /// A printable character typed into the focused text field.
    Char(char),
    /// The find box's backspace (not keymap-driven).
    FindBackspace,
    /// A bracketed paste: inserted at the cursor.
    Paste(String),
    /// The terminal was resized. The renderer re-measures on the next frame.
    Resize { rows: u16, cols: u16 },
    /// The hardcoded answer to the approval prompt on screen (`y`/`a`/`n`/`Esc`).
    ApprovalAnswer(ApprovalOutcome),
    /// The hardcoded answer to a `/memory` confirmation (`true` = confirmed).
    ConfirmCommand(bool),
    /// The streaming text the projection has accumulated (ADR 0023 U2). Deltas never come
    /// through the channel: the loop reads the projection when a `Dirty` notification says so.
    Stream(StreamSnapshot),
    /// A live agent event.
    Event(AgentEvent),
    /// A runtime event (ADR 0017's turn failure).
    Runtime(RuntimeEvent),
    /// An approval request arrived from the permission layer.
    Approval(ApprovalRequest),
    /// An `@` expansion finished (the result of [`Effect::ExpandMentions`]).
    MentionsExpanded(Result<Expansion, ExpandError>),
    /// An `@` completion finished (`None` = nothing to complete).
    CompletionReady(Option<Completion>),
    /// A `/memory` command finished (the result of [`Effect::RunMemoryCommand`]).
    MemoryCommandDone(MemoryResult),
    /// A manual `/compact` finished (the result of [`Effect::Compact`]).
    CompactDone(Result<CompactionOutcome, String>),
    /// A `/sessions`, `/export` or `/fork` finished: the notices to show.
    SessionDone(Vec<String>),
}

/// The result of [`Effect::RunMemoryCommand`]. The executor decides nothing: a command that
/// writes comes back as [`MemoryResult::NeedsConfirmation`] and `update` arms the prompt.
#[derive(Debug)]
pub enum MemoryResult {
    /// The command ran; these lines are the notices to show.
    Output(Vec<String>),
    /// The command writes and needs the user's `y` first.
    NeedsConfirmation {
        command: MemoryCommand,
        prompt: String,
    },
    /// The command failed; this is the message to show.
    Failed(String),
}

/// An external action. `update` returns these; the event loop runs them. An effect that has a
/// result reports it back as an [`Input`] (`ExpandMentions` → `MentionsExpanded`, and so on).
#[derive(Debug, Clone)]
pub enum Effect {
    /// Start a new turn: `Inbox::followup`.
    SendFollowup(String),
    /// Redirect the running turn: `Inbox::steer`.
    Steer(String),
    /// Cancel the running turn.
    Cancel,
    /// Leave the app.
    Quit,
    /// Answer the approval prompt `request_id` is showing. The loop owns the reply channel.
    AnswerApproval {
        request_id: String,
        outcome: ApprovalOutcome,
    },
    /// Drop the reply channel of `request_id` without answering: the permission layer treats a
    /// dropped channel as an abandoned request (a denial). This is what clearing the prompt
    /// used to do, and it is why the loop must be told.
    AbandonApproval { request_id: String },
    /// Expand the `@` references in `text` (reads files).
    ExpandMentions { text: String },
    /// Complete the `@` reference at the cursor (reads a directory).
    CompleteMention { text: String, cursor: usize },
    /// Run a `/memory` command through the shared executor (`confirmed` skips the prompt).
    RunMemoryCommand {
        command: MemoryCommand,
        confirmed: bool,
    },
    /// Run a manual `/compact` on the runtime, between turns.
    Compact { instructions: Option<String> },
    /// Run a `/sessions`, `/export` or `/fork` request.
    Session(command::SessionOutcome),
}

/// Effects are compared by **meaning** in the tests. `MemoryCommand` (a clap enum) has no
/// `PartialEq`, so two memory effects are equal when their parsed form is identical.
impl PartialEq for Effect {
    fn eq(&self, other: &Self) -> bool {
        use Effect::*;
        match (self, other) {
            (SendFollowup(a), SendFollowup(b)) | (Steer(a), Steer(b)) => a == b,
            (Cancel, Cancel) | (Quit, Quit) => true,
            (
                AnswerApproval {
                    request_id: a,
                    outcome: ao,
                },
                AnswerApproval {
                    request_id: b,
                    outcome: bo,
                },
            ) => a == b && ao == bo,
            (AbandonApproval { request_id: a }, AbandonApproval { request_id: b }) => a == b,
            (ExpandMentions { text: a }, ExpandMentions { text: b }) => a == b,
            (
                CompleteMention {
                    text: a,
                    cursor: ac,
                },
                CompleteMention {
                    text: b,
                    cursor: bc,
                },
            ) => a == b && ac == bc,
            (
                RunMemoryCommand {
                    command: a,
                    confirmed: ac,
                },
                RunMemoryCommand {
                    command: b,
                    confirmed: bc,
                },
            ) => ac == bc && format!("{a:?}") == format!("{b:?}"),
            (Compact { instructions: a }, Compact { instructions: b }) => a == b,
            (Session(a), Session(b)) => a == b,
            _ => false,
        }
    }
}

/// What one [`App::update`] produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateResult {
    /// Whether the state the renderer reads may have changed. A false positive only costs a
    /// frame; the event loop's zero-redraw rule (ADR 0023 U2) is about *no input at all*.
    pub dirty: bool,
    /// The external actions the loop must run, in order.
    pub effects: Vec<Effect>,
}

impl UpdateResult {
    /// Nothing happened.
    fn none() -> Self {
        Self::default()
    }

    /// The state changed.
    fn dirty() -> Self {
        Self {
            dirty: true,
            effects: Vec::new(),
        }
    }

    /// The state changed and the loop has work to do.
    fn effect(effect: Effect) -> Self {
        Self {
            dirty: true,
            effects: vec![effect],
        }
    }

    /// The loop has work to do, but nothing the renderer reads changed.
    fn quiet(effect: Effect) -> Self {
        Self {
            dirty: false,
            effects: vec![effect],
        }
    }
}

/// Which inbox call an in-flight `@` expansion is for, so its result can become the right
/// effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSend {
    Followup,
    Steer,
}

/// The send that is waiting for its `@` expansion: `update` produced
/// [`Effect::ExpandMentions`] and remembers what to do when [`Input::MentionsExpanded`]
/// arrives.
#[derive(Debug)]
struct PendingExpand {
    kind: PendingSend,
    text: String,
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
    /// Set while a tool call waits for the human; keys answer it instead of typing. The reply
    /// channel lives in the event loop, which owns the handoff back to the permission layer.
    pub pending_approval: Option<ApprovalRequest>,
    /// Set while a `/memory` write waits for the user's `y`; keys answer it instead of typing.
    pub pending_command: Option<PendingCommand>,
    /// Whether this process has a memory root. `/memory` runs through the loop, which owns the
    /// service; without one the command is refused with the same notice as before.
    pub memory_enabled: bool,
    /// The send waiting for its `@` expansion (see [`Effect::ExpandMentions`]).
    pending_expand: Option<PendingExpand>,
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
            memory_enabled: false,
            pending_expand: None,
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

    /// Whether this process has a memory root, for `/memory`. Set once at startup; the service
    /// itself stays on the loop's side (ADR 0023 U3).
    pub fn set_memory_enabled(&mut self, enabled: bool) {
        self.memory_enabled = enabled;
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
    fn request_approval(&mut self, request: ApprovalRequest) -> UpdateResult {
        let mut effects = Vec::new();
        if let Some(previous) = self.pending_approval.replace(request) {
            effects.push(Effect::AnswerApproval {
                request_id: previous.request_id,
                outcome: ApprovalOutcome::Deny {
                    reason: Some("superseded by another approval request".into()),
                },
            });
        }
        UpdateResult {
            dirty: true,
            effects,
        }
    }

    /// Answers the pending prompt and clears it. A no-op when nothing is pending.
    fn answer_approval(&mut self, outcome: ApprovalOutcome) -> UpdateResult {
        match self.pending_approval.take() {
            Some(request) => UpdateResult::quiet(Effect::AnswerApproval {
                request_id: request.request_id,
                outcome,
            }),
            None => UpdateResult::none(),
        }
    }

    /// The single state-change entry point (ADR 0023 U3). Pure: no I/O, no runtime, no
    /// service. Anything that needs the outside world comes back as an [`Effect`].
    pub fn update(&mut self, input: Input) -> UpdateResult {
        match input {
            Input::Action(action) => {
                // The `@` completion list: any key other than `complete` dismisses it, because
                // typing continues the word and the list would be stale. `escape` only closes
                // the list and keeps the input, so it does not reach its own action here.
                if self.completion.is_some() && action != Action::Complete {
                    self.completion = None;
                    if action == Action::Escape {
                        return UpdateResult::dirty();
                    }
                }
                self.update_action(action)
            }
            Input::Char(c) => {
                self.completion = None;
                if self.find.is_some() {
                    self.find_push(c);
                } else {
                    self.input.insert_char(c);
                }
                UpdateResult::dirty()
            }
            Input::FindBackspace => {
                self.completion = None;
                self.find_backspace();
                UpdateResult::dirty()
            }
            // A bracketed paste is text for the message box, exactly as before.
            Input::Paste(text) => {
                self.input.insert_str(&text);
                UpdateResult::dirty()
            }
            Input::Resize { .. } => UpdateResult::dirty(),
            Input::ApprovalAnswer(outcome) => self.answer_approval(outcome),
            Input::ConfirmCommand(confirmed) => self.confirm_command(confirmed),
            Input::Stream(stream) => {
                // The snapshot is the whole live view of the current stream, so the entries are
                // *set*, not appended: a coalesced notification can never leave a short entry.
                if stream.order.is_empty() {
                    // A cleared stream (a committed message replaced it): nothing to show.
                    return UpdateResult::none();
                }
                for kind in &stream.order {
                    match kind {
                        crate::projection::StreamKind::Assistant => {
                            self.set_live(EntryKind::Assistant, &stream.assistant);
                        }
                        crate::projection::StreamKind::Reasoning => {
                            self.set_live(EntryKind::Reasoning, &stream.reasoning);
                        }
                    }
                }
                UpdateResult::dirty()
            }
            Input::Event(event) => UpdateResult {
                dirty: true,
                effects: self.apply_event(&event),
            },
            Input::Runtime(event) => self.apply_runtime_event(&event),
            Input::Approval(request) => self.request_approval(request),
            Input::MentionsExpanded(result) => self.mentions_expanded(result),
            Input::CompletionReady(completion) => self.completion_ready(completion),
            Input::MemoryCommandDone(result) => self.memory_command_done(result),
            Input::CompactDone(result) => self.compact_done(result),
            Input::SessionDone(notices) => {
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
        }
    }

    /// Resolves one key event to an [`Input`] through the keymap and the current layer. Pure.
    ///
    /// The layers, in order: the approval prompt's hardcoded keys, the `/memory`
    /// confirmation's hardcoded keys, cancel, the `@` completion list, the find box's own
    /// layer, the transcript layer, and finally "a plain character is text, never a binding".
    fn key_input(&self, key: &KeyEvent) -> Option<Input> {
        // While a prompt is up, its own keys answer it and never type into the input box.
        // `y`/`a`/`n` and `Esc` are hardcoded **before** the keymap, so a `tui.keybindings`
        // entry can never take them over (rebinding them could approve a call the user meant
        // to deny). Cancel (Ctrl-C) still works; every other key is ignored.
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return Some(Input::ApprovalAnswer(ApprovalOutcome::AllowOnce));
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    return Some(Input::ApprovalAnswer(ApprovalOutcome::AllowForSession));
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    return Some(Input::ApprovalAnswer(ApprovalOutcome::Deny {
                        reason: None,
                    }));
                }
                _ => {}
            }
            if self.keymap.action_for(key) == Some(Action::Cancel) {
                return Some(Input::Action(Action::Cancel));
            }
            return None;
        }
        // A `/memory` write waiting for `y`: same hardcoded keys, same cancel exception.
        if self.pending_command.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return Some(Input::ConfirmCommand(true));
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    return Some(Input::ConfirmCommand(false));
                }
                _ => {}
            }
            if self.keymap.action_for(key) == Some(Action::Cancel) {
                return Some(Input::Action(Action::Cancel));
            }
            return None;
        }
        let action = self.keymap.action_for(key);
        if action == Some(Action::Cancel) {
            return Some(Input::Action(Action::Cancel));
        }
        // The find box is a layer over the transcript: while it is open its keys are looked up
        // first, so `enter` is the earlier match there and the send key in the transcript.
        // `ctrl-f` is both "open the box" and "earlier match"; `esc` closes it.
        if self.find.is_some() {
            if let Some(action) = self.keymap.find_action_for(key) {
                return Some(Input::Action(action));
            }
            match key.code {
                KeyCode::Backspace => return Some(Input::FindBackspace),
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Some(Input::Char(c));
                }
                // PageUp/PageDown, Home/End and the arrows keep reaching the transcript
                // layer, so the box does not trap navigation.
                _ => {}
            }
        }
        if let Some(action) = action {
            return Some(Input::Action(action));
        }
        // Any other plain character is text, never a binding.
        if let KeyCode::Char(c) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return Some(Input::Char(c));
        }
        None
    }

    /// The terminal's entry point: resolve the key, then [`App::update`]. A thin wrapper —
    /// everything that changes state lives in `update`.
    pub fn on_key(&mut self, key: KeyEvent) -> UpdateResult {
        match self.key_input(&key) {
            Some(input) => self.update(input),
            None => UpdateResult::none(),
        }
    }

    /// Cancels a running turn, or quits when idle.
    fn cancel_or_quit(&mut self) -> UpdateResult {
        if self.turn == TurnState::Running {
            UpdateResult::quiet(Effect::Cancel)
        } else {
            self.quit = true;
            UpdateResult::quiet(Effect::Quit)
        }
    }

    /// Runs one keymap action. The default bindings reproduce the pre-keymap key table.
    fn update_action(&mut self, action: Action) -> UpdateResult {
        match action {
            Action::Send => {
                let text = std::mem::take(&mut self.input).text();
                self.history_cursor = None;
                self.history_stash.clear();
                if text.trim().is_empty() {
                    return UpdateResult::dirty();
                }
                // Every submitted line (messages and `//` escapes and `/…` commands) joins the
                // session history; browsing edits never change it.
                self.history.push(text.clone());
                let trimmed = text.trim();
                // `//x` is the escape: send it as the ordinary message `/x`.
                if let Some(escaped) = trimmed.strip_prefix("//") {
                    return self.begin_send(format!("/{escaped}"));
                }
                // A `/…` line is a command: it never reaches the model or the journal.
                if trimmed.starts_with('/') {
                    return self.run_slash_command(trimmed);
                }
                self.begin_send(text)
            }
            Action::Newline => {
                self.input.insert_char('\n');
                UpdateResult::dirty()
            }
            Action::CursorLeft => {
                self.input.left();
                UpdateResult::dirty()
            }
            Action::CursorRight => {
                self.input.right();
                UpdateResult::dirty()
            }
            Action::WordLeft => {
                self.input.word_left();
                UpdateResult::dirty()
            }
            Action::WordRight => {
                self.input.word_right();
                UpdateResult::dirty()
            }
            Action::LineStart => {
                self.input.line_start();
                UpdateResult::dirty()
            }
            Action::LineEnd => {
                self.input.line_end();
                UpdateResult::dirty()
            }
            Action::HistoryPrev => {
                if self.input.is_first_line() {
                    self.history_up();
                } else {
                    self.input.up();
                }
                UpdateResult::dirty()
            }
            Action::HistoryNext => {
                if self.input.is_last_line() {
                    self.history_down();
                } else {
                    self.input.down();
                }
                UpdateResult::dirty()
            }
            Action::DeleteBefore => {
                self.input.backspace();
                UpdateResult::dirty()
            }
            Action::DeleteUnder => {
                self.input.delete();
                UpdateResult::dirty()
            }
            // Tab: complete the `@` file reference at the cursor (Phase 4). On any other word
            // it does nothing, so the key stays free for a future binding. Reading the
            // directory is the loop's job: this only asks for it.
            Action::Complete => {
                if self.workspace.is_none() {
                    self.completion = None;
                    return UpdateResult::dirty();
                }
                UpdateResult::quiet(Effect::CompleteMention {
                    text: self.input.text(),
                    cursor: self.input.cursor(),
                })
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
                UpdateResult::dirty()
            }
            Action::FindPrev => {
                self.find_prev();
                UpdateResult::dirty()
            }
            Action::FindNext => {
                self.find_next();
                UpdateResult::dirty()
            }
            Action::FindClose => {
                self.find_close();
                UpdateResult::dirty()
            }
            Action::DeleteWord => {
                self.input.delete_word();
                UpdateResult::dirty()
            }
            Action::DeleteToLineStart => {
                self.input.kill_to_line_start();
                UpdateResult::dirty()
            }
            Action::DeleteToLineEnd => {
                self.input.kill_to_line_end();
                UpdateResult::dirty()
            }
            Action::ScrollPageUp => {
                self.scroll_by(-(self.view_height.max(1) as i32));
                UpdateResult::dirty()
            }
            Action::ScrollPageDown => {
                self.scroll_by(self.view_height.max(1) as i32);
                UpdateResult::dirty()
            }
            Action::ScrollLineUp => {
                self.scroll_by(-1);
                UpdateResult::dirty()
            }
            Action::ScrollLineDown => {
                self.scroll_by(1);
                UpdateResult::dirty()
            }
            Action::ScrollTop => {
                self.set_scroll(0);
                UpdateResult::dirty()
            }
            Action::ScrollBottom => {
                self.scroll_to_bottom();
                UpdateResult::dirty()
            }
            Action::Escape => {
                // A non-empty input is cleared first (kept in the history, so `Up` finds it);
                // an empty one cancels a running turn or quits.
                if !self.input.is_empty() {
                    let text = std::mem::take(&mut self.input).text();
                    self.history.push(text);
                    self.history_cursor = None;
                    self.history_stash.clear();
                    UpdateResult::dirty()
                } else {
                    self.cancel_or_quit()
                }
            }
            Action::Cancel => self.cancel_or_quit(),
        }
    }

    /// Starts a send. The `@` references are expanded by the loop first (an effect), and the
    /// result becomes the follow-up or steer effect. Whether it steers is decided here, because
    /// the turn state is app state.
    fn begin_send(&mut self, text: String) -> UpdateResult {
        let kind = if self.turn == TurnState::Running {
            PendingSend::Steer
        } else {
            PendingSend::Followup
        };
        self.pending_expand = Some(PendingExpand {
            kind,
            text: text.clone(),
        });
        UpdateResult::effect(Effect::ExpandMentions { text })
    }

    /// The hardcoded answer to a `/memory` confirmation: `y` runs the command (through the
    /// loop), `n`/`Esc` cancels it.
    fn confirm_command(&mut self, confirmed: bool) -> UpdateResult {
        let Some(pending) = self.pending_command.take() else {
            return UpdateResult::none();
        };
        if !confirmed {
            self.push_notice("cancelled");
            return UpdateResult::dirty();
        }
        UpdateResult::effect(Effect::RunMemoryCommand {
            command: pending.command,
            confirmed: true,
        })
    }

    /// ADR 0017: a failed turn is shown in the transcript; the session stays open.
    fn apply_runtime_event(&mut self, event: &RuntimeEvent) -> UpdateResult {
        if let RuntimeEvent::TurnFailed { message, .. } = event {
            self.entries.push(Entry::new(
                EntryKind::Notice,
                format!("turn failed: {message}"),
            ));
            // ADR 0018's fallback: a recognizable context overflow gets one extra line, since
            // automatic compaction did not keep the request inside the window. The host only
            // has the text, so the prefix is what identifies it.
            if radian_host::is_context_overflow(message) {
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    radian_host::CONTEXT_OVERFLOW_HELP.to_string(),
                ));
            }
            return UpdateResult::dirty();
        }
        UpdateResult::none()
    }

    /// The `@` expansion finished. On success the send effect is produced, with the notices the
    /// expansion reported; on failure the original text goes back into the input box, so
    /// nothing is sent and the user can fix the message.
    fn mentions_expanded(&mut self, result: Result<Expansion, ExpandError>) -> UpdateResult {
        let Some(pending) = self.pending_expand.take() else {
            return UpdateResult::none();
        };
        match result {
            Ok(expansion) => {
                for warning in &expansion.warnings {
                    self.entries
                        .push(Entry::new(EntryKind::Notice, warning.clone()));
                }
                if !expansion.attached.is_empty() {
                    self.entries.push(Entry::new(
                        EntryKind::Notice,
                        format!(
                            "attached {} file(s): {}",
                            expansion.attached.len(),
                            expansion.attached.join(", ")
                        ),
                    ));
                }
                let effect = match pending.kind {
                    PendingSend::Followup => Effect::SendFollowup(expansion.text),
                    PendingSend::Steer => Effect::Steer(expansion.text),
                };
                UpdateResult::effect(effect)
            }
            Err(error) => {
                self.entries
                    .push(Entry::new(EntryKind::Notice, format!("not sent: {error}")));
                self.restore_input(pending.text);
                UpdateResult::dirty()
            }
        }
    }

    /// The `@` completion finished: apply it, or clear the list when there is nothing.
    fn completion_ready(&mut self, completion: Option<Completion>) -> UpdateResult {
        match completion {
            Some(completion) if !completion.candidates.is_empty() => {
                self.apply_completion(completion);
            }
            _ => self.completion = None,
        }
        UpdateResult::dirty()
    }

    /// A `/memory` command finished: show its output, or arm the confirmation prompt.
    fn memory_command_done(&mut self, result: MemoryResult) -> UpdateResult {
        match result {
            MemoryResult::Output(lines) => {
                for line in lines {
                    self.push_notice(line);
                }
            }
            MemoryResult::NeedsConfirmation { command, prompt } => {
                self.entries
                    .push(Entry::new(EntryKind::Notice, format!("{prompt} [y/N]")));
                self.pending_command = Some(PendingCommand { command, prompt });
            }
            MemoryResult::Failed(message) => self.push_notice(message),
        }
        UpdateResult::dirty()
    }

    /// A manual `/compact` finished (ADR 0018).
    fn compact_done(&mut self, result: Result<CompactionOutcome, String>) -> UpdateResult {
        match result {
            Ok(CompactionOutcome::Compacted {
                tokens_before,
                tokens_after,
                ..
            }) => {
                self.push_notice(format!(
                    "compaction: {tokens_before} → {tokens_after} tokens"
                ));
            }
            Ok(CompactionOutcome::NothingToCompact) => {
                self.push_notice("compaction: nothing to compact");
            }
            Err(message) => self.push_notice(format!("compaction failed: {message}")),
        }
        UpdateResult::dirty()
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

    /// Runs one `/…` line. Parsing is pure; everything that touches the world becomes an
    /// effect, and its result comes back as another [`Input`]. The line is not sent anywhere.
    fn run_slash_command(&mut self, line: &str) -> UpdateResult {
        match command::parse(line, self.memory_enabled) {
            command::Outcome::Notices(notices) => {
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
            command::Outcome::Memory(command) => UpdateResult::effect(Effect::RunMemoryCommand {
                command,
                confirmed: false,
            }),
            // `/compact` is asynchronous: the loop runs it on the runtime between turns and the
            // result comes back as `Input::CompactDone`.
            command::Outcome::Compact(instructions) => {
                UpdateResult::effect(Effect::Compact { instructions })
            }
            command::Outcome::Sessions(query) => {
                UpdateResult::effect(Effect::Session(command::SessionOutcome::List(query)))
            }
            command::Outcome::Export(request) => {
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Export(request)))
            }
            command::Outcome::Fork => {
                // ADR 0022 §1: fork only when the session is idle. The decision is app state,
                // so it is made here rather than in the loop.
                if self.turn == TurnState::Running {
                    self.push_notice(
                        "finish the current turn before forking (`/fork` needs an idle session)",
                    );
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Fork))
            }
            command::Outcome::Message => UpdateResult::none(),
        }
    }

    /// Pushes a notice entry. Public so the event loop can show a `/compact` result.
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::new(EntryKind::Notice, text));
    }

    /// Applies a live event. Deltas build `live` entries; the committed message
    /// (`TranscriptAppended`) replaces them, so a dropped delta can never leave a duplicate.
    /// Returns the effects the event implies (clearing an approval prompt abandons its reply
    /// channel, which the loop owns).
    pub fn apply_event(&mut self, event: &AgentEvent) -> Vec<Effect> {
        match event {
            AgentEvent::TurnStarted => self.turn = TurnState::Running,
            // ADR 0018: the request carries its projection and budget `B`, for the status bar.
            AgentEvent::ModelRequestStarted { budget, .. } => {
                self.turn = TurnState::Running;
                self.budget_report = budget.clone();
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
                // already resolved the hook's `ask` as a denial. Clearing the prompt must also
                // drop the reply channel — the loop owns it, so it is told to abandon the
                // request (a dropped channel is what the permission layer sees as abandoned).
                return match self.pending_approval.take() {
                    Some(request) => vec![Effect::AbandonApproval {
                        request_id: request.request_id,
                    }],
                    None => Vec::new(),
                };
            }
            _ => {}
        }
        Vec::new()
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

    /// Sets the live entry of `kind` to `text` (creating it when it does not exist yet). The
    /// stream arrives as a snapshot, so this replaces rather than appends.
    fn set_live(&mut self, kind: EntryKind, text: &str) {
        match self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.live && entry.kind == kind)
        {
            Some(entry) => entry.text = text.to_owned(),
            None => {
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

    /// Applies one `@` completion (Phase 4):
    ///
    /// * one candidate: insert it; a directory keeps its trailing `/`, a file gets a trailing
    ///   space so the next word can be typed;
    /// * several: extend the word to their longest common prefix; a second `Tab` already at
    ///   that prefix shows the candidate list (the renderer draws `self.completion`).
    ///
    /// Reading the directory happens in the loop; this is only the state change.
    fn apply_completion(&mut self, completion: Completion) {
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

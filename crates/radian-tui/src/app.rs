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
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::{AgentEvent, BudgetReport, CompactionOutcome, ContentPart, Message, UsageSource};
use radian_coding::PromptError;
use radian_host::memory::commands::MemoryCommand;
use radian_host::{PendingRequest, PendingResponse, PendingView};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_session::RuntimeEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::blocks::{Block, ReasoningMode};
use crate::clip::ClipboardMode;
use crate::clip::MAX_CLIPBOARD_BYTES;
use crate::command::{self, PendingCommand};
use crate::composer::Composer;
use crate::find::FindState;
use crate::keys::{Action, Keymap};
use crate::login::{LoginKey, LoginOutcome, LoginWizard};
use crate::mention::{Completion, ExpandError, Expansion};
use crate::picker::{Outcome as PickerOutcome, Picker};
use crate::projection::{SessionProjection, StreamSnapshot};
use crate::question::{FormKey, FormOutcome, QuestionForm};
use crate::render::InputStyle;
use crate::select::{self, Anchor, Selection};
use crate::startup::StartupScreen;
use crate::theme::Theme;

// ADR 0023 U5: the turn state is a **session fact**, so it lives in the projection
// (`radian-host`) and `App` only reads it. Re-exported here so the existing `app::TurnState`
// path keeps working.
pub use radian_host::projection::TurnState;

/// The layout cache's key (ADR 0028 R1/R6): the width, the entry's text length, the display
/// modes and the per-entry fold override. The override is part of it, so a search hit's
/// expansion invalidates the count.
pub(crate) type LayoutKey = (usize, usize, crate::tools::DisplayModes, Option<bool>);

/// A compaction summary's display data (ADR 0028 R5). The `summary` and the verbatim user
/// words come from the message's `origin` (ADR 0018) through Core's own readers;
/// the revision and the token counts are **live only** — the journaled message does not carry
/// them, so a resume replay shows `── context compacted ──` without inventing numbers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompactionInfo {
    pub summary: String,
    pub user_messages: Vec<String>,
    pub revision: Option<u64>,
    pub tokens_before: Option<u64>,
    pub tokens_after: Option<u64>,
}

impl CompactionInfo {
    /// The text the entry shows when expanded: the model's summary, then the user's words
    /// (ADR 0018), each as its own paragraph. This **is** the entry's text, so it
    /// is what the selection copies and the search matches.
    pub fn display_text(&self) -> String {
        let mut out = self.summary.trim().to_owned();
        for message in &self.user_messages {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(message.trim());
        }
        out
    }

    /// The folded one-liner, omitting whatever is not known: `── context compacted (rev 3,
    /// 12000 → 4000) ──`, `── context compacted (rev 3) ──` or `── context compacted ──`.
    pub fn line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(revision) = self.revision {
            parts.push(format!("rev {revision}"));
        }
        if let (Some(before), Some(after)) = (self.tokens_before, self.tokens_after) {
            parts.push(format!("{before} → {after}"));
        }
        if parts.is_empty() {
            "── context compacted ──".to_string()
        } else {
            format!("── context compacted ({}) ──", parts.join(", "))
        }
    }
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
    /// The startup screen's wordmark (; ADR 0027 §标志: the primary text
    /// color). It is the first entry in a session.
    Banner,
    /// The startup information's collapsed/expanded lines. Their `label: value` first lines
    /// use the same accent label / secondary body levels as a notice (`split_label`, ADR
    /// 0028 / ADR 0029).
    Startup,
    /// A startup safety line (an unavailable sandbox, an unsandboxed MCP, an unknown
    /// window, a memory/config warning): it is **never** folded away, and it uses the
    /// warning color.
    Warning,
    /// The one-line `Tip: …` under the startup block. Its `Tip:` prefix is dim, the body
    /// muted.
    Tip,
    /// A compaction summary (ADR 0028 R5): one line when folded, the summary and the user's
    /// verbatim words when expanded.
    Compaction,
    /// A user's `!`/`!!` command and its output (ADR 0033). The renderer colors `!` (goes to
    /// the model's context) and `!!` (does not) differently; the first line is `$ cmd`.
    Bash {
        exclude_from_context: bool,
    },
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
    /// A tool call's outcome, once its result has arrived: `None` while it is running,
    /// `Some(is_error)` after. The card's background follows it (ADR 0028 R4's states).
    pub result: Option<bool>,
    /// The card's data (ADR 0028 R4): the tool name, the argument JSON, the structured result
    /// and (no producer yet) the duration. `None` for every non-tool entry, and for a tool
    /// entry whose message carried nothing to model.
    pub tool: Option<Box<crate::tools::ToolCard>>,
    /// A compaction summary's data (ADR 0028 R5). `None` for every other entry.
    pub compaction: Option<Box<CompactionInfo>>,
    /// Where this tool entry sits in a merged card (ADR 0028). Derived from the
    /// entry order, refreshed whenever the entries change; the data stays two entries.
    pub card: crate::blocks::Card,
    /// A tool result's error code (`interrupted` for a cancelled call, ADR 0001), for the
    /// card's `cancelled` meta. `None` for every other entry.
    pub error_code: Option<String>,
    /// A **per-entry** fold override (ADR 0028 R6): `Some(true)` expands this entry whatever
    /// the global fold says, which is how a search hit opens exactly one folded entry.
    /// Cleared by the next global fold toggle. Part of the layout cache key.
    pub fold_override: Option<bool>,
    /// The parsed blocks (ADR 0028 R1), keyed by text length and the reasoning mode. Only
    /// filled for assistant prose (the only kind with fence grammar today).
    pub(crate) block_cache: Option<((usize, ReasoningMode), Vec<Block>)>,
    /// How many lines this entry laid out to, for the width, text length and mode it was
    /// last measured at. The renderer keeps it so a frame only re-lays out entries that
    /// changed or are on screen.
    pub(crate) layout_cache: Option<(LayoutKey, usize)>,
    /// Rendered Mermaid diagrams (ADR 0028 R8), keyed by `(width, text_len)`: the width is in
    /// the key because `TooWide` can succeed on a wider terminal. Filled by the mutable
    /// measure pass; the immutable layout paths render fresh on a miss.
    pub(crate) diagram_cache: Option<(usize, usize, Vec<crate::blocks::DiagramOutcome>)>,
    /// The entry's laid-out Markdown paragraph lines (ADR 0028 R2), keyed by `(width,
    /// text_len)` and indexed by the paragraph block's start. The layout does not depend on
    /// the reasoning/tool folds, so a global fold toggle reuses it instead of re-parsing
    /// every Assistant entry (the R7 toggle gate).
    pub(crate) md_cache: Option<MdCache>,
}

impl Entry {
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            live: false,
            interrupted: false,
            is_error: false,
            result: None,
            tool: None,
            compaction: None,
            card: crate::blocks::Card::None,
            error_code: None,
            fold_override: None,
            block_cache: None,
            layout_cache: None,
            diagram_cache: None,
            md_cache: None,
        }
    }
}

/// The three tiers of the layer stack (ADR 0023 U4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Long-lived: the transcript and the message box. Always at the bottom of the stack.
    Surface,
    /// A temporary floating layer: the find box, the `@` completion list, the session picker.
    /// Never above a [`Modal`].
    Overlay,
    /// A prompt that needs an explicit answer before it goes away: the approval prompt, the
    /// `/memory` confirmation, and (extension point, not implemented) a structured question.
    /// Always above every [`Overlay`].
    Modal,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Self::Surface => "surface",
            Self::Overlay => "overlay",
            Self::Modal => "modal",
        }
    }
}

/// One layer of the stack. The variant *is* the tier, so an [`Overlay`] can never be a
/// [`Modal`]; [`App::push_overlay`] additionally keeps an overlay below every modal.
pub enum Layer {
    Surface,
    Overlay(Overlay),
    Modal(Modal),
}

impl Layer {
    pub fn tier(&self) -> Tier {
        match self {
            Self::Surface => Tier::Surface,
            Self::Overlay(_) => Tier::Overlay,
            Self::Modal(_) => Tier::Modal,
        }
    }
}

/// The state of one [`Tier::Overlay`] layer.
pub enum Overlay {
    /// The in-transcript search box (Phase 5).
    Find(FindState),
    /// The completion candidate list (label + description), drawn by `select_list`.
    Completion(Vec<crate::command::Candidate>),
    /// The session picker (ADR 0023 U4): `--pick` at startup, `/sessions` in a session.
    Picker(Picker),
}

/// The Markdown layout cache: `(width, text_len, paragraphs)`, where each paragraph is
/// `(block start, lines)`. A type alias keeps the `Option` readable in `Entry`.
pub(crate) type MdCache = (usize, usize, Vec<(usize, Vec<crate::blocks::BodyLine>)>);

/// One approval as the UI shows it: the queue's id (ADR 0023 U7), the session that asked and
/// the request itself. The queue id is what the answer names, not the permission layer's own
/// `request_id`.
///
/// The scroll fields belong to this modal, not to the app: a new pending request builds a fresh
/// `PendingApproval`, so the next question starts at the top of its body.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: u64,
    pub origin: String,
    pub request: ApprovalRequest,
    /// The body's first visible row. The renderer clamps it against what it measured, so a key
    /// can never park the window past the end (or before the first frame move it at all).
    pub scroll: usize,
    /// The wrapped body rows and the visible rows, as the last frame measured them. Both zero
    /// before the first frame, which clamps every scroll to zero.
    pub total_rows: usize,
    pub visible_rows: usize,
}

/// The state of one [`Tier::Modal`] layer.
pub enum Modal {
    /// A tool call waiting for `y`/`a`/`n`. The keys are hardcoded (ADR 0023 U4, invariant 2).
    /// Boxed, like the question: the two payloads are much larger than the confirmation's.
    Approval(Box<PendingApproval>),
    /// A `/memory` write waiting for `y`/`n`. Also hardcoded.
    ConfirmCommand(PendingCommand),
    /// A structured question (ADR 0024 Q7). Its keys are the rebindable `form` layer, so it is
    /// the one modal whose input goes through the keymap. Boxed: the form is much larger than
    /// the other variants, and a `Modal` sits in every `Layer`.
    Question(Box<QuestionForm>),
    /// The `/login` wizard (ADR 0026 D3). Its keys are hardcoded (a login prompt is
    /// sensitive, like the approval), and the key field is never echoed. Boxed, like the
    /// question.
    Login(Box<LoginWizard>),
    /// A `/logout` confirmation: the provider whose stored credential would be removed.
    /// `y` removes, `n`/`Esc` keeps — hardcoded, like the `/memory` confirmation.
    ConfirmLogout(String),
    /// The startup project-trust prompt (ADR 0031 T3). A small list whose keys are hardcoded
    /// like the approval's: a startup decision must not depend on the keymap. Boxed, like the
    /// other payload modals.
    Trust(Box<TrustWizard>),
}

/// The startup trust modal's state (ADR 0031 T3): the workspace and the highlighted option.
/// The default highlight is "do not trust this directory", so Enter is the
/// fail-safe answer.
pub struct TrustWizard {
    pub workspace: PathBuf,
    pub cursor: usize,
    pub choices: Vec<radian_host::trust::Choice>,
}

impl TrustWizard {
    pub fn new(workspace: PathBuf) -> Self {
        let choices: Vec<radian_host::trust::Choice> = radian_host::trust::Choice::ALL
            .into_iter()
            // "Trust parent" needs a parent to name.
            .filter(|choice| {
                *choice != radian_host::trust::Choice::TrustParent || workspace.parent().is_some()
            })
            .collect();
        let cursor = choices
            .iter()
            .position(|choice| *choice == radian_host::trust::Choice::UntrustHere)
            .unwrap_or(0);
        Self {
            workspace,
            cursor,
            choices,
        }
    }

    pub fn selected(&self) -> radian_host::trust::Choice {
        self.choices[self.cursor]
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let len = self.choices.len() as i32;
        self.cursor = (self.cursor as i32 + delta).rem_euclid(len) as usize;
    }

    pub fn label(&self, choice: radian_host::trust::Choice) -> String {
        choice
            .label(&self.workspace)
            .unwrap_or_else(|| "Trust the parent directory".to_owned())
    }
}

/// How far one approval-scroll key moves the box's body (approval-fit). Hardcoded like the
/// answers (ADR 0023 U4, invariant 2): scrolling must never depend on the keymap and must
/// never collide with an answer key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScroll {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
}

/// One mouse-wheel notch (ADR 0023 U2, plan B). The input thread maps crossterm's
/// `MouseEventKind::ScrollUp`/`ScrollDown` to this; no other mouse event reaches the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseScrollDirection {
    Up,
    Down,
}

/// How many transcript lines one wheel notch scrolls. A constant, not a magic number: the
/// wheel is a coarse gesture (a notch is not a line), and this is the one place to tune it.
pub const MOUSE_WHEEL_LINES: usize = 3;

/// How often the drag's auto-scroll ticks while the pointer is held at the transcript's top
/// or bottom edge. Only consulted while a drag is live (`recv_timeout` in the loop), so an
/// idle app still blocks and never redraws (ADR 0023 U2).
pub const AUTO_SCROLL_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// How quickly a second press counts as a double-click. Wall-clock (not frames): the loop
/// is idle between clicks, so there is nothing else to measure with.
pub const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// How long the first idle Ctrl-C arms "press again to quit" (ADR 0023 U4, owner
/// 2026-10-14). A second Ctrl-C inside the window quits; the window closing, or any other
/// key, disarms it and the hint disappears.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Whether an [`Input`] is something the **user** did — a key, a paste, a mouse action, an
/// approval answer — as opposed to a host fact or an asynchronous result. Only user input
/// disarms the Ctrl-C quit window: a terminal resize or a
/// background `/v1/models` refresh must not silently cancel the pending quit.
///
/// The match is exhaustive on purpose: a new `Input` variant forces a decision here.
/// The transcript block for a `!`/`!!` command (ADR 0033): `$ cmd` first, the output, the exit
/// status. The live-display form; the model-side text is [`bash_context_text`].
fn bash_entry(command: &str, exclude_from_context: bool, run: &radian_coding::UserBash) -> Entry {
    // The renderer's marker column carries the `$ ` (ADR 0028); the text here is
    // the command and its output.
    let mut text = command.to_owned();
    if !run.stdout.is_empty() {
        text.push('\n');
        text.push_str(run.stdout.trim_end_matches('\n'));
    }
    if !run.stderr.is_empty() {
        text.push('\n');
        text.push_str(run.stderr.trim_end_matches('\n'));
    }
    text.push('\n');
    if run.cancelled {
        text.push_str("[cancelled]");
    } else if run.timed_out {
        text.push_str("[timed out]");
    } else {
        text.push_str(&format!(
            "[exit code: {}]",
            run.exit_code
                .map_or_else(|| "none".to_owned(), |code| code.to_string())
        ));
    }
    Entry::new(
        EntryKind::Bash {
            exclude_from_context,
        },
        text,
    )
}

/// The user message a `!` command injects (ADR 0033; omp's shape): ``Ran `cmd` `` + a fenced
/// output block + the exit status. The command string is part of the context too.
fn bash_context_text(command: &str, run: &radian_coding::UserBash) -> String {
    let mut out = format!("Ran `{command}`\n```\n");
    out.push_str(&run.stdout);
    if !run.stderr.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&run.stderr);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    if run.cancelled {
        out.push_str("(command cancelled)");
    } else if run.timed_out {
        out.push_str("(command timed out)");
    } else {
        match run.exit_code {
            Some(code) => out.push_str(&format!("Command exited with code {code}")),
            None => out.push_str("Command exited without a code (killed by a signal)"),
        }
    }
    out
}

/// The command in a `Ran \`cmd\`` context message, or `None` when the text is not one.
fn bash_context_command(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("Ran `")?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// A resumed `!` context message as a Bash block: `$ cmd` (its first line) and the fenced body.
fn bash_entry_from_context(text: &str) -> Option<Entry> {
    let command = bash_context_command(text)?;
    let body = text.find('\n').map_or("", |i| &text[i + 1..]);
    Some(Entry::new(
        EntryKind::Bash {
            exclude_from_context: false,
        },
        format!("{command}\n{}", body.trim_end_matches('\n')),
    ))
}

fn is_user_input(input: &Input) -> bool {
    match input {
        Input::Action(_)
        | Input::Cancel { .. }
        | Input::Char(_)
        | Input::FindBackspace
        | Input::Paste(_)
        | Input::ApprovalAnswer(_)
        | Input::ApprovalAuto
        | Input::ApprovalScroll(_)
        | Input::ConfirmCommand(_)
        | Input::Form(_)
        | Input::PickerKey(_)
        | Input::PickerClose
        | Input::LoginKey(_)
        | Input::ConfirmLogout(_)
        | Input::TrustMove(_)
        | Input::TrustChosen(_)
        | Input::MouseDown { .. }
        | Input::MouseDrag { .. }
        | Input::MouseUp { .. } => true,
        Input::QuitDeadline
        | Input::Resize { .. }
        | Input::Stream(_)
        | Input::Event(_)
        | Input::Runtime(_)
        | Input::Pending(..)
        | Input::MentionsExpanded(_)
        | Input::CompletionReady(_)
        | Input::CompletionsReady { .. }
        | Input::MemoryCommandDone(_)
        | Input::CompactDone(_)
        | Input::BashDone { .. }
        | Input::ThinkingChecked { .. }
        | Input::ModelChecked { .. }
        | Input::SwitchCompacted { .. }
        | Input::SessionDone(_)
        | Input::TrustResolved(_)
        | Input::CommandRefused(_)
        | Input::PickerOpened(_)
        | Input::LoginVerified { .. }
        | Input::LoginSaved { .. }
        | Input::LogoutDone { .. }
        | Input::ClipboardDone(_)
        | Input::ThemeReady { .. }
        | Input::SkillExpanded { .. } => false,
    }
}

/// The last press, for double/triple-click detection: where and when, and how many presses
/// in a row landed on the same cell inside the window (1, 2 or 3; capped at 3).
#[derive(Debug, Clone, Copy)]
struct LastClick {
    at: std::time::Instant,
    cell: (u16, u16),
    count: usize,
}

/// The live left-button gesture: the press cell, and where the pointer last was (the
/// auto-scroll edges are read off `last`). Screen rows/columns, `0`-based. `moved` says a
/// drag event actually moved the pointer, so a release on the press cell is a click (which
/// clears) and not a one-char selection. `anchor` is the press's located end, with whether
/// the pointer was on a char there (the end-side extension at release needs to know).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectDrag {
    press: (u16, u16),
    last: (u16, u16),
    moved: bool,
    /// The selection's fixed end, located once at press time (an anchor, not a cell, so
    /// scrolling during the drag cannot move it).
    anchor: Anchor,
    anchor_on_char: bool,
    /// How many presses began this gesture (1 click, 2 double, 3 triple): the release counts
    /// as a click — and clears — only for a plain 1.
    count: usize,
}

/// Which startup phase the app is in (ADR 0023 U4, plan B). `--pick` runs the event
/// loop once **before** a session exists; the loop returns as soon as the picker resolves, and
/// the caller then establishes the session and runs the loop again in the same terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    /// No session yet: only the picker overlay is live. No journal, driver or MCP connection
    /// exists, and a `Quit` here creates nothing.
    Picking,
    /// The ordinary phase: a session is open.
    Session,
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
pub enum Input {
    /// A semantic action, from the keymap or synthesized by the host.
    Action(Action),
    /// The `cancel` action (Ctrl-C by default) with the wall clock it was read at. The only
    /// action whose meaning depends on time: idle with an empty input, the first press arms
    /// "press again to quit" and a second inside [`QUIT_WINDOW`] quits. `update` stays pure;
    /// the loop (or a test) stamps the time.
    Cancel { now: Instant },
    /// The armed quit window closed: disarm and redraw, so the hint goes away. Sent by the
    /// loop on its `recv_timeout` (ADR 0023 U2: a time-driven display takes the timeout).
    QuitDeadline,
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
    /// The approval box's auto key `!` (ADR 0026 D9, a constant in `render.rs`): approve the
    /// pending request once **and** turn auto on for the rest of the session. Hardcoded like
    /// the answers (ADR 0023 U4, invariant 2).
    ApprovalAuto,
    /// A scroll of the approval box's body (`↑`/`↓`/PgUp/PgDn). Hardcoded like the answers:
    /// scrolling must never depend on the keymap, and it must never collide with an answer key.
    ApprovalScroll(ApprovalScroll),
    /// The hardcoded answer to a `/memory` confirmation (`true` = confirmed).
    ConfirmCommand(bool),
    /// The streaming text the projection has accumulated (ADR 0023 U2). Deltas never come
    /// through the channel: the loop reads the projection when a `Dirty` notification says so.
    Stream(StreamSnapshot),
    /// A live agent event.
    Event(AgentEvent),
    /// A runtime event (ADR 0017's turn failure).
    Runtime(RuntimeEvent),
    /// The pending queue's head changed (ADR 0023 U7): the request to show, and how many are
    /// queued. `None` means nothing is pending.
    Pending(Option<Box<PendingView<PendingRequest>>>, usize),
    /// One key inside the question form (ADR 0024 Q7).
    Form(FormKey),
    /// An `@` expansion finished (the result of [`Effect::ExpandMentions`]).
    MentionsExpanded(Result<Expansion, ExpandError>),
    /// An `@` completion finished (`None` = nothing to complete).
    CompletionReady(Option<Completion>),
    /// The host answered [`Effect::FetchCompletions`] with the candidates for one level of the
    /// command tree.
    CompletionsReady {
        fetch: command::Fetch,
        candidates: Vec<command::Candidate>,
    },
    /// A `/memory` command finished (the result of [`Effect::RunMemoryCommand`]).
    MemoryCommandDone(MemoryResult),
    /// A manual `/compact` finished (the result of [`Effect::Compact`]).
    CompactDone(Result<CompactionOutcome, String>),
    /// A `!`/`!!` command finished (the result of [`Effect::RunBash`]).
    BashDone {
        command: String,
        exclude_from_context: bool,
        result: Result<radian_coding::UserBash, String>,
    },
    /// A `/thinking` level was clamped against the current model's map (D11): `applied` is the
    /// level that will be used (`None` when the model supports no level at all) and
    /// `supported` the levels the completion offered.
    ThinkingChecked {
        requested: String,
        applied: Option<String>,
        supported: Vec<String>,
    },
    /// A `/model` target was vetted by the loop (ADR 0026 D5/D7). `tokens` is the current
    /// context estimate `T` (`None` = unknown, so it cannot exceed anything), `budget` the
    /// new model's `B'` (`None` = unknown, so no comparison is possible).
    ModelChecked {
        provider: String,
        model: String,
        tokens: Option<u64>,
        budget: Option<u64>,
    },
    /// The compaction before a switch finished (D7). Success queues the switch.
    SwitchCompacted {
        provider: String,
        model: String,
        result: Result<CompactionOutcome, String>,
    },
    /// A `/sessions`, `/export` or `/fork` finished: the notices to show.
    SessionDone(Vec<String>),
    /// A `/skill:<name> [args]` expansion finished (ADR 0025 S3): the message text to send, or
    /// a notice (an unknown skill or a file that can no longer be read). `steer` mirrors the
    /// turn state when the command was submitted.
    SkillExpanded {
        result: Result<String, String>,
        steer: bool,
    },
    /// The session's facade refused a command (ADR 0023 U5): an undeclared capability. The UI
    /// shows a notice instead of failing the loop.
    CommandRefused(radian_host::CommandRefused),
    /// One key for the session picker overlay (ADR 0023 U4). The picker owns its own parser,
    /// so the key is forwarded rather than resolved here.
    PickerKey(KeyEvent),
    /// Close the session picker without choosing (`Esc` on an unfocused overlay).
    PickerClose,
    /// The session picker's data arrived (the result of [`Effect::OpenPicker`]).
    PickerOpened(PickerData),
    /// One key inside the `/login` wizard (ADR 0026 D3). Hardcoded keys, like the approval.
    LoginKey(LoginKey),
    /// The `/login` verification request finished (D3). `name` is the provider the wizard is
    /// logging in (the answer is ignored unless the wizard is still waiting on it).
    LoginVerified {
        name: String,
        result: Result<(), String>,
    },
    /// The `auth.json` write of a `/login` finished: the wizard closes either way. The
    /// success notice names the provider and the way to switch to it (D3).
    LoginSaved {
        name: String,
        result: Result<(), String>,
    },
    /// The hardcoded answer to a `/logout` confirmation (`true` = remove).
    ConfirmLogout(bool),
    /// A `/logout` removal finished: the provider and the outcome. The notice names the
    /// provider (the removal is otherwise silent, D3's confirmation being the record).
    LogoutDone {
        provider: String,
        result: Result<(), String>,
    },
    /// The startup trust modal's cursor moved (ADR 0031 T3).
    TrustMove(i32),
    /// The startup trust modal chose an option; the loop writes `trust.json` and journals.
    TrustChosen(radian_host::trust::Choice),
    /// The trust choice was applied: the session's new state, for the startup notice.
    TrustResolved(radian_host::trust::Setup),
    /// The left button went down at a screen cell: the press that may start a transcript
    /// selection (and that clears the previous one). Only arrives when mouse reporting is on.
    MouseDown { row: u16, col: u16 },
    /// The left button moved while held: extends the selection.
    MouseDrag { row: u16, col: u16 },
    /// The left button came up: ends the selection and copies it ([`Effect::Copy`]).
    MouseUp { row: u16, col: u16 },
    /// The clipboard write finished (the result of [`Effect::Copy`]): the honest status-bar
    /// notice — what Radian did, never a claim about the clipboard it cannot verify.
    ClipboardDone(String),
    /// `/theme` finished (the result of [`Effect::Theme`]): the loaded theme (`None` when the
    /// switch did not happen, e.g. an unknown name or `NO_COLOR`) and the notices the loop
    /// produced (warnings, and whether the write-back happened).
    ThemeReady {
        theme: Option<Box<crate::theme::Theme>>,
        notices: Vec<String>,
    },
}

/// What the loop loaded for a picker overlay: the rows to show, the index for full-text
/// search, and the workspace they belong to.
///
/// It holds a `radian_session::Index`, whose `Connection` is not `Debug`, so this type does
/// not derive `Debug`.
pub struct PickerData {
    pub rows: Vec<radian_host::sessions::SessionRow>,
    pub index: Option<radian_session::Index>,
    pub workspace: String,
    /// `/sessions <query>`: open in full-text search mode with this query.
    pub query: Option<String>,
    /// Values to redact from a search snippet (the host's secret list).
    pub secrets: Vec<String>,
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
    SendFollowup(Message),
    /// Redirect the running turn: `Inbox::steer`.
    Steer(Message),
    /// Cancel the running turn.
    Cancel,
    /// Leave the app.
    Quit,
    /// Answer the pending request `request_id` (ADR 0023 U7). The loop runs it through the
    /// session's facade, which re-validates the `AnswerRequest` capability.
    AnswerRequest {
        request_id: u64,
        response: PendingResponse,
    },
    /// Expand the `@` references in `text` (reads files).
    ExpandMentions { text: String },
    /// Complete the `@` reference at the cursor (reads a directory).
    CompleteMention { text: String, cursor: usize },
    /// List one level's candidates in the host (I/O): the loop answers with
    /// [`Input::CompletionsReady`].
    FetchCompletions(command::Fetch),
    /// Run a `/memory` command through the shared executor (`confirmed` skips the prompt).
    RunMemoryCommand {
        command: MemoryCommand,
        confirmed: bool,
    },
    /// Run a manual `/compact` on the runtime, between turns.
    Compact { instructions: Option<String> },
    /// A `/model` target to vet: the loop resolves the credential and the new model's budget
    /// (ADR 0026 D5/D7) and reports back as `Input::ModelChecked`.
    ModelSwitch { provider: String, model: String },
    /// Compact the context to make room for a switch (D7): the loop runs the same compaction
    /// path as `/compact`, targeting the **new** model's budget.
    CompactForSwitch {
        provider: String,
        model: String,
        budget: u64,
    },
    /// Queue a model route for the next turn boundary (D5). The change is a route only.
    QueueModel { provider: String, model: String },
    /// A `/thinking` level to clamp against the current route's map (ADR 0026 D11); the loop
    /// reports back as `Input::ThinkingChecked`.
    SetThinking { level: String },
    /// Queue a thinking level for the next turn boundary (D11): a level name only.
    QueueThinking { level: String },
    /// Run a `/sessions`, `/export` or `/fork` request.
    Session(command::SessionOutcome),
    /// Load this workspace's sessions and open the picker overlay (`/sessions`). The result is
    /// [`Input::PickerOpened`]; loading is I/O, so it is the loop's job (ADR 0023 U3).
    OpenPicker { query: Option<String> },
    /// Run the `/login` verification request (ADR 0026 D3): one `GET <base>/models`. The
    /// answer is [`Input::LoginVerified`].
    LoginVerify {
        name: String,
        protocol: &'static str,
        base_url: String,
        key: String,
    },
    /// Write the `/login` entry into `credentials/auth.json` (D3; only auth.json is written).
    /// The answer is [`Input::LoginSaved`].
    LoginSave {
        name: String,
        key: String,
        env: Vec<(String, String)>,
    },
    /// Remove one stored credential (`/logout`, after the confirmation). The answer is
    /// [`Input::LogoutDone`].
    Logout { provider: String },
    /// Switch the session's auto-approval (ADR 0026 D9). The loop applies it through the
    /// facade, which flips the hooks' live policy, audits the switch and queues the durable
    /// `ConfigChanged` record for the next resume.
    SetApprovalPolicy { on: bool },
    /// Expand `/skill:<name> [args]` (ADR 0025 S3): the host reads the body and the loop sends
    /// the folded `<skill …>` text as one user message (`steer` picks steer vs follow-up). The
    /// answer is [`Input::SkillExpanded`].
    ExpandSkill {
        name: String,
        args: String,
        steer: bool,
    },
    /// List the discovered skills (`/skills`, ADR 0025 S3) as notices. The answer is
    /// [`Input::SessionDone`].
    ListSkills,
    /// `/jobs` (ADR 0037 decision 4): list the session's background jobs, or stop one/all. The
    /// loop runs it (the registry lives in the host) and answers with [`Input::SessionDone`].
    Jobs(command::JobsRequest),
    /// Write the transcript selection to the system clipboard (`tui.clipboard` picks the
    /// transport). The loop runs it and answers with [`Input::ClipboardDone`]: OSC 52 has no
    /// receipt, and `pbcopy` can fail — the loop owns both facts, `update` only owns the text.
    Copy { text: String },
    /// `/theme <name>` (ADR 0008 revision): load the theme and write `tui.theme` back to the
    /// config file. Both are I/O, so the loop runs them and answers with
    /// [`Input::ThemeReady`].
    Theme { name: String },
    /// Apply a startup trust choice (ADR 0031 T3): the loop writes `trust.json` through the
    /// facade and journals one `ProjectTrustChanged` per write. The answer is
    /// [`Input::TrustResolved`].
    SetTrust { choice: radian_host::trust::Choice },
    /// Run a `!`/`!!` command in the sandbox (ADR 0033). The loop spawns it and answers with
    /// [`Input::BashDone`].
    RunBash {
        command: String,
        exclude_from_context: bool,
    },
    /// Enqueue the `Ran \`cmd\`` message for `!` (ADR 0033) as an input that does **not** wake
    /// an idle driver (ADR 0011's `inject`), so the next turn carries it and no turn starts now.
    Inject(Message),
    /// Append one durable `UserBash` audit record (ADR 0033). `!!` emits it too.
    AuditUserBash {
        command: String,
        exclude_from_context: bool,
        exit_code: Option<i32>,
        cancelled: bool,
    },
    /// Cancel the running `!`/`!!` command (Esc, ADR 0033).
    CancelBash,
}

/// Effects are compared by **meaning** in the tests. `MemoryCommand` (a clap enum) has no
/// `PartialEq`, so two memory effects are equal when their parsed form is identical.
impl PartialEq for Effect {
    fn eq(&self, other: &Self) -> bool {
        use Effect::*;
        match (self, other) {
            (SendFollowup(a), SendFollowup(b)) | (Steer(a), Steer(b)) => a == b,
            (Cancel, Cancel) | (Quit, Quit) => true,
            (SetApprovalPolicy { on: a }, SetApprovalPolicy { on: b }) => a == b,
            (
                AnswerRequest {
                    request_id: a,
                    response: ao,
                },
                AnswerRequest {
                    request_id: b,
                    response: bo,
                },
            ) => a == b && ao == bo,
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
            (FetchCompletions(a), FetchCompletions(b)) => a == b,
            (OpenPicker { query: a }, OpenPicker { query: b }) => a == b,
            (Session(a), Session(b)) => a == b,
            (
                LoginVerify {
                    name: an,
                    protocol: ap,
                    base_url: ab,
                    key: ak,
                },
                LoginVerify {
                    name: bn,
                    protocol: bp,
                    base_url: bb,
                    key: bk,
                },
            ) => an == bn && ap == bp && ab == bb && ak == bk,
            (
                LoginSave {
                    name: an,
                    key: ak,
                    env: ae,
                },
                LoginSave {
                    name: bn,
                    key: bk,
                    env: be,
                },
            ) => an == bn && ak == bk && ae == be,
            (Logout { provider: a }, Logout { provider: b }) => a == b,
            (Copy { text: a }, Copy { text: b }) => a == b,
            (SetTrust { choice: a }, SetTrust { choice: b }) => a == b,
            (
                ExpandSkill {
                    name: an,
                    args: aa,
                    steer: as_,
                },
                ExpandSkill {
                    name: bn,
                    args: ba,
                    steer: bs,
                },
            ) => an == bn && aa == ba && as_ == bs,
            (ListSkills, ListSkills) => true,
            (Jobs(a), Jobs(b)) => a == b,
            (Theme { name: a }, Theme { name: b }) => a == b,
            (Inject(a), Inject(b)) => a == b,
            (CancelBash, CancelBash) => true,
            (
                RunBash {
                    command: ac,
                    exclude_from_context: ae,
                },
                RunBash {
                    command: bc,
                    exclude_from_context: be,
                },
            ) => ac == bc && ae == be,
            (
                AuditUserBash {
                    command: ac,
                    exclude_from_context: ae,
                    exit_code: aix,
                    cancelled: aca,
                },
                AuditUserBash {
                    command: bc,
                    exclude_from_context: be,
                    exit_code: bix,
                    cancelled: bca,
                },
            ) => ac == bc && ae == be && aix == bix && aca == bca,
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

/// The `/…` completion on screen: the char range a chosen candidate replaces, and the
/// candidates (best first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCompletion {
    pub range: std::ops::Range<usize>,
    pub candidates: Vec<command::Candidate>,
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
    /// The model's display name (ADR 0027 C2 `{display_name}`): the descriptor/catalog name
    /// when known, else the id. Set once at startup.
    pub model_display: Option<String>,
    /// The provider's name for the footer's `{provider}` placeholder. Set once at startup.
    pub provider: Option<String>,
    /// The session's working directory (the footer's `{path_home}`), and whether the git
    /// branch was read. Both are startup facts; the branch is refreshed by the host (this
    /// slice reads it once and exposes [`App::set_git_branch`] for the refresh).
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    /// The machine's hostname (ADR 0027 `host`), read once at startup.
    pub hostname: Option<String>,
    /// The model's catalog unit prices (ADR 0027). Kept for the host's startup
    /// wiring and its own tests; the footer's `cost` reads the **projection's** accumulated
    /// fee, because the fee is per call at the price in force then (a `/model` switch does not
    /// re-price earlier calls). [`App::set_model_price`] records it in both places.
    pub model_price: Option<radian_host::ModelPrice>,
    /// Whether automatic compaction is on (`context`'s `{auto}` marker), set once at startup.
    pub auto_compact: bool,
    /// The loop's current instant (ADR 0027). The timer never reads the wall clock;
    /// the loop stamps `now` once per iteration and tests set it to inject time.
    pub now: Instant,
    /// The spinner's origin (ADR 0027): `now - started` is the tick the spinner
    /// frame is a function of. Set at construction; tests set it to inject time.
    pub spinner_origin: Instant,
    /// The local turn timer behind `speed`/`elapsed`.
    pub timer: crate::timing::TurnTimer,
    /// The applied thinking level (ADR 0026 D11), for the footer's `reasoning` item. A queued
    /// switch shows the queued level until the driver applies it.
    pub thinking_level: Option<String>,
    /// The footer (ADR 0027 C2): its preset, glyphs and rows. Set from `[tui.footer]` at
    /// startup; the default is the two-row `pi` preset.
    pub footer: crate::footer::Footer,
    /// The contributor store (ADR 0027 C3). The background tasks write it; the renderer reads a
    /// snapshot, and the `git_branch`/`worktree`/`status` items come from it.
    pub contributions: std::sync::Arc<radian_host::ContributionStore>,
    /// `[tui.widgets]`: contributor items on a line above/below the input box (ADR 0027 C3).
    pub widgets: radian_host::config::TuiWidgetsConfig,
    pub session_id: String,
    pub entries: Vec<Entry>,
    /// How many entries are currently `live` (fed by stream deltas, not yet a committed
    /// message). Maintained so `push_message` scans the transcript for live entries only
    /// when there are some: a resumed session pushes every message with none, and the old
    /// unconditional `retain` was O(n^2) there (resume-perf A).
    live_count: usize,
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
    /// The session facts (ADR 0023 U5): turn, usage, budget, tier, MCP counts, the last tool
    /// call and the streaming buffer. `App` holds a **read handle** only; the host's
    /// `SessionFacade` owns the projection, and every fact is written there, never here.
    pub projection: Arc<SessionProjection>,
    pub quit: bool,
    pub note: Option<String>,
    /// The call ids of the tools running right now (ADR 0027's status note). A
    /// batch can run several; the note clears only when the last one finishes, so the footer
    /// does not say "tool shell running" while the next model request is already in flight.
    running_tools: std::collections::HashSet<String>,
    /// A compaction is running (ADR 0018): the input rule's status line shows `Compacting`
    /// (ADR 0027). Set on `CompactionStarted`, cleared when it finishes, fails or
    /// is skipped.
    pub compacting: bool,
    /// How many requests the queue holds (ADR 0023 U7), for the status bar.
    pub pending: usize,
    /// The startup screen: the leading block of entries (wordmark,
    /// collapsed information, safety lines, Tip). `None` before the host sets it.
    pub startup_screen: Option<StartupScreen>,
    /// How many leading entries are the startup block, so a toggle replaces exactly it and
    /// leaves the history alone.
    startup_len: usize,
    /// When the first idle Ctrl-C armed "press again to quit": the
    /// deadline a second Ctrl-C must beat. `None` means not armed. Pure state; the wall clock
    /// arrives on [`Input::Cancel`], so `update` never reads the clock.
    pub quit_pending: Option<Instant>,
    /// The in-session auto-approval state (ADR 0026 D9). The status bar shows `AUTO` while
    /// it is on; the approval box's `!` key and `/auto` switch it. The live policy itself
    /// is the hooks' — the loop applies it through [`Effect::SetApprovalPolicy`] — this is
    /// the UI's mirror, so `update` stays pure.
    pub auto: bool,
    /// The last request the UI answered: the queue's head stays that request for a moment, and
    /// re-showing it would flash a form the user just submitted.
    last_answered: Option<u64>,
    /// The layer stack (ADR 0023 U4): [`Layer::Surface`] at index 0, then overlays and modals
    /// above it. Only [`App::push_overlay`]/[`App::push_modal`] and the matching removals
    /// change it, so the tier rules hold by construction.
    layers: Vec<Layer>,
    /// Whether this process has a memory root. `/memory` runs through the loop, which owns the
    /// service; without one the command is refused with the same notice as before.
    pub memory_enabled: bool,
    /// The send waiting for its `@` expansion (see [`Effect::ExpandMentions`]).
    pending_expand: Option<PendingExpand>,
    /// The `/…` completion on screen: the range a candidate replaces and the candidates, best
    /// first. `None` when the line is not a slash command or the level has nothing to offer.
    /// (The overlay's *rows* live in the layer stack, next to the `@` list's.)
    pub slash_completion: Option<SlashCompletion>,
    /// The `@` completion the menu is showing (its range and raw candidates), so the highlighted
    /// row can be accepted. `None` unless the `@` list is up.
    pub mention_completion: Option<(std::ops::Range<usize>, Vec<String>)>,
    /// The highlighted row of the completion menu (slash or `@`). Reset to the first row
    /// whenever the menu is (re)built, so typing keeps the highlight on the best match.
    pub completion_cursor: usize,
    /// The host fetch the current completion waits for: a result is applied only when it
    /// answers *this* fetch, so a stale answer cannot land on a newer line.
    pending_fetch: Option<command::Fetch>,
    /// The queued switch, shown as `next: <provider/model>` until the driver applies it.
    pub next_model: Option<String>,
    /// The queued thinking level (D11), cleared when the driver applies it.
    pub next_thinking: Option<String>,
    /// The `!`/`!!` command currently running (ADR 0033); `Some` blocks a second one.
    pub bash_running: Option<String>,
    /// A `!`/`!!` command submitted while a turn was running: it starts when the turn ends.
    pub queued_bash: Option<(String, bool)>,
    /// Whether this session has a shell (`!`) at all: false on the `safe` tier (ADR 0012).
    pub bash_available: bool,
    /// The exact `Ran \`cmd\`` text we displayed live as a [`EntryKind::Bash`] block for a `!`
    /// command. When that message is admitted to the transcript, `push_message` skips it so
    /// the block is not shown twice; a resume has no pending text and renders it as a block.
    bash_context_pending: Option<String>,
    /// The color theme (config `tui.theme`, `NO_COLOR`).
    pub theme: Theme,
    /// `tui.brand` (ADR 0027): the brand text, or `None` for the glyph set's
    /// default. The footer's `brand` item and the startup banner both read it.
    pub brand: Option<String>,
    /// `tui.spinner` (ADR 0027): the running indicator. `pulse` by default.
    pub spinner_kind: crate::spinner::Kind,
    /// The key bindings (config `tui.keybindings`), defaulting to the pre-config key table.
    pub keymap: Keymap,
    /// How the input area is drawn (config `tui.input_style`), defaulting to `bar`.
    pub input_style: InputStyle,
    pub total_lines: usize,
    pub view_height: u16,
    pub max_scroll: usize,
    /// The workspace root, so `Tab` can complete `@` file references. `None` until the host
    /// sets it; without it `Tab` does nothing.
    pub workspace: Option<PathBuf>,
    /// The transcript width the renderer last used, so a find recomputes on a resize.
    pub transcript_width: usize,
    /// The startup phase (ADR 0023 U4): [`Startup::Picking`] while the pre-session `--pick`
    /// overlay is the whole point of the loop, [`Startup::Session`] otherwise.
    pub startup: Startup,
    /// Set when the picker resolves during [`Startup::Picking`]. The pre-session loop leaves
    /// as soon as it is `Some`; the caller reads it with [`App::take_picker_outcome`].
    picker_outcome: Option<PickerOutcome>,
    /// The transcript selection, anchored to entries and offsets ([`Selection`]) — never to
    /// cells — so new output, scrolling and reflow leave it where it was. `Some` also while
    /// the drag is still moving; the renderer highlights it each frame.
    pub selection: Option<Selection>,
    /// The live left-button gesture, from press to release.
    selecting: Option<SelectDrag>,
    /// The last press, for double/triple-click detection.
    last_click: Option<LastClick>,
    /// The revision and token counts of the compaction that just finished (ADR 0028 R5): the
    /// summary message that follows takes them for its one-liner. Empty on a resume replay.
    pending_compaction: Option<(u64, u64, u64)>,
    /// Whether mouse reporting is on (`tui.mouse`, default on). With it off the terminal
    /// never sends a mouse event and every mouse method here is a no-op: none of the
    /// selection/copy features exist (the belt behind the terminal's suspenders).
    mouse_enabled: bool,
    /// How a copy reaches the system clipboard (config `tui.clipboard`, default OSC 52).
    pub clipboard: ClipboardMode,
    /// How the reasoning channel displays (ADR 0028 R3, `tui.display.reasoning`). Part of
    /// the block/layout cache keys: toggling re-lays the reasoning entries out.
    pub reasoning_mode: ReasoningMode,
    /// How tool card bodies fold (ADR 0028 R4, `tui.display.tool_output` +
    /// `tool_preview_lines`). Also part of the layout cache keys.
    pub tool_display: crate::tools::ToolDisplay,
    /// `tui.display.mermaid` (ADR 0028 R8), default true: draw a closed ```mermaid block as a
    /// diagram. `false` renders every code block as source. Part of `DisplayModes`, so it is
    /// in the layout cache key.
    pub mermaid_enabled: bool,
    /// `tui.display.highlight` (ADR 0028 R2), default true: highlight a closed code block whose
    /// info string names a common language. `false` renders every code block as plain text.
    /// Part of `DisplayModes`, so it is in the layout cache key.
    pub highlight_enabled: bool,
}

impl App {
    pub fn new(model_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            model_display: None,
            provider: None,
            cwd: None,
            git_branch: None,
            hostname: None,
            model_price: None,
            auto_compact: true,
            now: Instant::now(),
            spinner_origin: Instant::now(),
            timer: crate::timing::TurnTimer::default(),
            thinking_level: None,
            footer: crate::footer::Footer::default(),
            contributions: radian_host::ContributionStore::new(),
            widgets: radian_host::config::TuiWidgetsConfig::default(),
            session_id: session_id.into(),
            entries: Vec::new(),
            live_count: 0,
            input: Composer::new(),
            history: Vec::new(),
            history_cursor: None,
            history_stash: String::new(),
            scroll: 0,
            follow: true,
            projection: Arc::new(SessionProjection::new()),
            quit: false,
            note: None,
            running_tools: std::collections::HashSet::new(),
            compacting: false,
            pending: 0,
            startup_screen: None,
            startup_len: 0,
            quit_pending: None,
            auto: false,
            last_answered: None,
            layers: vec![Layer::Surface],
            memory_enabled: false,
            pending_expand: None,
            slash_completion: None,
            mention_completion: None,
            completion_cursor: 0,
            pending_fetch: None,
            next_model: None,
            next_thinking: None,
            bash_running: None,
            queued_bash: None,
            bash_available: true,
            bash_context_pending: None,
            theme: Theme::default(),
            brand: None,
            spinner_kind: crate::spinner::Kind::default(),
            keymap: Keymap::default(),
            input_style: InputStyle::default(),
            total_lines: 0,
            view_height: 0,
            max_scroll: 0,
            workspace: None,
            transcript_width: 80,
            startup: Startup::Session,
            picker_outcome: None,
            selection: None,
            selecting: None,
            last_click: None,
            pending_compaction: None,
            mouse_enabled: true,
            clipboard: ClipboardMode::Osc52,
            // ADR 0028 R3's default: thinking collapsed to one line.
            reasoning_mode: ReasoningMode::Collapsed,
            tool_display: crate::tools::ToolDisplay::default(),
            mermaid_enabled: true,
            highlight_enabled: true,
        }
    }

    /// Enters the pre-session `--pick` phase: the picker overlay is pushed **focused** (it is
    /// the whole point of the phase) and [`Startup`] becomes [`Startup::Picking`].
    pub fn start_picking(&mut self, mut picker: Picker) {
        picker.focus();
        self.startup = Startup::Picking;
        self.push_overlay(Overlay::Picker(picker));
    }

    /// Leaves the pre-session phase: the picker overlay is removed and the app is a normal
    /// session app again (the caller then opens the session).
    pub fn finish_picking(&mut self) {
        self.picker_close();
        self.startup = Startup::Session;
    }

    /// Takes the pre-session picker's choice, if it has one.
    pub fn take_picker_outcome(&mut self) -> Option<PickerOutcome> {
        self.picker_outcome.take()
    }

    /// True once the pre-session picker has resolved, so the loop can leave. Always false in
    /// [`Startup::Session`].
    pub fn picking_resolved(&self) -> bool {
        self.picker_outcome.is_some()
    }

    /// The display modes a frame lays the transcript out with (ADR 0028 R1/R3/R4): the
    /// reasoning fold and the tool fold together are the layout cache key.
    pub fn display_modes(&self) -> crate::tools::DisplayModes {
        crate::tools::DisplayModes {
            reasoning: self.reasoning_mode,
            tools: self.tool_display,
            mermaid: self.mermaid_enabled,
            highlight: self.highlight_enabled,
            glyphs: self.footer_glyphs(),
        }
    }

    /// Records the effective input budget `B` (no budget is configured when this is `None`).
    ///
    /// The host normally writes this through the projection when it opens the session; the
    /// method is the seam the tests use.
    pub fn set_context_window(&mut self, input_budget: Option<u64>) {
        self.projection.set_context_window(input_budget);
    }

    /// Records the permission tier's display name, for the status bar.
    pub fn set_tier(&mut self, tier: impl Into<String>) {
        self.projection.set_tier(Some(tier.into()));
    }

    /// Whether this process has a memory root, for `/memory`. Set once at startup; the service
    /// itself stays on the loop's side (ADR 0023 U3).
    pub fn set_memory_enabled(&mut self, enabled: bool) {
        self.memory_enabled = enabled;
    }

    /// The session's in-session auto state (ADR 0026 D9), set at startup from the resolved
    /// policy and afterwards only by the switch itself.
    pub fn set_auto(&mut self, auto: bool) {
        self.auto = auto;
    }

    /// Applies the wired `tui.*` configuration. A bad keymap (a key bound to two actions) is
    /// an error the host turns into a startup refusal; unknown actions/keys are warnings.
    pub fn apply_config(
        &mut self,
        config: &radian_host::config::Config,
        no_color: bool,
    ) -> Result<Vec<String>, String> {
        let keymap = Keymap::from_config(&config.tui.keybindings).map_err(|e| e.to_string())?;
        let mut warnings = keymap.warnings.clone();
        self.keymap = keymap;
        // The theme: built-in names resolve directly, anything else is a file in
        // `~/.radian/themes/` (ADR 0029). NO_COLOR wins over both the file and the
        // `[tui.colors]` overrides (ADR 0029 decision 7); the theme module owns the rules.
        let (theme, theme_warnings) = crate::theme::load(
            config.tui.theme.as_deref(),
            no_color,
            crate::theme::truecolor_env(),
            radian_host::themes_dir().as_deref(),
            &config.tui.colors,
        );
        self.theme = theme;
        warnings.extend(theme_warnings);
        // `tui.brand` (ADR 0027): a configured short text, else `None` for the
        // glyph set's default. The reader already refused an over-wide value.
        self.brand = config.tui.brand.clone();
        // `tui.spinner` (ADR 0027): an unknown name warns and uses `pulse`.
        self.spinner_kind = match config.tui.spinner.as_deref() {
            None => crate::spinner::Kind::default(),
            Some(name) => crate::spinner::Kind::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.spinner = {name:?} is not one of pulse|arc|quadrant|braille; using pulse"
                ));
                crate::spinner::Kind::default()
            }),
        };
        // An unknown `tui.input_style` is a skipped key (the reader warns once); the TUI falls
        // back to the default.
        self.input_style = config
            .tui
            .input_style
            .as_deref()
            .and_then(InputStyle::parse)
            .unwrap_or_default();
        // `tui.display.reasoning` (ADR 0028 R3, default collapsed) and
        // `tui.display.tool_output` + `tool_preview_lines` (R4, default errors / 5). An
        // unknown spelling never reaches here (the host reader skips it with a warning).
        self.reasoning_mode = config
            .tui
            .display_reasoning
            .as_deref()
            .and_then(ReasoningMode::parse)
            .unwrap_or_default();
        self.tool_display = crate::tools::ToolDisplay {
            fold: config
                .tui
                .display_tool_output
                .as_deref()
                .and_then(crate::tools::ToolFold::parse)
                .unwrap_or_default(),
            preview_lines: config
                .tui
                .display_tool_preview_lines
                .map(|lines| lines as usize)
                .unwrap_or(5),
        };
        // `tui.display.mermaid` (ADR 0028 R8, default true).
        self.mermaid_enabled = config.tui.display_mermaid.unwrap_or(true);
        self.highlight_enabled = config.tui.display_highlight.unwrap_or(true);
        // `tui.clipboard` (default OSC 52). An unknown spelling never reaches here (the host
        // config reader skips it with a warning); `pbcopy` off macOS does: warn once and fall
        // back, because the copy effect would have no program to run.
        self.mouse_enabled = config.tui.mouse.unwrap_or(true);
        self.clipboard = config
            .tui
            .clipboard
            .as_deref()
            .and_then(ClipboardMode::parse)
            .unwrap_or_default();
        if self.clipboard == ClipboardMode::PbCopy && !self.clipboard.supported_on_this_platform() {
            self.clipboard = ClipboardMode::Osc52;
            warnings.push("tui.clipboard = \"pbcopy\" is macOS-only; using osc52".into());
        }
        // `[tui.footer]` (ADR 0027 C2) and `[tui.widgets]` (C3).
        self.widgets = config.tui.widgets.clone();
        warnings.extend(self.apply_footer(&config.tui.footer));
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
        self.projection.set_mcp(configured, open);
    }

    /// Replaces the read handle with the host's projection, so the renderer reads the same
    /// facts the facade owns.
    /// The inputs to the herdr state function (ADR 0034 H2): the turn state, the pending
    /// queue's length, and whether the startup trust prompt is waiting.
    pub fn herdr_state_inputs(&self) -> (TurnState, usize, bool) {
        let trust = matches!(self.top_modal(), Some(Modal::Trust(_)));
        (self.projection.read().turn(), self.pending, trust)
    }

    /// What is blocking, for herdr's category-only message (ADR 0034 H2). The approval's tool
    /// name only - never the command, path or question body.
    pub fn herdr_blocked(&self) -> Option<radian_host::herdr::Blocked> {
        match self.top_modal() {
            Some(Modal::Approval(prompt)) => Some(radian_host::herdr::Blocked::Approval(
                prompt.request.tool.clone(),
            )),
            Some(Modal::Question(_)) => Some(radian_host::herdr::Blocked::Question),
            _ => None,
        }
    }

    pub fn set_projection(&mut self, projection: Arc<SessionProjection>) {
        self.projection = projection;
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

    /// One mouse-wheel notch (ADR 0023 U2, plan B). It acts on the topmost **scrollable**
    /// layer: a modal consumes it (the approval prompt does not scroll), a picker overlay moves
    /// its selection, the `@` completion list consumes it (it has no scroll state of its own,
    /// and the transcript behind it must not move), and otherwise the transcript scrolls. The
    /// transcript path goes through the same [`Action::ScrollLineUp`]/[`Action::ScrollLineDown`]
    /// code as the keys, `MOUSE_WHEEL_LINES` lines per notch.
    pub fn mouse_scroll(&mut self, direction: MouseScrollDirection) -> UpdateResult {
        if self.has_modal() {
            return UpdateResult::none();
        }
        if self.picker().is_some() {
            let key = match direction {
                MouseScrollDirection::Up => KeyCode::Up,
                MouseScrollDirection::Down => KeyCode::Down,
            };
            // A list selection is discrete: one notch moves it one row (the transcript below
            // is the one that scrolls `MOUSE_WHEEL_LINES`).
            return self.picker_key(KeyEvent::new(key, KeyModifiers::NONE));
        }
        if self.completion().is_some() {
            return UpdateResult::none();
        }
        let action = match direction {
            MouseScrollDirection::Up => Action::ScrollLineUp,
            MouseScrollDirection::Down => Action::ScrollLineDown,
        };
        let mut dirty = false;
        for _ in 0..MOUSE_WHEEL_LINES {
            dirty |= self.update_action(action).dirty;
        }
        UpdateResult {
            dirty,
            effects: Vec::new(),
        }
    }

    // ---------- transcript selection (left-button drag) ----------

    /// Whether a left-button drag is live (the loop auto-scrolls only while it is).
    pub fn is_selecting(&self) -> bool {
        self.selecting.is_some()
    }

    /// The left button went down. Any press clears the previous selection (a later release
    /// without movement is the click that clears, per the contract); only a press **inside
    /// the transcript** starts a new one — never on the input box, the status bar, a modal
    /// prompt or the picker overlay, and never with `tui.mouse = false`. A press that starts
    /// nothing and clears nothing is not dirty: an idle screen is not redrawn (ADR 0023 U2).
    pub fn mouse_down(&mut self, row: u16, col: u16) -> UpdateResult {
        if !self.mouse_enabled {
            return UpdateResult::none();
        }
        let count = self.bump_click_count(row, col);
        let had = self.selection.take();
        let inside = !self.has_modal()
            && self.picker().is_none()
            && (row as usize) < usize::from(self.view_height)
            && self.transcript_width > 0;
        let cleared = had.is_some();
        if !inside {
            return cleared
                .then(UpdateResult::dirty)
                .unwrap_or(UpdateResult::none());
        }
        // The fixed end is located once, here: a scroll during the drag moves the view, not
        // the anchor. An empty transcript simply starts nothing.
        let press_line = self.scroll + row as usize;
        let Some(hit) = select::locate_hit(
            &self.entries,
            self.display_modes(),
            self.transcript_width,
            press_line,
            col as usize,
        ) else {
            return cleared
                .then(UpdateResult::dirty)
                .unwrap_or(UpdateResult::none());
        };
        // A multi-click replaces the just-cleared selection with its bigger range and
        // anchors the gesture at its start, so a following drag extends from the word (or
        // the entry).
        let (anchor, anchor_on_char) = match count {
            2 => match select::run_at(
                &self.entries,
                self.display_modes(),
                self.transcript_width,
                press_line,
                col as usize,
            ) {
                Some((start, end)) => {
                    self.selection = Some(Selection {
                        anchor: start,
                        head: end,
                    });
                    (start, true)
                }
                None => (hit.anchor, hit.on_char),
            },
            3 => {
                let len = select::entry_display_len(&self.entries, hit.anchor.entry);
                if len > 0 {
                    self.selection = Some(Selection {
                        anchor: Anchor {
                            entry: hit.anchor.entry,
                            offset: 0,
                        },
                        head: Anchor {
                            entry: hit.anchor.entry,
                            offset: len,
                        },
                    });
                    (
                        Anchor {
                            entry: hit.anchor.entry,
                            offset: 0,
                        },
                        true,
                    )
                } else {
                    (hit.anchor, hit.on_char)
                }
            }
            _ => (hit.anchor, hit.on_char),
        };
        self.selecting = Some(SelectDrag {
            press: (row, col),
            last: (row, col),
            moved: false,
            anchor,
            anchor_on_char,
            count,
        });
        cleared
            .then(UpdateResult::dirty)
            .unwrap_or(UpdateResult::none())
    }

    /// The press counter for double/triple-click: one more when the same cell is pressed
    /// again inside [`DOUBLE_CLICK_WINDOW`] (capped at 3), otherwise back to 1.
    fn bump_click_count(&mut self, row: u16, col: u16) -> usize {
        let next = match self.last_click {
            Some(click)
                if click.cell == (row, col) && click.at.elapsed() <= DOUBLE_CLICK_WINDOW =>
            {
                (click.count + 1).min(3)
            }
            _ => 1,
        };
        self.last_click = Some(LastClick {
            at: std::time::Instant::now(),
            cell: (row, col),
            count: next,
        });
        next
    }

    /// The left button moved while held: extend the selection to the pointer's cell.
    pub fn mouse_drag(&mut self, row: u16, col: u16) -> UpdateResult {
        if !self.mouse_enabled || self.selecting.is_none() {
            return UpdateResult::none();
        }
        if let Some(drag) = self.selecting.as_mut() {
            if (row, col) != drag.press {
                drag.moved = true;
            }
            drag.last = (row, col);
        }
        self.extend_selection_to(row, col);
        UpdateResult::dirty()
    }

    /// The left button came up: the release cell re-extends the selection one last time —
    /// through the char under it ([`Self::extend_selection_to`]). A click — the pointer
    /// never left the press cell — leaves nothing selected and copies nothing.
    pub fn mouse_up(&mut self, row: u16, col: u16) -> UpdateResult {
        let Some(drag) = self.selecting else {
            return UpdateResult::none();
        };
        if !self.mouse_enabled {
            return UpdateResult::none();
        }
        self.selecting = None;
        if drag.count == 1 && !drag.moved && (row, col) == drag.press {
            // The click: the press already cleared; nothing to copy. A double/triple click's
            // release copies the word or entry it just selected.
            return UpdateResult::none();
        }
        self.extend_selection_to(row, col);
        let Some(selection) = self.selection else {
            return UpdateResult::none();
        };
        if selection.is_empty() {
            self.selection = None;
            return UpdateResult::none();
        }
        let text = select::logical_text(&self.entries, &selection);
        if text.is_empty() {
            self.selection = None;
            return UpdateResult::dirty();
        }
        // Refuse the absurd honestly, with the reason: the transcript selection's own line.
        if text.len() > MAX_CLIPBOARD_BYTES {
            self.selection = None;
            self.push_notice(format!(
                "selection too large to copy: {} bytes (the limit is {})",
                text.len(),
                MAX_CLIPBOARD_BYTES
            ));
            return UpdateResult::dirty();
        }
        // `off` means the selection still draws, but releasing it copies nothing.
        if self.clipboard == ClipboardMode::Off {
            return UpdateResult::none();
        }
        UpdateResult::effect(Effect::Copy { text })
    }

    /// One auto-scroll tick of a live drag: the pointer held at (or beyond) the transcript's
    /// top or bottom edge moves the view one line toward it, and the selection's head is
    /// re-located under the pointer. Returns whether anything changed. The loop calls this
    /// only while a drag is live, on its `recv_timeout` tick.
    pub fn mouse_autoscroll(&mut self) -> bool {
        let Some(drag) = self.selecting else {
            return false;
        };
        let view = usize::from(self.view_height.max(1));
        let direction: i32 = if drag.last.0 == 0 {
            -1
        } else if drag.last.0 as usize >= view {
            1
        } else {
            return false;
        };
        let before = self.scroll;
        self.scroll_by(direction);
        if self.scroll == before {
            return false;
        }
        self.extend_selection_to(drag.last.0, drag.last.1);
        true
    }

    /// Re-locates the selection under a pointer cell: the earlier of {press anchor, pointer}
    /// is the inclusive start, the later the end — extended **through** the char under it
    /// when that char is what the pointer is on (native-selection semantics: dragging across
    /// "abc" copies "abc" in either direction). A gap (past a line's end, or on a folded
    /// boundary) is a plain boundary and extends nothing. The stored selection is the
    /// normalized `start..end`; rendering and copying read exactly this range.
    fn extend_selection_to(&mut self, row: u16, col: u16) {
        let Some(drag) = self.selecting else {
            return;
        };
        let head_line = self.scroll + row as usize;
        let Some(hit) = select::locate_hit(
            &self.entries,
            self.display_modes(),
            self.transcript_width,
            head_line,
            col as usize,
        ) else {
            return;
        };
        let (start, mut end) =
            if (hit.anchor.entry, hit.anchor.offset) < (drag.anchor.entry, drag.anchor.offset) {
                (hit.anchor, drag.anchor)
            } else {
                (drag.anchor, hit.anchor)
            };
        let end_on_char = if end == hit.anchor {
            hit.on_char
        } else {
            drag.anchor_on_char
        };
        if end_on_char {
            end.offset += select::char_len_at(&self.entries, end).unwrap_or(1);
        }
        self.selection = Some(Selection {
            anchor: start,
            head: end,
        });
    }

    // ---------- the layer stack (ADR 0023 U4) ----------

    /// The layer stack, bottom ([`Layer::Surface`]) first. Read-only; use the `push_*` and
    /// removal methods to change it.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Pushes an overlay. ADR 0023 U4: an overlay is never above **any** modal, so when modals
    /// are up the overlay is inserted just below the lowest one (below the highest one would
    /// put it above every modal under that). With no modal it goes on top.
    pub fn push_overlay(&mut self, overlay: Overlay) {
        match self
            .layers
            .iter()
            .position(|layer| layer.tier() == Tier::Modal)
        {
            Some(index) => self.layers.insert(index, Layer::Overlay(overlay)),
            None => self.layers.push(Layer::Overlay(overlay)),
        }
    }

    /// Pushes a modal on top of the whole stack.
    pub fn push_modal(&mut self, modal: Modal) {
        self.layers.push(Layer::Modal(modal));
    }

    /// Pushes the startup trust modal (ADR 0031 T3). Called once, after the terminal is
    /// entered, when the workspace has project resources and no saved decision.
    pub fn push_trust_modal(&mut self, workspace: PathBuf) {
        self.push_modal(Modal::Trust(Box::new(TrustWizard::new(workspace))));
    }

    /// Removes the topmost overlay matching `pick`. Returns it, or `None` when absent.
    fn remove_overlay(&mut self, mut pick: impl FnMut(&Overlay) -> bool) -> Option<Overlay> {
        let index = self.layers.iter().rposition(|layer| match layer {
            Layer::Overlay(overlay) => pick(overlay),
            _ => false,
        })?;
        match self.layers.remove(index) {
            Layer::Overlay(overlay) => Some(overlay),
            _ => None,
        }
    }

    /// Removes the topmost modal matching `pick`. Returns it, or `None` when absent.
    fn remove_modal(&mut self, mut pick: impl FnMut(&Modal) -> bool) -> Option<Modal> {
        let index = self.layers.iter().rposition(|layer| match layer {
            Layer::Modal(modal) => pick(modal),
            _ => false,
        })?;
        match self.layers.remove(index) {
            Layer::Modal(modal) => Some(modal),
            _ => None,
        }
    }

    /// The topmost approval modal, mutably, so the renderer can feed the measured body size
    /// back into it (the way the transcript's viewport is fed back) and the scroll keys can
    /// move its window.
    pub fn approval_modal_mut(&mut self) -> Option<&mut PendingApproval> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Approval(prompt)) => Some(&mut **prompt),
            _ => None,
        })
    }

    /// The topmost `/login` wizard, mutably, so `update` can step it.
    pub fn login_modal_mut(&mut self) -> Option<&mut LoginWizard> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Login(wizard)) => Some(&mut **wizard),
            _ => None,
        })
    }

    /// The `/logout` confirmation on screen, if any.
    pub fn pending_logout(&self) -> Option<&str> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::ConfirmLogout(provider)) => Some(provider.as_str()),
            _ => None,
        })
    }

    /// The topmost modal, if any. The input box is not editable while one is up.
    pub fn top_modal(&self) -> Option<&Modal> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(modal) => Some(modal),
            _ => None,
        })
    }

    pub fn has_modal(&self) -> bool {
        self.top_modal().is_some()
    }

    /// The approval prompt on screen, if any.
    pub fn pending_approval(&self) -> Option<&ApprovalRequest> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Approval(prompt)) => Some(&prompt.request),
            _ => None,
        })
    }

    /// The `/memory` confirmation on screen, if any.
    pub fn pending_command(&self) -> Option<&PendingCommand> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::ConfirmCommand(command)) => Some(command),
            _ => None,
        })
    }

    /// The `@` completion candidates, if the list is up.
    pub fn completion(&self) -> Option<&[crate::command::Candidate]> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Completion(candidates)) => Some(candidates.as_slice()),
            _ => None,
        })
    }

    fn completion_clear(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Completion(_)));
        self.mention_completion = None;
    }

    pub fn completion_set(&mut self, candidates: Vec<crate::command::Candidate>) {
        self.completion_clear();
        if !candidates.is_empty() {
            // A fresh menu highlights its first (best-ranked) row.
            self.completion_cursor = 0;
            self.push_overlay(Overlay::Completion(candidates));
        }
    }

    /// The word the slash menu is completing: the text from the level's range start to the
    /// cursor. The menu filters on it, so typing a provider name finds that provider's models
    /// (ADR 0035).
    pub fn completion_filter(&self) -> String {
        let Some(slash) = &self.slash_completion else {
            return String::new();
        };
        let chars: Vec<char> = self.input.text().chars().collect();
        let end = self.input.cursor().min(chars.len());
        let start = slash.range.start.min(end);
        chars[start..end].iter().collect()
    }

    /// Whether a completion menu is on screen (the slash list or the `@` list).
    pub fn menu_open(&self) -> bool {
        self.slash_completion.is_some() || self.completion().is_some()
    }

    /// Closes the menu and keeps the input as it is.
    fn menu_close(&mut self) {
        self.completion_clear();
        self.slash_completion = None;
    }

    /// The number of rows in the open menu.
    fn menu_len(&self) -> usize {
        if let Some(slash) = &self.slash_completion {
            return slash.candidates.len();
        }
        self.completion()
            .map(<[crate::command::Candidate]>::len)
            .unwrap_or(0)
    }

    /// Moves the menu highlight, wrapping.
    fn move_menu(&mut self, delta: i32) {
        let len = self.menu_len();
        if len == 0 {
            return;
        }
        self.completion_cursor =
            (self.completion_cursor as i32 + delta).rem_euclid(len as i32) as usize;
    }

    /// Accepts the highlighted row: Tab and Enter do the same, and Enter never sends.
    fn accept_menu_candidate(&mut self) -> UpdateResult {
        if self.slash_completion.is_some() {
            return self.accept_slash_candidate();
        }
        self.accept_mention_candidate()
    }

    /// Accepts the highlighted `@` candidate (a path), completing the reference.
    fn accept_mention_candidate(&mut self) -> UpdateResult {
        let Some((range, candidates)) = self.mention_completion.take() else {
            return UpdateResult::none();
        };
        let Some(candidate) = candidates.get(self.completion_cursor) else {
            return UpdateResult::none();
        };
        // A directory keeps its trailing `/`; a file gets a trailing space (mention.rs's rule).
        let insert = if candidate.ends_with('/') {
            candidate.clone()
        } else {
            format!("{candidate} ")
        };
        self.replace_range(range, &insert);
        self.menu_close();
        UpdateResult::dirty()
    }

    /// The session picker overlay, if it is up.
    pub fn picker(&self) -> Option<&Picker> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Picker(picker)) => Some(picker),
            _ => None,
        })
    }

    fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Picker(picker)) => Some(picker),
            _ => None,
        })
    }

    fn picker_close(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Picker(_)));
    }

    // ---------- in-transcript search (Phase 5) ----------

    /// Records the transcript width the renderer used. A width change re-wraps the transcript,
    /// so an open find box recomputes its matches — once per resize, never per frame.
    pub fn set_transcript_width(&mut self, width: usize) {
        if self.transcript_width == width {
            return;
        }
        self.transcript_width = width;
        // The wordmark drops to its one-line fallback below a width; rebuilding is cheap and
        // only happens when the width actually changed.
        if self.startup_screen.is_some() {
            self.rebuild_startup_entries();
        }
        if self.find_state().is_some() {
            self.find_recompute();
        }
    }

    /// Sets the startup screen and builds its leading entry block.
    pub fn set_startup(&mut self, startup: StartupScreen) {
        self.startup_screen = Some(startup);
        self.rebuild_startup_entries();
    }

    /// `Ctrl-P` (the `toggle_startup` action): fold or unfold the startup information.
    pub fn toggle_startup(&mut self) -> UpdateResult {
        let Some(startup) = self.startup_screen.as_mut() else {
            return UpdateResult::none();
        };
        startup.expanded = !startup.expanded;
        self.rebuild_startup_entries();
        UpdateResult::dirty()
    }

    /// The leading startup block, rebuilt from [`StartupScreen`]: the wordmark (the wide form
    /// when it fits the current width), the collapsed line or the full lines, the safety
    /// lines (always shown), and the Tip.
    fn rebuild_startup_entries(&mut self) {
        let Some(startup) = self.startup_screen.clone() else {
            self.entries
                .drain(0..self.startup_len.min(self.entries.len()));
            self.startup_len = 0;
            return;
        };
        let mut block: Vec<Entry> = startup
            .banner_for_width(self.transcript_width)
            .into_iter()
            .map(|line| Entry::new(EntryKind::Banner, line))
            .collect();
        if startup.expanded {
            block.push(Entry::new(
                EntryKind::Startup,
                "- startup information (Ctrl-P to fold)",
            ));
            for line in &startup.lines {
                block.push(Entry::new(EntryKind::Startup, line.clone()));
            }
        } else {
            block.push(Entry::new(
                EntryKind::Startup,
                format!("+ {}   (Ctrl-P to expand)", startup.summary),
            ));
        }
        for line in &startup.safety {
            block.push(Entry::new(EntryKind::Warning, line.clone()));
        }
        if let Some(tip) = &startup.tip {
            block.push(Entry::new(EntryKind::Tip, tip.clone()));
        }
        let block_len = block.len();
        let replace = self.startup_len.min(self.entries.len());
        self.entries.splice(0..replace, block);
        self.startup_len = block_len;
    }

    /// The open find box, if any.
    pub fn find_state(&self) -> Option<&FindState> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Find(find)) => Some(find),
            _ => None,
        })
    }

    fn find_mut(&mut self) -> Option<&mut FindState> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Find(find)) => Some(find),
            _ => None,
        })
    }

    /// Opens the find box anchored at the current viewport top (`Ctrl-F`). Refused while a
    /// modal prompt is up.
    pub fn find_open(&mut self) {
        if self.has_modal() || self.find_state().is_some() {
            return;
        }
        let anchored = self.scroll;
        self.push_overlay(Overlay::Find(FindState::open(anchored)));
    }

    /// Closes the find box; the viewport stays where it is (`Esc`).
    pub fn find_close(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Find(_)));
    }

    /// The earlier match (`Enter`, or `Ctrl-F` again).
    pub fn find_prev(&mut self) {
        if let Some(find) = self.find_mut() {
            find.prev();
        }
        self.find_jump();
    }

    /// The later match (`Shift-Enter`, `Ctrl-G`).
    pub fn find_next(&mut self) {
        if let Some(find) = self.find_mut() {
            find.next();
        }
        self.find_jump();
    }

    fn find_recompute(&mut self) {
        let width = self.transcript_width;
        // Split the borrow: the entries to match and the layer that holds the query are two
        // different fields of `self`.
        let modes = self.display_modes();
        let App {
            entries, layers, ..
        } = self;
        if let Some(find) = layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Find(find)) => Some(find),
            _ => None,
        }) {
            find.recompute(entries.as_slice(), modes, width);
        }
        self.find_jump();
    }

    fn find_push(&mut self, c: char) {
        if let Some(find) = self.find_mut() {
            find.query.push(c);
        }
        self.find_recompute();
    }

    fn find_backspace(&mut self) {
        if let Some(find) = self.find_mut() {
            find.query.pop();
        }
        self.find_recompute();
    }

    /// Moves a selection's anchors after an entry was inserted at `from` (the tool result that
    /// belongs right after its call, rule 4): the anchors name entries by index, so an
    /// insertion must carry them along or a selection would point at the wrong entry.
    fn shift_anchors(&mut self, from: usize) {
        if let Some(selection) = &mut self.selection {
            for anchor in [&mut selection.anchor, &mut selection.head] {
                if anchor.entry >= from {
                    anchor.entry += 1;
                }
            }
        }
    }

    /// Drops every per-entry fold override (ADR 0028 R6): a global fold toggle moots them.
    fn clear_fold_overrides(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.fold_override = None;
        }
    }

    /// Centers the current match in the viewport and stops following the bottom. A search
    /// that matches nothing leaves the viewport alone.
    ///
    /// ADR 0028 R6: when the match is in a **folded** entry, exactly that entry opens (a
    /// per-entry override), the others stay folded, and the status bar says so. The override
    /// stays until the next global fold toggle: collapsing the hit the moment the box closes
    /// would take the text the user just searched for away again.
    fn find_jump(&mut self) {
        let Some((entry, line)) = self
            .find_state()
            .and_then(FindState::current)
            .map(|m| (m.entry, m.line))
        else {
            return;
        };
        if entry >= self.entries.len() {
            return;
        }
        let modes = self.display_modes();
        // Does this entry's display hide content? (Its count can coincide with the expanded
        // one and still hide the text - a one-line thinking channel does exactly that.)
        let folded = crate::blocks::is_folded(&self.entries[entry], modes);
        if folded {
            // Exactly one override at a time.
            for other in self.entries.iter_mut() {
                other.fold_override = None;
            }
            self.entries[entry].fold_override = Some(true);
            self.note = Some("expanded 1 folded entry for the match".to_string());
        }
        // The display line of the match: the expanded line index only maps onto the display
        // when the entries above it are laid out folded, so walk the display counts (the
        // entry itself is measured with its override, so its own lines line up).
        let modes = self.display_modes();
        let width = self.transcript_width;
        let mut display_line = 0usize;
        for item in self.entries[..entry].iter_mut() {
            display_line += crate::blocks::line_count_cached(item, modes, width);
        }
        let line = display_line + line;
        let height = usize::from(self.view_height.max(1));
        self.scroll = crate::find::centered_scroll(line, height, self.max_scroll);
        self.follow = false;
    }

    /// Shows the queue's head (ADR 0023 U7). The head is the only pending request on screen:
    /// a second request waits in the queue instead of superseding the first. A head that is
    /// already shown keeps its state, so a question's drafts survive a redraw.
    fn sync_pending(
        &mut self,
        head: Option<Box<PendingView<PendingRequest>>>,
        len: usize,
    ) -> UpdateResult {
        self.pending = len;
        let head_id = head.as_ref().map(|view| view.request_id);
        if head_id != self.last_answered {
            // The head moved on, so a request answered a moment ago is gone for good.
            self.last_answered = None;
        }
        let showing = self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Approval(prompt)) => Some(prompt.request_id),
            Layer::Modal(Modal::Question(form)) => Some(form.request_id),
            _ => None,
        });
        if showing == head_id {
            return UpdateResult::dirty();
        }
        // A different request (or none): drop the old modal and show the new head.
        self.remove_modal(|modal| matches!(modal, Modal::Approval(_) | Modal::Question(_)));
        let Some(view) = head else {
            return UpdateResult::dirty();
        };
        if Some(view.request_id) == self.last_answered {
            return UpdateResult::dirty();
        }
        match view.request {
            PendingRequest::Approval(request) => {
                self.push_modal(Modal::Approval(Box::new(PendingApproval {
                    request_id: view.request_id,
                    origin: view.origin,
                    request,
                    // A new question starts at the top of its body.
                    scroll: 0,
                    total_rows: 0,
                    visible_rows: 0,
                })))
            }
            PendingRequest::Question(request) => self.push_modal(Modal::Question(Box::new(
                QuestionForm::new(view.request_id, view.origin, request.questions),
            ))),
        }
        UpdateResult::dirty()
    }

    /// Scrolls the approval box's body (approval-fit). The window is clamped against what the
    /// last frame measured, so a key before the first draw (or past the end) moves nothing.
    fn approval_scroll(&mut self, direction: ApprovalScroll) -> UpdateResult {
        let Some(index) = self
            .layers
            .iter()
            .rposition(|layer| matches!(layer, Layer::Modal(Modal::Approval(_))))
        else {
            return UpdateResult::none();
        };
        let Layer::Modal(Modal::Approval(prompt)) = &mut self.layers[index] else {
            return UpdateResult::none();
        };
        let visible = prompt.visible_rows.max(1);
        let max = prompt.total_rows.saturating_sub(visible);
        prompt.scroll = match direction {
            ApprovalScroll::LineUp => prompt.scroll.saturating_sub(1),
            ApprovalScroll::LineDown => (prompt.scroll + 1).min(max),
            ApprovalScroll::PageUp => prompt.scroll.saturating_sub(visible),
            ApprovalScroll::PageDown => (prompt.scroll + visible).min(max),
        };
        UpdateResult::dirty()
    }

    /// Answers the pending approval and clears it. A no-op when nothing is pending.
    fn answer_approval(&mut self, outcome: ApprovalOutcome) -> UpdateResult {
        match self.remove_modal(|modal| matches!(modal, Modal::Approval(_))) {
            Some(Modal::Approval(prompt)) => {
                self.last_answered = Some(prompt.request_id);
                UpdateResult::quiet(Effect::AnswerRequest {
                    request_id: prompt.request_id,
                    response: PendingResponse::Approval(outcome),
                })
            }
            _ => UpdateResult::none(),
        }
    }

    /// The approval box's `!` key (ADR 0026 D9): approve the pending request once **and**
    /// turn auto on for the rest of the session, in one press, no confirmation (owner's
    /// correction 2026-10-13: `!` had no meaning before, so it cannot be mistyped into one).
    /// The effects run in order: the answer first, then the switch. The switch stands even
    /// with nothing pending — it is the same switch `/auto on` makes.
    fn approval_auto_key(&mut self) -> UpdateResult {
        let mut effects = Vec::new();
        if let Some(Modal::Approval(prompt)) =
            self.remove_modal(|modal| matches!(modal, Modal::Approval(_)))
        {
            self.last_answered = Some(prompt.request_id);
            effects.push(Effect::AnswerRequest {
                request_id: prompt.request_id,
                response: PendingResponse::Approval(ApprovalOutcome::AllowOnce),
            });
        }
        self.auto = true;
        self.push_notice(
            "auto: tool calls are approved without asking for the rest of this session; \
             deny rules and the sandbox still apply",
        );
        effects.push(Effect::SetApprovalPolicy { on: true });
        UpdateResult {
            dirty: true,
            effects,
        }
    }

    /// One key inside the question form (ADR 0024 Q7).
    fn form_key(&mut self, key: FormKey) -> UpdateResult {
        let Some(index) = self
            .layers
            .iter()
            .rposition(|layer| matches!(layer, Layer::Modal(Modal::Question(_))))
        else {
            return UpdateResult::none();
        };
        let outcome = match &mut self.layers[index] {
            Layer::Modal(Modal::Question(form)) => form.key(key),
            _ => return UpdateResult::none(),
        };
        match outcome {
            FormOutcome::Stay => UpdateResult::dirty(),
            FormOutcome::Blocked(id) => {
                self.push_notice(format!(
                    "`{id}` is required: answer it before submitting (Tab to it)"
                ));
                UpdateResult::dirty()
            }
            FormOutcome::Answer(answer) => {
                let request_id = self.question_request_id(index);
                self.remove_modal(|modal| matches!(modal, Modal::Question(_)));
                self.last_answered = request_id;
                UpdateResult::quiet(Effect::AnswerRequest {
                    request_id: request_id.unwrap_or_default(),
                    response: PendingResponse::Question(Ok(answer)),
                })
            }
            FormOutcome::Decline => {
                let request_id = self.question_request_id(index);
                self.remove_modal(|modal| matches!(modal, Modal::Question(_)));
                self.last_answered = request_id;
                UpdateResult::quiet(Effect::AnswerRequest {
                    request_id: request_id.unwrap_or_default(),
                    response: PendingResponse::Question(Err(PromptError::Declined)),
                })
            }
        }
    }

    fn question_request_id(&self, index: usize) -> Option<u64> {
        match self.layers.get(index) {
            Some(Layer::Modal(Modal::Question(form))) => Some(form.request_id),
            _ => None,
        }
    }

    /// The single state-change entry point (ADR 0023 U3). Pure: no I/O, no runtime, no
    /// service. Anything that needs the outside world comes back as an [`Effect`].
    ///
    /// The `/…` completion follows the input text, so it is recomputed here whenever an input
    /// changed the line (one place, instead of every editing arm).
    pub fn update(&mut self, input: Input) -> UpdateResult {
        let before = self.input.text();
        let mut result = self.update_inner(input);
        if self.input.text() != before {
            result.effects.extend(self.refresh_slash());
            result.dirty = true;
        }
        result
    }

    fn update_inner(&mut self, input: Input) -> UpdateResult {
        // A pending quit is disarmed by user input only: a resize,
        // a stream/runtime fact or an asynchronous result is not the user changing their mind,
        // and letting one disarm the window made the second Ctrl-C look like it did nothing.
        // The confirming Ctrl-C itself and the deadline input are excluded here because this
        // match handles them.
        let disarmed = self.quit_pending.is_some()
            && is_user_input(&input)
            && !matches!(input, Input::Cancel { .. });
        if disarmed {
            self.quit_pending = None;
        }
        let mut result = match input {
            // The two clock-aware inputs the loop (or a test) sends.
            Input::Cancel { now } => self.cancel_or_quit_at(now),
            Input::QuitDeadline => {
                // The armed window closed: clear it here (the disarm rule below excludes this
                // input, because this *is* the disarming event).
                self.quit_pending = None;
                UpdateResult::dirty()
            }
            Input::Action(action) => {
                // A completion menu is a selectable list: while it is up,
                // Up/Down (and Ctrl-N) move the highlight and wrap, Enter and Tab accept the
                // highlighted row (Enter never sends), and Esc closes the menu keeping the
                // input. Every other action dismisses the menu and proceeds as usual.
                if self.menu_open() {
                    match action {
                        Action::HistoryPrev => {
                            self.move_menu(-1);
                            return UpdateResult::dirty();
                        }
                        Action::HistoryNext | Action::MenuNext => {
                            self.move_menu(1);
                            return UpdateResult::dirty();
                        }
                        Action::Escape => {
                            self.menu_close();
                            return UpdateResult::dirty();
                        }
                        Action::Complete | Action::Send => {
                            return self.accept_menu_candidate();
                        }
                        _ => self.menu_close(),
                    }
                }
                self.update_action(action)
            }
            Input::Char(c) => {
                self.completion_clear();
                if self.find_state().is_some() {
                    self.find_push(c);
                } else {
                    self.input.insert_char(c);
                }
                UpdateResult::dirty()
            }
            Input::FindBackspace => {
                self.completion_clear();
                self.find_backspace();
                UpdateResult::dirty()
            }
            // A bracketed paste is text for the message box — unless the `/login` wizard is
            // up: its fields take the paste (masked like typing, D3), and the message box is
            // frozen while a modal is up anyway.
            Input::Paste(text) => {
                if matches!(self.top_modal(), Some(Modal::Login(_))) {
                    let outcome = self
                        .login_modal_mut()
                        .map(|wizard| wizard.on_key(LoginKey::Paste(text)));
                    return self.login_outcome(outcome.unwrap_or(LoginOutcome::Stay));
                }
                self.input.insert_str(&text);
                UpdateResult::dirty()
            }
            Input::Resize { .. } => UpdateResult::dirty(),
            Input::ApprovalAnswer(outcome) => self.answer_approval(outcome),
            Input::ApprovalAuto => self.approval_auto_key(),
            Input::ApprovalScroll(direction) => self.approval_scroll(direction),
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
            Input::Pending(head, len) => self.sync_pending(head, len),
            Input::Form(key) => self.form_key(key),
            Input::MentionsExpanded(result) => self.mentions_expanded(result),
            Input::CompletionReady(completion) => self.completion_ready(completion),
            Input::CompletionsReady { fetch, candidates } => {
                self.completions_ready(fetch, candidates)
            }
            Input::MemoryCommandDone(result) => self.memory_command_done(result),
            Input::CompactDone(result) => self.compact_done(result),
            Input::BashDone {
                command,
                exclude_from_context,
                result,
            } => self.bash_done(command, exclude_from_context, result),
            Input::ThinkingChecked {
                requested,
                applied,
                supported,
            } => self.thinking_checked(requested, applied, supported),
            Input::ModelChecked {
                provider,
                model,
                tokens,
                budget,
            } => self.model_checked(provider, model, tokens, budget),
            Input::SwitchCompacted {
                provider,
                model,
                result,
            } => self.switch_compacted(provider, model, result),
            Input::SessionDone(notices) => {
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
            Input::TrustMove(delta) => {
                if let Some(Modal::Trust(wizard)) =
                    self.layers.iter_mut().rev().find_map(|layer| match layer {
                        Layer::Modal(modal) => Some(modal),
                        _ => None,
                    })
                {
                    wizard.move_cursor(delta);
                }
                UpdateResult::dirty()
            }
            Input::TrustChosen(choice) => {
                self.remove_modal(|modal| matches!(modal, Modal::Trust(_)));
                UpdateResult::effect(Effect::SetTrust { choice })
            }
            Input::TrustResolved(setup) => {
                self.push_notice(radian_host::trust::status_line(&setup));
                UpdateResult::dirty()
            }
            Input::SkillExpanded { result, steer } => self.skill_expanded(result, steer),
            Input::CommandRefused(refused) => {
                self.push_notice(format!(
                    "the session refused `{}`: {}",
                    refused.command, refused.reason
                ));
                UpdateResult::dirty()
            }
            Input::PickerKey(key) => self.picker_key(key),
            Input::PickerClose => {
                self.picker_close();
                UpdateResult::dirty()
            }
            Input::PickerOpened(data) => self.picker_opened(data),
            // The `/login` wizard's keys, its verification result, and the save/removal
            // results (ADR 0026 D3).
            Input::LoginKey(key) => {
                let outcome = self
                    .login_modal_mut()
                    .map(|wizard| wizard.on_key(key))
                    .unwrap_or(LoginOutcome::Stay);
                self.login_outcome(outcome)
            }
            Input::LoginVerified { name: _, result } => {
                // Only a wizard still waiting consumes the answer (D3).
                let waiting = matches!(
                    self.top_modal(),
                    Some(Modal::Login(wizard))
                        if matches!(wizard.step, crate::login::LoginStep::Verifying)
                );
                if !waiting {
                    return UpdateResult::none();
                }
                let outcome = self
                    .login_modal_mut()
                    .and_then(|wizard| wizard.verified(&result))
                    .unwrap_or(LoginOutcome::Stay);
                self.login_outcome(outcome)
            }
            Input::LoginSaved { name, result } => {
                self.remove_modal(|modal| matches!(modal, Modal::Login(_)));
                match result {
                    Ok(()) => {
                        // D3's required "what next" line: how to actually use the provider.
                        self.push_notice(format!(
                            "saved the key for `{name}` in credentials/auth.json; switch to it \
                             with /model {name}/<model> (or set model.provider)"
                        ));
                    }
                    Err(error) => self.push_notice(format!("could not save the key: {error}")),
                }
                UpdateResult::dirty()
            }
            Input::ConfirmLogout(confirmed) => {
                let Some(provider) = self.pending_logout().map(str::to_owned) else {
                    return UpdateResult::none();
                };
                self.remove_modal(|modal| matches!(modal, Modal::ConfirmLogout(_)));
                if confirmed {
                    UpdateResult::effect(Effect::Logout { provider })
                } else {
                    self.push_notice(format!("kept `{provider}`"));
                    UpdateResult::dirty()
                }
            }
            Input::LogoutDone { provider, result } => {
                match result {
                    Ok(()) => {
                        self.push_notice(format!("removed the stored credential for `{provider}`"))
                    }
                    Err(error) => {
                        self.push_notice(format!("could not remove `{provider}`: {error}"))
                    }
                }
                UpdateResult::dirty()
            }
            Input::MouseDown { row, col } => self.mouse_down(row, col),
            Input::MouseDrag { row, col } => self.mouse_drag(row, col),
            Input::MouseUp { row, col } => self.mouse_up(row, col),
            // The loop's clipboard write finished; its notice says what Radian did ("copied N
            // chars (via OSC 52)"), never that the clipboard holds it — OSC 52 has no receipt.
            Input::ClipboardDone(notice) => {
                self.note = Some(notice);
                UpdateResult::dirty()
            }
            Input::ThemeReady { theme, notices } => {
                if let Some(theme) = theme {
                    self.theme = *theme;
                }
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
        };
        if disarmed {
            result.dirty = true;
        }
        result
    }

    /// Resolves one key event to an [`Input`] through the keymap and the layer stack
    /// (ADR 0023 U4).
    ///
    /// Order: (2) a modal's hardcoded keys (`y`/`a`/`n`/`Esc`) win over the keymap, even over a
    /// `cancel` rebound onto them; then (1) the bound `cancel` action runs before the stack; then
    /// the stack is walked top-down, the first layer that consumes the key winning and an
    /// unconsumed key falling toward the [`Layer::Surface`].
    pub fn key_input(&mut self, key: &KeyEvent) -> Option<Input> {
        // Invariant 2 (ADR 0023 U4): a modal's own keys are hardcoded, not read from the keymap.
        if self.has_modal()
            && let Some(input) = self.modal_hardcoded_key(key)
        {
            return Some(input);
        }
        // Invariant 1 (ADR 0023 U4): the `cancel` action runs before the whole stack. Its
        // default key is Ctrl-C; a rebind moves it, and it still wins over every overlay.
        let action = self.keymap.action_for(key);
        if action == Some(Action::Cancel) {
            return Some(Input::Action(Action::Cancel));
        }
        // The question form is the one modal whose keys are rebindable (ADR 0024 Q7): its own
        // actions come from the `form` layer, and everything else that can edit text goes to
        // the focused field. A key the form does not want is swallowed, never passed down.
        if matches!(self.top_modal(), Some(Modal::Question(_))) {
            // `alt-h` collapses the panel (ADR 0024 R3): it is the form's own key,
            // not a rebindable `form` action.
            if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::ALT) {
                return Some(Input::Form(FormKey::Hide));
            }
            if let Some(action) = self.keymap.form_action_for(key) {
                return Some(Input::Form(FormKey::Action(action)));
            }
            if let KeyCode::Char(c) = key.code
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                return Some(Input::Form(FormKey::Char(c)));
            }
            if let Some(action) = self.keymap.action_for(key) {
                return Some(Input::Form(FormKey::Action(action)));
            }
            return None;
        }

        /// What a layer did with one key.
        enum Step {
            /// The layer turned it into an input.
            Input(Input),
            /// The layer took the key and did nothing with it; it does not fall through.
            Swallow,
            /// The layer did not want the key; keep walking down.
            Pass,
            /// An unfocused picker: typing dismisses it; keep walking down.
            DropPicker,
        }

        let mut index = self.layers.len();
        while index > 0 {
            index -= 1;
            let step = match &self.layers[index] {
                // Its hardcoded keys and `cancel` were handled above; every other key stops
                // here rather than reaching the transcript.
                Layer::Modal(_) => Step::Swallow,
                Layer::Overlay(Overlay::Picker(picker)) => {
                    if picker.is_focused() {
                        Step::Input(Input::PickerKey(*key))
                    } else {
                        match key.code {
                            // Navigation focuses the picker and moves the selection.
                            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                                Step::Input(Input::PickerKey(*key))
                            }
                            KeyCode::Esc => Step::Input(Input::PickerClose),
                            // Typing dismisses the overlay and goes to the composer.
                            _ => Step::DropPicker,
                        }
                    }
                }
                // The find box is a layer over the transcript: while it is open its keys are
                // looked up first, so `enter` is the earlier match there and the send key in
                // the transcript. `ctrl-f` is both "open the box" and "earlier match"; `esc`
                // closes it.
                Layer::Overlay(Overlay::Find(_)) => {
                    if let Some(found) = self.keymap.find_action_for(key) {
                        Step::Input(Input::Action(found))
                    } else {
                        match key.code {
                            KeyCode::Backspace => Step::Input(Input::FindBackspace),
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                Step::Input(Input::Char(c))
                            }
                            // PageUp/PageDown, Home/End and the arrows keep reaching the
                            // transcript layer, so the box does not trap navigation.
                            _ => Step::Pass,
                        }
                    }
                }
                Layer::Overlay(Overlay::Completion(_)) => Step::Pass,
                Layer::Surface => {
                    if let Some(found) = action {
                        Step::Input(Input::Action(found))
                    } else if let KeyCode::Char(c) = key.code
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                    {
                        Step::Input(Input::Char(c))
                    } else {
                        Step::Pass
                    }
                }
            };
            match step {
                Step::Input(input) => return Some(input),
                Step::Swallow => return None,
                Step::Pass => continue,
                Step::DropPicker => {
                    self.layers.remove(index);
                    continue;
                }
            }
        }
        None
    }

    /// The hardcoded key of the topmost modal, if `key` is one of them (ADR 0023 U4, invariant
    /// 2). `None` means the modal does not own this key; the caller then checks `cancel` and the
    /// stack.
    fn modal_hardcoded_key(&self, key: &KeyEvent) -> Option<Input> {
        match self.top_modal()? {
            Modal::Approval(_) => match key.code {
                // The auto key `!` (ADR 0026 D9, the constant in `render.rs`): approve this
                // request once and turn auto on for the session. It must not take a scroll
                // key, and it is un-rebindable like every answer key (ADR 0023 U4).
                KeyCode::Char('!') => Some(Input::ApprovalAuto),
                // `Y` is `y` (allow once); it never switches auto on (correction
                // 2026-10-13: a Caps-Lock `y` must not silently enable auto).
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Some(Input::ApprovalAnswer(ApprovalOutcome::AllowOnce))
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Some(Input::ApprovalAnswer(ApprovalOutcome::AllowForSession))
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Input::ApprovalAnswer(ApprovalOutcome::Deny {
                        reason: None,
                    }))
                }
                // The body's scroll keys (approval-fit). They are not answer keys, so they
                // cannot be confused with one, and the answers stay un-rebindable. The auto
                // key `!` (ADR 0026 D9) shares this property: it is no scroll key either.
                KeyCode::Up => Some(Input::ApprovalScroll(ApprovalScroll::LineUp)),
                KeyCode::Down => Some(Input::ApprovalScroll(ApprovalScroll::LineDown)),
                KeyCode::PageUp => Some(Input::ApprovalScroll(ApprovalScroll::PageUp)),
                KeyCode::PageDown => Some(Input::ApprovalScroll(ApprovalScroll::PageDown)),
                _ => None,
            },
            // The question form's keys are the rebindable `form` layer (ADR 0024 Q7), not
            // hardcoded: `key_input` looks them up when this modal is on top.
            Modal::Question(_) => None,
            Modal::ConfirmCommand(_) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Input::ConfirmCommand(true)),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Input::ConfirmCommand(false))
                }
                _ => None,
            },
            // The `/logout` confirmation is hardcoded like the `/memory` one (ADR 0023 U4).
            Modal::ConfirmLogout(_) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Input::ConfirmLogout(true)),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Input::ConfirmLogout(false))
                }
                _ => None,
            },
            // The startup trust prompt's keys are hardcoded (ADR 0031 T3): a startup decision
            // must not depend on the keymap, and Enter takes the fail-safe default.
            Modal::Trust(wizard) => match key.code {
                KeyCode::Up => Some(Input::TrustMove(-1)),
                KeyCode::Down => Some(Input::TrustMove(1)),
                KeyCode::Enter => Some(Input::TrustChosen(wizard.selected())),
                // Esc is "do not trust, this session only": it must not
                // write `trust.json`, so the next start asks again. Enter takes the
                // highlighted default, which is the persistent "do not trust this directory".
                KeyCode::Esc => Some(Input::TrustChosen(
                    radian_host::trust::Choice::UntrustSession,
                )),
                _ => None,
            },
            // The `/login` wizard's keys are hardcoded too: a login prompt is sensitive, and
            // the key field must behave identically whoever rebinds what (ADR 0026 D3).
            Modal::Login(_) => match key.code {
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    Some(Input::LoginKey(LoginKey::Char(c)))
                }
                KeyCode::Backspace => Some(Input::LoginKey(LoginKey::Backspace)),
                KeyCode::Enter => Some(Input::LoginKey(LoginKey::Enter)),
                KeyCode::Esc => Some(Input::LoginKey(LoginKey::Esc)),
                KeyCode::Up => Some(Input::LoginKey(LoginKey::Up)),
                KeyCode::Down => Some(Input::LoginKey(LoginKey::Down)),
                _ => None,
            },
        }
    }

    /// Handles one key for the picker overlay. An in-session picker opens unfocused (the
    /// composer keeps the keys); the first navigation focuses it. Selecting does not switch
    /// sessions in place (ADR 0022/0023): it shows the `--resume <id>` line to copy.
    fn picker_key(&mut self, key: KeyEvent) -> UpdateResult {
        if let Some(picker) = self.picker_mut()
            && !picker.is_focused()
        {
            picker.focus();
        }
        let outcome = self.picker_mut().and_then(|picker| picker.on_key(key));
        let Some(outcome) = outcome else {
            return UpdateResult::dirty();
        };
        self.picker_close();
        if self.startup == Startup::Picking {
            // The pre-session phase: hand the choice to the loop, which leaves and lets the
            // caller open the session in the same terminal. `Quit` also sets `quit` so the
            // loop leaves even if the caller does not read the outcome.
            if outcome == PickerOutcome::Quit {
                self.quit = true;
            }
            self.picker_outcome = Some(outcome);
            return UpdateResult::dirty();
        }
        match outcome {
            // In-session the picker's `Esc` closes the overlay, it does not quit the app.
            PickerOutcome::Quit => UpdateResult::dirty(),
            PickerOutcome::New => {
                self.push_notice(
                    "a new session starts on the next launch (run `radian` with no `--resume`)",
                );
                UpdateResult::dirty()
            }
            PickerOutcome::Resume(id) => {
                self.push_notice(format!(
                    "--resume {id} (this process does not switch sessions in place)"
                ));
                UpdateResult::dirty()
            }
        }
    }

    /// Pushes the picker overlay from the loop's loaded rows. An in-session overlay opens
    /// unfocused, so it informs without stealing the composer (ADR 0023 U4).
    fn picker_opened(&mut self, data: PickerData) -> UpdateResult {
        let mut picker =
            Picker::new(data.rows, data.index, data.workspace).with_secrets(data.secrets);
        if let Some(query) = data.query {
            picker = picker.with_search(query);
        }
        self.push_overlay(Overlay::Picker(picker));
        UpdateResult::dirty()
    }

    /// The terminal's entry point: resolve the key, then [`App::update`]. A thin wrapper —
    /// everything that changes state lives in `update`.
    pub fn on_key(&mut self, key: KeyEvent) -> UpdateResult {
        self.on_key_at(key, Instant::now())
    }

    /// Like [`App::on_key`], with the wall clock supplied: only the Ctrl-C quit window
    /// depends on it, so production passes the real clock and a test injects its own.
    pub fn on_key_at(&mut self, key: KeyEvent, now: Instant) -> UpdateResult {
        match self.key_input(&key) {
            // The cancel action (`Ctrl-C` by default) carries its timestamp: idle with an
            // empty input it arms the quit window instead of quitting at once.
            Some(Input::Action(Action::Cancel)) => self.update(Input::Cancel { now }),
            Some(input) => self.update(input),
            None => UpdateResult::none(),
        }
    }

    /// The current turn state, read from the projection (ADR 0023 U5). `update` reads it to
    /// decide between cancel and quit, and between a steer and a follow-up; it never writes it.
    fn turn(&self) -> TurnState {
        self.projection.read().turn()
    }

    /// `Esc`'s escalation (unchanged, ): cancel a running turn, otherwise
    /// quit at once. Ctrl-C ([`Input::Cancel`]) is the one that asks twice.
    fn cancel_or_quit_now(&mut self) -> UpdateResult {
        if self.turn() == TurnState::Running {
            self.quit_pending = None;
            UpdateResult::quiet(Effect::Cancel)
        } else {
            self.quit = true;
            UpdateResult::quiet(Effect::Quit)
        }
    }

    /// Ctrl-C (the `cancel` action), the one action whose meaning depends on the clock
    ///:
    /// - a turn is running: cancel it (unchanged);
    /// - idle, the input has text: clear it into the history (the same clearing `Esc` does,
    ///   so `Up` finds it);
    /// - idle, the input is empty: the first press arms [`QUIT_WINDOW`]; a second press
    ///   inside the window quits; a press after it re-arms.
    fn cancel_or_quit_at(&mut self, now: Instant) -> UpdateResult {
        if self.turn() == TurnState::Running {
            self.quit_pending = None;
            return UpdateResult::quiet(Effect::Cancel);
        }
        if !self.input.is_empty() {
            let text = std::mem::take(&mut self.input).text();
            self.history.push(text);
            self.history_cursor = None;
            self.history_stash.clear();
            self.quit_pending = None;
            return UpdateResult::dirty();
        }
        match self.quit_pending.take() {
            Some(deadline) if now <= deadline => {
                self.quit = true;
                UpdateResult::quiet(Effect::Quit)
            }
            // The first press, or a second one after the window closed (re-arm, do not
            // double-fire): arm the deadline and redraw the hint.
            _ => {
                self.quit_pending = Some(now + QUIT_WINDOW);
                UpdateResult::dirty()
            }
        }
    }

    /// Runs one keymap action. The default bindings reproduce the pre-keymap key table.
    fn update_action(&mut self, action: Action) -> UpdateResult {
        match action {
            Action::Send => {
                // A completion list on screen means Enter picks the candidate instead of
                // sending: it completes the line and never executes.
                if self.slash_completion.is_some() {
                    return self.accept_slash_candidate();
                }
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
                // A `/…` line is a command: it never reaches the model or the journal. A `!`
                // line is the user's shell command (ADR 0033), routed before `/` so `!/bin/sh`
                // is a command and not the unknown `/bin/sh`.
                if trimmed.starts_with('/') || trimmed.starts_with('!') {
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
                // A `/…` line completes through the command tree (every level, one fuzzy
                // match); anything else completes `@` file references.
                if self.input.text().starts_with('/') {
                    return self.accept_slash_candidate();
                }
                if self.workspace.is_none() {
                    self.completion_clear();
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
                if self.find_state().is_some() {
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
            // ADR 0028 R3: Ctrl-T folds or unfolds the thinking blocks. The mode is part of
            // the block/layout cache keys, so the next frame re-lays them out.
            Action::ToggleReasoning => {
                self.reasoning_mode = self.reasoning_mode.toggled();
                // A global fold toggle drops the per-entry search overrides (ADR 0028 R6).
                self.clear_fold_overrides();
                UpdateResult::dirty()
            }
            // ADR 0028 R4: Ctrl-O steps the tool bodies through errors -> expanded ->
            // collapsed -> errors, pi's three modes in one key.
            Action::ToggleToolOutput => {
                self.tool_display.fold = match self.tool_display.fold {
                    crate::tools::ToolFold::Errors => crate::tools::ToolFold::Expanded,
                    crate::tools::ToolFold::Expanded => crate::tools::ToolFold::Collapsed,
                    crate::tools::ToolFold::Collapsed => crate::tools::ToolFold::Errors,
                };
                self.clear_fold_overrides();
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
                } else if self.bash_running.is_some() {
                    // Esc cancels the running `!` command (ADR 0033); the task reports back
                    // as a cancelled `BashDone`.
                    self.push_notice("cancelling the command…");
                    UpdateResult::effect(Effect::CancelBash)
                } else {
                    self.cancel_or_quit_now()
                }
            }
            // The form actions normally arrive as `Input::Form`; a main-layer binding to one
            // would still land here, so route it to the form rather than dropping it.
            Action::FormNext
            | Action::FormPrev
            | Action::FormUp
            | Action::FormDown
            | Action::FormSelect
            | Action::FormSubmit
            | Action::FormDecline => self.form_key(FormKey::Action(action)),
            Action::Cancel => self.cancel_or_quit_at(Instant::now()),
            // `Ctrl-P` folds the startup information.
            Action::ToggleStartup => self.toggle_startup(),
            // Inert without a menu; `update` intercepts it while one is open.
            Action::MenuNext => UpdateResult::none(),
        }
    }

    /// Starts a send. The `@` references are expanded by the loop first (an effect), and the
    /// result becomes the follow-up or steer effect. Whether it steers is decided here, because
    /// the turn state is app state.
    fn begin_send(&mut self, text: String) -> UpdateResult {
        let kind = if self.turn() == TurnState::Running {
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
        let Some(Modal::ConfirmCommand(pending)) =
            self.remove_modal(|modal| matches!(modal, Modal::ConfirmCommand(_)))
        else {
            return UpdateResult::none();
        };
        // A `/model` confirm (D7): y compacts first, n cancels the switch. The modal's y/n are
        // hardcoded (ADR 0023 U4), exactly like the approval's.
        if let Some((provider, model, budget)) = pending.switch {
            if !confirmed {
                self.push_notice("cancelled; the model is unchanged");
                return UpdateResult::dirty();
            }
            return UpdateResult::effect(Effect::CompactForSwitch {
                provider,
                model,
                budget,
            });
        }
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
        if matches!(event, RuntimeEvent::ConfigChanged { .. }) {
            // The driver applied the queued change: the status bar's `next:` promise is done.
            self.next_model = None;
            self.next_thinking = None;
        }
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

    /// A `/skill:<name>` expansion finished (ADR 0025 S3). On success the message is sent like
    /// a typed one (steer when a turn was running); on failure the notice is shown and nothing
    /// is sent — an unknown skill is not a message.
    fn skill_expanded(&mut self, result: Result<String, String>, steer: bool) -> UpdateResult {
        match result {
            Ok(text) => {
                let message = Message::user(text);
                UpdateResult::effect(if steer {
                    Effect::Steer(message)
                } else {
                    Effect::SendFollowup(message)
                })
            }
            Err(error) => {
                self.push_notice(error);
                UpdateResult::dirty()
            }
        }
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
                let attached = expansion.attached();
                if !attached.is_empty() {
                    self.entries.push(Entry::new(
                        EntryKind::Notice,
                        format!(
                            "attached {} file(s): {}",
                            attached.len(),
                            attached.join(", ")
                        ),
                    ));
                }
                // ADR 0018: the message carries the text and each attachment as
                // separate parts, so Core can keep the user's words verbatim.
                let message = crate::mention::user_message(&expansion.text, &expansion.attachments);
                let effect = match pending.kind {
                    PendingSend::Followup => Effect::SendFollowup(message),
                    PendingSend::Steer => Effect::Steer(message),
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
            _ => self.completion_clear(),
        }
        UpdateResult::dirty()
    }

    /// Recomputes the `/…` completion for the current input. Pure: a level that needs the host
    /// returns the effect that fetches its candidates.
    fn refresh_slash(&mut self) -> Vec<Effect> {
        if !self.input.text().starts_with('/') {
            self.slash_completion = None;
            self.pending_fetch = None;
            return Vec::new();
        }
        match command::complete_in(command::tree(), &self.input.text(), self.input.cursor()) {
            Some(completion) => match completion.level_result {
                command::Level::Ready(candidates) => {
                    self.pending_fetch = None;
                    self.set_slash(completion.range, candidates);
                    Vec::new()
                }
                command::Level::Needs(fetch) => {
                    // The candidates need the host: ask for them and keep no list until they
                    // arrive (a list from the previous level would be misleading).
                    self.slash_completion = None;
                    self.completion_clear();
                    self.pending_fetch = Some(fetch.clone());
                    vec![Effect::FetchCompletions(fetch)]
                }
                command::Level::Free => {
                    self.slash_completion = None;
                    self.completion_clear();
                    self.pending_fetch = None;
                    Vec::new()
                }
            },
            None => {
                self.slash_completion = None;
                self.completion_clear();
                self.pending_fetch = None;
                Vec::new()
            }
        }
    }

    /// Shows the `/…` candidates: the overlay rows, and the state Tab/Enter accepts from.
    fn set_slash(&mut self, range: std::ops::Range<usize>, candidates: Vec<command::Candidate>) {
        if candidates.is_empty() {
            // Nothing to offer at this level: an empty list must not stay "open", or the next
            // Enter would be swallowed accepting a candidate that does not exist (and the
            // line could never be submitted).
            self.slash_completion = None;
            self.completion_clear();
            return;
        }
        self.completion_set(candidates.clone());
        self.slash_completion = Some(SlashCompletion { range, candidates });
    }

    /// Accepts the best `/…` candidate: it completes the line and never executes. The text
    /// change recomputes the next level (an effect when that level needs the host).
    fn accept_slash_candidate(&mut self) -> UpdateResult {
        let Some(pending) = self.slash_completion.take() else {
            return UpdateResult::none();
        };
        // The highlighted row (the first by default); the cursor is clamped because the list
        // can shrink between a key and its handling.
        let index = self.completion_cursor.min(pending.candidates.len() - 1);
        let Some(candidate) = pending.candidates.get(index) else {
            return UpdateResult::none();
        };
        // A trailing space, so the next level can be typed (or completed) right away.
        let insert = format!("{} ", candidate.insert);
        self.replace_range(pending.range.clone(), &insert);
        self.completion_clear();
        // The next level's list is recomputed by `update` (it sees the text change), so this
        // returns no effect of its own.
        UpdateResult::dirty()
    }

    /// The host answered [`Effect::FetchCompletions`]: show the candidates, unless the line has
    /// moved on since (then the answer is stale and is dropped).
    fn completions_ready(
        &mut self,
        fetch: command::Fetch,
        candidates: Vec<command::Candidate>,
    ) -> UpdateResult {
        if self.pending_fetch.as_ref() != Some(&fetch) {
            return UpdateResult::none();
        }
        self.pending_fetch = None;
        let Some(completion) =
            command::complete_in(command::tree(), &self.input.text(), self.input.cursor())
        else {
            return UpdateResult::none();
        };
        if completion.level_result != command::Level::Needs(fetch.clone()) {
            return UpdateResult::none();
        }
        let candidates = command::rank(candidates, fetch.partial());
        // The `Ready` levels' rule holds here too: the one candidate that is already exactly
        // what was typed is not a completion to accept — arming it would make Enter accept
        // (inserting the same word) instead of submitting the finished line.
        if candidates.len() == 1 && candidates[0].insert == fetch.partial() {
            self.completion_clear();
            return UpdateResult::dirty();
        }
        self.set_slash(completion.range, candidates);
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
                self.push_modal(Modal::ConfirmCommand(PendingCommand {
                    command,
                    prompt,
                    switch: None,
                }));
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
        !self.has_modal()
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

    /// Applies the wizard's outcome: stay, or hand the loop an effect / close. The "what
    /// next" line on success is the loop's (it knows the save succeeded).
    fn login_outcome(&mut self, outcome: LoginOutcome) -> UpdateResult {
        match outcome {
            LoginOutcome::Stay => {}
            LoginOutcome::Cancel => {
                self.remove_modal(|modal| matches!(modal, Modal::Login(_)));
                self.push_notice("login cancelled; nothing was saved");
            }
            LoginOutcome::Verify {
                name,
                protocol,
                base_url,
                key,
            } => {
                return UpdateResult::effect(Effect::LoginVerify {
                    name,
                    protocol,
                    base_url,
                    key,
                });
            }
            LoginOutcome::Save { name, key, env } => {
                return UpdateResult::effect(Effect::LoginSave { name, key, env });
            }
        }
        UpdateResult::dirty()
    }

    /// A `!`/`!!` command (ADR 0033): one at a time, and while a turn is running it waits for
    /// the turn to end rather than racing it. The `safe` tier has no shell at all.
    fn begin_bash(&mut self, command: String, exclude_from_context: bool) -> UpdateResult {
        if !self.bash_available {
            self.push_notice(
                "the `safe` tier has no shell (ADR 0012); `!` needs a shell-capable tier",
            );
            return UpdateResult::dirty();
        }
        if self.bash_running.is_some() {
            self.push_notice("a shell command is already running; Esc cancels it first");
            return UpdateResult::dirty();
        }
        if self.turn() == TurnState::Running {
            self.queued_bash = Some((command, exclude_from_context));
            self.push_notice("a turn is running; the command runs when it ends");
            return UpdateResult::dirty();
        }
        self.bash_running = Some(command.clone());
        UpdateResult::effect(Effect::RunBash {
            command,
            exclude_from_context,
        })
    }

    /// A `!`/`!!` command finished: show its block, and for `!` enqueue the `Ran \`cmd\``
    /// message for the next turn (never starting one, ADR 0011). The audit record is emitted
    /// for both `!` and `!!`.
    fn bash_done(
        &mut self,
        command: String,
        exclude_from_context: bool,
        result: Result<radian_coding::UserBash, String>,
    ) -> UpdateResult {
        self.bash_running = None;
        let mut effects = Vec::new();
        let (exit_code, cancelled) = match &result {
            Ok(run) => (run.exit_code, run.cancelled),
            Err(_) => (None, false),
        };
        match result {
            Ok(run) => {
                self.entries
                    .push(bash_entry(&command, exclude_from_context, &run));
                if !exclude_from_context {
                    let text = bash_context_text(&command, &run);
                    // Shown live; the same message coming back through the transcript is
                    // skipped so the block is not drawn twice.
                    self.bash_context_pending = Some(text.clone());
                    effects.push(Effect::Inject(Message::user(text)));
                }
            }
            Err(error) => self.entries.push(Entry::new(
                EntryKind::Notice,
                format!("!{command}: {error}"),
            )),
        }
        effects.push(Effect::AuditUserBash {
            command,
            exclude_from_context,
            exit_code,
            cancelled,
        });
        UpdateResult {
            dirty: true,
            effects,
        }
    }

    /// Runs one `/…` line (and a `!`/`!!` line, which shares the parse). Parsing is pure;
    /// everything that touches the world becomes an effect, and its result comes back as
    /// another [`Input`]. The line is not sent anywhere.
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
                // `/sessions` opens the picker overlay (ADR 0023 U4): listing reads the session
                // directory, so it is an effect the loop runs.
                UpdateResult::effect(Effect::OpenPicker { query })
            }
            command::Outcome::Export(request) => {
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Export(request)))
            }
            command::Outcome::Fork => {
                // ADR 0022 §1: fork only when the session is idle. The decision is app state,
                // so it is made here rather than in the loop.
                if self.turn() == TurnState::Running {
                    self.push_notice(
                        "finish the current turn before forking (`/fork` needs an idle session)",
                    );
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Fork))
            }
            command::Outcome::Theme(name) => {
                // ADR 0008 revision: the loop loads the theme (I/O) and writes `tui.theme`
                // back to the config file; the notice says which of those happened.
                UpdateResult::effect(Effect::Theme { name })
            }
            command::Outcome::Thinking(level) => {
                UpdateResult::effect(Effect::SetThinking { level })
            }
            command::Outcome::Model(switch) => UpdateResult::effect(Effect::ModelSwitch {
                provider: switch.provider,
                model: switch.model,
            }),
            // `/login`: the wizard is app state (pure); its network and file work are effects
            // the loop runs (ADR 0023 U3). An explicit `/login <id>` skips the choice step.
            command::Outcome::Login(provider) => {
                let step = match provider {
                    None => crate::login::LoginStep::ChooseProvider { cursor: 0 },
                    Some(id) if id.starts_with("no-such-provider:") => {
                        let name = id.trim_start_matches("no-such-provider:").to_owned();
                        self.push_notice(format!(
                            "unknown provider {name:?}: /login takes a built-in provider or \
                             `custom`"
                        ));
                        return UpdateResult::dirty();
                    }
                    Some(id) => {
                        // for_provider builds the matching first step (D3).
                        self.push_modal(Modal::Login(Box::new(LoginWizard::for_provider(&id))));
                        return UpdateResult::dirty();
                    }
                };
                self.push_modal(Modal::Login(Box::new(LoginWizard::new(step))));
                UpdateResult::dirty()
            }
            // `/logout <provider>`: confirm first (`y`/`n`), then remove (D3).
            command::Outcome::Logout(provider) => {
                if provider.is_empty() {
                    self.push_notice("usage: /logout <provider>");
                    return UpdateResult::dirty();
                }
                if let Some(extra) = provider.strip_prefix("__extra__") {
                    self.push_notice(format!(
                        "unexpected extra name {extra:?}; usage: /logout <provider>"
                    ));
                    return UpdateResult::dirty();
                }
                self.push_modal(Modal::ConfirmLogout(provider));
                UpdateResult::dirty()
            }
            command::Outcome::AutoToggle(on) => {
                // ADR 0026 D9: the in-session switch. `/auto on` needs no confirmation (it is
                // typed, not one keypress), and a no-op switch is still recorded — the audit
                // line says who switched, so the journal keeps every command.
                self.auto = on;
                if on {
                    self.push_notice(
                        "auto: tool calls are approved without asking for the rest of this \
                         session; deny rules and the sandbox still apply",
                    );
                } else {
                    self.push_notice("auto off: tool asks prompt again");
                }
                UpdateResult::effect(Effect::SetApprovalPolicy { on })
            }
            // `/skill:` (ADR 0025 S3): the host reads the body and the expanded text becomes a
            // user message. Whether it steers is decided here, like a typed send.
            command::Outcome::Skill { name, args } => {
                let steer = self.turn() == TurnState::Running;
                UpdateResult::effect(Effect::ExpandSkill { name, args, steer })
            }
            // `!`/`!!` (ADR 0033): executed by the loop in the sandbox. `!` also enqueues its
            // `Ran \`cmd\`` message for the model, but only once it has run.
            command::Outcome::Bash {
                command,
                exclude_from_context,
            } => self.begin_bash(command, exclude_from_context),
            // `/skills`: the host lists the discovered skills as notices.
            command::Outcome::Skills => UpdateResult::effect(Effect::ListSkills),
            command::Outcome::Jobs(request) => UpdateResult::effect(Effect::Jobs(request)),
            command::Outcome::Message => UpdateResult::none(),
        }
    }

    /// The `/model` decision (ADR 0026 D5/D7). The loop has already refused a provider without
    /// a credential; here the app decides between queueing directly and asking first.
    fn model_checked(
        &mut self,
        provider: String,
        model: String,
        tokens: Option<u64>,
        budget: Option<u64>,
    ) -> UpdateResult {
        match (tokens, budget) {
            (Some(t), Some(b)) if t > b => {
                // D7: the switch does not fit, so compaction comes first — with the user's yes.
                let prompt = format!(
                    "switching to {provider}/{model} needs a compact first: about {t} tokens, \
                     over the new model's budget {b}. compact and switch? [y] compact and switch \
                     [n] cancel"
                );
                // The payload's `command` is unused on this path: `confirm_command` routes
                // on `switch` first, so `y` can never run a memory command. `Purge` is the
                // one variant a refusal can never reach.
                let command = radian_host::memory::commands::MemoryCommand::List {
                    global: false,
                    project: false,
                    all: false,
                    json: false,
                };
                self.push_modal(Modal::ConfirmCommand(PendingCommand {
                    command,
                    prompt,
                    switch: Some((provider, model, b)),
                }));
                UpdateResult::dirty()
            }
            _ => self.queue_model(provider, model),
        }
    }

    /// Queues the route (D5) and says what happens next. The cache warning is the accepted
    /// cost of a switch (ADR 0026 已接受的代价).
    fn queue_model(&mut self, provider: String, model: String) -> UpdateResult {
        self.next_model = Some(format!("{provider}/{model}"));
        self.push_notice(format!(
            "next turn: {provider}/{model}; the first turn after a switch will not hit the cache"
        ));
        UpdateResult::effect(Effect::QueueModel { provider, model })
    }

    /// The clamped `/thinking` (D11): queue it, and say so when the clamp moved the level.
    fn thinking_checked(
        &mut self,
        requested: String,
        applied: Option<String>,
        supported: Vec<String>,
    ) -> UpdateResult {
        let Some(applied) = applied else {
            self.push_notice("this model supports no thinking level; nothing changed");
            return UpdateResult::dirty();
        };
        if applied != requested {
            self.push_notice(format!(
                "{requested} → {applied} (this model supports {})",
                supported.join("/")
            ));
        }
        self.next_thinking = Some(applied.clone());
        self.push_notice(format!("next turn: thinking {applied}"));
        UpdateResult::effect(Effect::QueueThinking { level: applied })
    }

    /// The pre-switch compaction finished (D7): only success queues the switch.
    fn switch_compacted(
        &mut self,
        provider: String,
        model: String,
        result: Result<CompactionOutcome, String>,
    ) -> UpdateResult {
        match result {
            Ok(_) => {
                self.push_notice(format!("compacted; switching to {provider}/{model}"));
                self.queue_model(provider, model)
            }
            Err(error) => {
                self.push_notice(format!(
                    "the switch was not queued: the compaction failed ({error})"
                ));
                UpdateResult::dirty()
            }
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
        let mut effects = Vec::new();
        match event {
            // ADR 0023 U5: the turn, budget, usage and tool facts are written to the session
            // projection by the event sink (`ProjectionSink`), never here. `App` handles only
            // the transcript, the note and the effects.
            AgentEvent::TurnStarted => {}
            // ADR 0027: the local turn timer starts with the model request and
            // stops with its response. The token count is the model's reported output.
            AgentEvent::ModelRequestStarted { .. } => self.timer.request_started(self.now),
            AgentEvent::ModelResponseReceived { usage, .. } => {
                self.timer
                    .response_finished(self.now, usage.as_ref().and_then(|usage| usage.output));
            }
            AgentEvent::ToolStarted { call_id, name, .. } => {
                self.running_tools.insert(call_id.clone());
                self.note = Some(format!("tool {name} running"));
            }
            AgentEvent::ToolFinished { call_id, .. } => {
                // The tool is done. Clear the note only when no other tool in the batch is
                // still running; otherwise the footer keeps naming a running one.
                self.running_tools.remove(call_id);
                if self.running_tools.is_empty() {
                    self.note = None;
                }
            }
            AgentEvent::TranscriptAppended { message, .. } => self.push_message(message),
            // ADR 0018: compaction is visible to the user as it happens.
            AgentEvent::CompactionStarted { tokens_before, .. } => {
                self.note = Some("compacting context".to_string());
                self.compacting = true;
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("context over the budget ({tokens_before} tokens); compacting…"),
                ));
            }
            AgentEvent::CompactionFinished {
                revision,
                tokens_before,
                tokens_after,
                ..
            } => {
                self.note = None;
                self.compacting = false;
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("context compacted ({tokens_before} → {tokens_after} tokens)"),
                ));
                // The summary message arrives right after this event (ADR 0018): the numbers
                // are handed to it, since the journaled message does not carry them.
                self.pending_compaction = Some((*revision, *tokens_before, *tokens_after));
            }
            AgentEvent::CompactionFailed { reason } => {
                self.note = None;
                self.compacting = false;
                self.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("compaction failed: {reason}"),
                ));
            }
            // A skip means nothing was summarized (ADR 0018): the flag must not stick.
            AgentEvent::CompactionSkipped { .. } => {
                self.compacting = false;
            }
            AgentEvent::RunFinished { status: _ } => {
                // The turn is over. A pending approval is not cleared here: the permission
                // layer raced the wait against the turn's cancellation, so its waiter is gone,
                // the queue drops the element and wakes the UI (ADR 0023 U7). Driving the modal
                // from the queue alone is what makes "the waiter gave up" and "the turn ended"
                // the same event.
                self.note = None;
                self.running_tools.clear();
                self.compacting = false;
                // ADR 0033: a `!` submitted behind the turn starts now.
                if let Some((command, exclude_from_context)) = self.queued_bash.take() {
                    effects.extend(self.begin_bash(command, exclude_from_context).effects);
                }
            }
            _ => {}
        }
        effects
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
                self.live_count += 1;
            }
        }
    }

    /// Appends a committed message as transcript entries. Used for live commits and for the
    /// history of a resumed session.
    pub fn push_message(&mut self, message: &Message) {
        // The committed message is the truth; discard the partial live view of it. The scan
        // runs **only when a live entry exists**: a resume pushes every message before any
        // stream has started, and an unconditional `retain` over a 66k-entry transcript per
        // message was O(n^2) (resume-perf A). The count is the number of live entries, so a
        // live entry is dropped wherever it sits - the stream can leave one behind a
        // compaction notice (they are not always a suffix of `entries`).
        if self.live_count > 0 {
            let mut scanned = 0u64;
            self.entries.retain(|entry| {
                scanned += 1;
                !entry.live
            });
            self.live_count = 0;
            #[cfg(test)]
            LIVE_SCANS.with(|count| count.set(count.get() + scanned));
        }
        // ADR 0033: a `!` command's `Ran \`cmd\`` context message is a Bash block, not user
        // prose. The one we already drew live is skipped so it is not shown twice; a resumed
        // one (no pending text) is drawn as a block.
        if let Message::User { .. } = message {
            let text = message.text();
            if self.bash_context_pending.as_deref() == Some(text.as_str()) {
                self.bash_context_pending = None;
                return;
            }
            if let Some(entry) = bash_entry_from_context(&text) {
                self.entries.push(entry);
                return;
            }
        }
        match message {
            Message::User { content, .. } => {
                // A compaction summary (ADR 0028 R5) is a `User` message with a
                // `compaction_summary` origin (ADR 0018); it shows as one line, not as a wall
                // of user text. Core's readers supply the summary and the verbatim user
                // words - nothing is parsed out of the body here.
                if agent_core::compaction::is_compaction_summary(message) {
                    let mut info = CompactionInfo {
                        summary: agent_core::compaction::compaction_summary_text(message)
                            .unwrap_or_default(),
                        user_messages: agent_core::compaction::compaction_user_messages(message)
                            .into_iter()
                            .map(|message| message.text)
                            .collect(),
                        ..CompactionInfo::default()
                    };
                    // The live path knows the revision and the token counts from the
                    // `CompactionFinished` event that preceded this message; a resume replay
                    // does not, and omits them.
                    if let Some((revision, before, after)) = self.pending_compaction.take() {
                        info.revision = Some(revision);
                        info.tokens_before = Some(before);
                        info.tokens_after = Some(after);
                    }
                    let mut entry = Entry::new(EntryKind::Compaction, info.display_text());
                    entry.compaction = Some(Box::new(info));
                    self.entries.push(entry);
                    return;
                }
                // ADR 0018: an attachment is its own part, so the display renders
                // it as `[file: P, N lines]` rather than joining its body into the message.
                self.entries.push(Entry::new(
                    EntryKind::User,
                    crate::mention::display_parts(content),
                ));
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
                // One card per call (ADR 0028 R4): each carries its own name and arguments,
                // and its text is the same header the renderer will show, so the head stays
                // selectable and copyable character for character.
                for call in tool_calls {
                    let card = crate::tools::ToolCard::new(
                        call.id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    );
                    // The call-time head has no result text yet (a `git_diff` head is terse
                    // until its diff arrives and the pairing refreshes it). Its status glyph
                    // is the running one (rule 4); the pairing swaps it for ✔/✘. The renderer
                    // substitutes the live spinner frame for it while the call runs (ADR 0027
                    // revision 10).
                    let [running, _, _] = self.footer.glyphs().tool_status();
                    let glyph = running.to_string();
                    let mut entry = Entry::new(
                        EntryKind::ToolCall,
                        crate::tools::call_text(&card, &glyph, ""),
                    );
                    entry.tool = Some(Box::new(card));
                    // A call with no result yet is a whole card of its own (ADR 0028 revision
                    // 2); the pairing below turns it into the head of the merged card.
                    entry.card = crate::blocks::Card::Solo;
                    self.entries.push(entry);
                }
            }
            Message::Tool { result, .. } => {
                // The result pairs with its call **by id** (the same key the live and the
                // resume paths carry), so a batch of calls pairs correctly whatever order
                // their results arrive in.
                let paired = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| {
                        entry.kind == EntryKind::ToolCall
                            && entry
                                .tool
                                .as_ref()
                                .is_some_and(|card| card.call_id == result.call_id)
                    })
                    .and_then(|entry| {
                        entry.result = Some(result.is_error);
                        let card = entry.tool.as_mut()?;
                        card.structured = result.structured.clone();
                        // The header can only be final now (the `shell` exit code comes from
                        // the result), so the entry's text - what renders and copies - is
                        // regenerated from the same function the layout uses.
                        let [_, ok, failed] = self.footer.glyphs().tool_status();
                        let glyph = if result.is_error { failed } else { ok };
                        entry.text = crate::tools::call_text(card, glyph, &result.text());
                        Some((card.name.clone(), card.arguments.clone()))
                    });
                let (name, arguments) = paired.unwrap_or_default();
                let mut card = crate::tools::ToolCard::new(result.call_id.clone(), name, arguments);
                card.structured = result.structured.clone();
                // The body's text **is** the entry's text (the mapping/search invariant): a
                // `patch` shows the synthesized diff, a `write` the content written, every
                // other tool its own result text.
                let display = crate::tools::result_display_text(&card, &result.text());
                let mut entry = Entry::new(EntryKind::ToolResult, display);
                entry.is_error = result.is_error;
                entry.error_code = result.error_code.clone();
                entry.tool = Some(Box::new(card));
                entry.card = crate::blocks::Card::Solo;
                // Rule 4: the result sits right after **its own** call, so a batch reads
                // call/result, call/result instead of every call then every result. The
                // entries are the UI's own order; the journal keeps the message order, so a
                // resume replay runs this same code and lands on the same order. A result
                // whose call is gone (a torn transcript) is appended.
                match self.entries.iter().position(|entry| {
                    entry.kind == EntryKind::ToolCall
                        && entry
                            .tool
                            .as_ref()
                            .is_some_and(|call| call.call_id == result.call_id)
                }) {
                    Some(index) => {
                        // The call heads the merged card and this result closes it (ADR 0028
                        // revision 2): the drawing merges them, the data stays two entries.
                        self.entries[index].card = crate::blocks::Card::Head;
                        entry.card = crate::blocks::Card::Tail;
                        self.entries.insert(index + 1, entry);
                        // The insertion shifts every later entry: a selection anchored to one
                        // of them moves with it.
                        self.shift_anchors(index + 1);
                    }
                    None => self.entries.push(entry),
                }
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

// Test-only: how many entries `push_message` has walked to drop live ones. A unit test
// asserts it does not grow when no live entry exists, which is the property the O(n^2) fix
// rests on (it is not wall-clock based). Thread-local so parallel tests do not interfere.
#[cfg(test)]
thread_local! {
    pub(crate) static LIVE_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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

    /// The footer's startup facts (ADR 0027 C2): the working directory, the model's display
    /// name and provider, and the applied thinking level.
    pub fn set_footer_facts(
        &mut self,
        cwd: Option<PathBuf>,
        model_display: Option<String>,
        provider: Option<String>,
        thinking_level: Option<String>,
    ) {
        self.cwd = cwd;
        self.model_display = model_display;
        self.provider = provider;
        self.thinking_level = thinking_level;
    }

    /// The background-refreshed git branch (ADR 0027 C2: the draw path never runs git). The
    /// host's `GitContributor` is the normal writer now; this setter remains for a host that
    /// has a first value without the contributor.
    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.git_branch = branch;
    }

    /// The machine's hostname (ADR 0027), read once at startup.
    pub fn set_hostname(&mut self, hostname: Option<String>) {
        self.hostname = hostname;
    }

    /// The model's catalog unit prices (ADR 0027). Written through to the
    /// projection, which is what prices each arriving call: the footer reads the accumulated
    /// fee from there, because a call keeps the price it arrived under even after a switch.
    pub fn set_model_price(&mut self, price: Option<radian_host::ModelPrice>) {
        self.model_price = price;
        self.projection.set_price(price);
    }

    /// Whether automatic compaction is on (ADR 0027).
    pub fn set_auto_compact(&mut self, on: bool) {
        self.auto_compact = on;
    }

    /// The loop's current instant (ADR 0027): the timer never reads the clock.
    pub fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    /// The spinner's current frame (ADR 0027): a function of the injected clock,
    /// so a test advances it with `set_now`.
    pub fn spinner_frame(&self) -> crate::spinner::Frame {
        let tick = self.now.saturating_duration_since(self.spinner_origin);
        crate::spinner::frame(self.spinner_kind, self.footer_glyphs(), tick)
    }

    /// The style for a spinner frame: the pulse alternates between the accent and the weak
    /// level; the rotating frames stay on the accent.
    pub fn spinner_style(&self, frame: crate::spinner::Frame) -> ratatui::style::Style {
        if frame.dim {
            self.theme.truncation
        } else {
            self.theme.notice_label
        }
    }

    /// The input rule's status text (ADR 0027): the most specific running state, or
    /// `None` when the app is idle (the rule is then plain). The states are the ones the UI
    /// already has: a modal waiting for the user, a running turn, a running `!`.
    pub fn spinner_status(&self) -> Option<&'static str> {
        if matches!(self.top_modal(), Some(Modal::Approval(_))) {
            return Some("Waiting for approval");
        }
        if matches!(self.top_modal(), Some(Modal::Question(_))) {
            return Some("Waiting for your answer");
        }
        // A running compaction is the most specific state (ADR 0018).
        if self.compacting {
            return Some("Compacting");
        }
        if self.turn_running() {
            return Some("Working");
        }
        if self.bash_running.is_some() {
            return Some("Running a command");
        }
        None
    }

    /// Whether a turn is running (the projection's own state, ADR 0023 U5). The `bar` status
    /// row shows `esc to interrupt` exactly then: Esc cancels a running turn (ADR 0009).
    pub fn turn_running(&self) -> bool {
        self.turn() == TurnState::Running
    }

    /// Whether any spinner is on screen (ADR 0027): the input rule's status line,
    /// a live reasoning line, or a running tool card. Only then does the loop animate.
    pub fn spinner_visible(&self) -> bool {
        self.spinner_status().is_some()
            || self.entries.iter().any(|entry| entry.live)
            || self
                .entries
                .iter()
                .any(|entry| entry.kind == EntryKind::ToolCall && entry.result.is_none())
    }

    /// Whether this session has a shell for `!` (ADR 0033): false on the `safe` tier, which
    /// has no sandbox at all (ADR 0012).
    pub fn set_bash_available(&mut self, available: bool) {
        self.bash_available = available;
    }

    /// The contributor store the renderer reads (ADR 0027 C3).
    pub fn set_contributions(
        &mut self,
        contributions: std::sync::Arc<radian_host::ContributionStore>,
    ) {
        self.contributions = contributions;
    }

    /// The applied thinking level, once the driver reports it (ADR 0026 D11).
    pub fn set_thinking_level(&mut self, level: Option<String>) {
        self.thinking_level = level;
    }

    /// The footer's glyph set, for the items that draw an icon.
    pub fn footer_glyphs(&self) -> crate::footer::Glyphs {
        self.footer.glyphs()
    }

    /// The brand mark (ADR 0027): `tui.brand` when set, else the glyph set's
    /// default. The footer's `brand` item and the startup banner's one-line fallback share it,
    /// so the two can never disagree.
    pub fn brand_mark(&self) -> &str {
        self.brand
            .as_deref()
            .unwrap_or_else(|| crate::footer::brand_mark(self.footer_glyphs()))
    }

    /// Applies `[tui.footer]` (ADR 0027 C2). Returns the footer's warnings (unknown item,
    /// placeholder, style), shown once at startup.
    pub fn apply_footer(&mut self, config: &radian_host::config::TuiFooterConfig) -> Vec<String> {
        let (footer, warnings) = crate::footer::Footer::from_config(config);
        self.footer = footer;
        warnings
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
            self.completion_clear();
            return;
        }
        let chars: Vec<char> = self.input.text().chars().collect();
        let typed: String = chars[completion.range.clone()].iter().collect();
        if completion.common_prefix.chars().count() > typed.chars().count() {
            self.replace_range(completion.range, &completion.common_prefix);
            self.completion_clear();
            return;
        }
        // The menu is up: keep the raw candidates so the highlighted row can be accepted.
        // `completion_set` clears the previous menu (including this field), so it is set after.
        let raw = completion.candidates.clone();
        self.completion_set(
            completion
                .candidates
                .iter()
                .map(|candidate| crate::command::Candidate {
                    insert: candidate.clone(),
                    description: String::new(),
                })
                .collect(),
        );
        self.mention_completion = Some((completion.range, raw));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027: the spinner frame follows the injected clock, not the text, and
    /// the configured kind decides the glyphs.
    #[test]
    fn the_spinner_frame_follows_the_injected_clock() {
        let mut app = App::new("m", "s");
        app.spinner_kind = crate::spinner::Kind::Arc;
        let base = app.spinner_origin;
        app.set_now(base);
        assert_eq!(app.spinner_frame().glyph, "◜");
        app.set_now(base + std::time::Duration::from_millis(250));
        assert_eq!(app.spinner_frame().glyph, "◝");
        app.set_now(base + std::time::Duration::from_millis(500));
        assert_eq!(app.spinner_frame().glyph, "◞");

        // The default pulse keeps its glyph and only changes the phase every 500 ms.
        app.spinner_kind = crate::spinner::Kind::Pulse;
        app.set_now(base);
        assert!(!app.spinner_frame().dim);
        app.set_now(base + std::time::Duration::from_millis(500));
        assert!(app.spinner_frame().dim);
    }

    /// ADR 0018 + ADR 0027: the compaction events drive the `Compacting` status.
    #[test]
    fn the_compacting_status_follows_the_compaction_events() {
        let mut app = App::new("m", "s");
        app.projection.set_turn(TurnState::Running);
        assert_eq!(app.spinner_status(), Some("Working"));

        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 1,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Budget,
        });
        assert_eq!(app.spinner_status(), Some("Compacting"));

        app.apply_event(&AgentEvent::CompactionFinished {
            revision: 1,
            tokens_before: 100,
            tokens_after: 40,
            below_target: false,
        });
        assert_eq!(app.spinner_status(), Some("Working"));

        // A failed or skipped compaction clears it too.
        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 2,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Manual,
        });
        app.apply_event(&AgentEvent::CompactionFailed {
            reason: "boom".to_string(),
        });
        assert_eq!(app.spinner_status(), Some("Working"));
        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 3,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Manual,
        });
        app.apply_event(&AgentEvent::CompactionSkipped {
            reason: "already fits".to_string(),
        });
        assert_eq!(app.spinner_status(), Some("Working"));
    }

    fn a_run(exit_code: Option<i32>, stdout: &str) -> radian_coding::UserBash {
        radian_coding::UserBash {
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_code,
            signal: None,
            timed_out: false,
            cancelled: false,
        }
    }

    /// ADR 0033: a `!` line parses to a `RunBash` effect, and the command is remembered so a
    /// second one is refused while it runs.
    #[test]
    fn a_bang_routes_to_a_run_bash_effect() {
        let mut app = App::new("m", "s");
        let result = app.run_slash_command("!echo hi");
        assert_eq!(
            result.effects,
            vec![Effect::RunBash {
                command: "echo hi".to_owned(),
                exclude_from_context: false,
            }]
        );
        assert_eq!(app.bash_running.as_deref(), Some("echo hi"));
        // A second one is refused (one at a time).
        let second = app.run_slash_command("!echo again");
        assert!(second.effects.is_empty());
        assert!(app.entries.last().unwrap().text.contains("already running"));
    }

    /// ADR 0012: the `safe` tier has no shell, so `!` never runs there.
    #[test]
    fn a_bang_on_the_safe_tier_does_not_run() {
        let mut app = App::new("m", "s");
        app.set_bash_available(false);
        let result = app.run_slash_command("!echo hi");
        assert!(result.effects.is_empty());
        assert!(app.bash_running.is_none());
        assert!(
            app.entries.last().unwrap().text.contains("safe"),
            "{}",
            app.entries.last().unwrap().text
        );
    }

    /// ADR 0033: while a turn runs, `!` waits for it to end rather than racing it.
    #[test]
    fn a_bang_queues_behind_a_running_turn() {
        let mut app = App::new("m", "s");
        app.projection.set_turn(TurnState::Running);
        let result = app.run_slash_command("!echo hi");
        assert!(result.effects.is_empty());
        assert_eq!(
            app.queued_bash,
            Some(("echo hi".to_owned(), false)),
            "queued, not run"
        );
        // The event sink writes the Idle fact before the app sees the event; the test does
        // the same.
        app.projection.set_turn(TurnState::Idle);
        let effects = app.apply_event(&AgentEvent::RunFinished {
            status: agent_core::RunStatus::Completed,
        });
        assert!(
            matches!(effects.as_slice(), [Effect::RunBash { command, .. }] if command == "echo hi"),
            "{effects:?}"
        );
        assert_eq!(app.bash_running.as_deref(), Some("echo hi"));
    }

    /// `!` shows a block, injects the `Ran \`cmd\`` message and audits; `!!` only shows and
    /// audits (ADR 0033).
    #[test]
    fn a_bang_done_injects_and_audits_and_a_bangbang_only_audits() {
        let mut app = App::new("m", "s");
        app.run_slash_command("!echo hi");
        let result = app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: false,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        assert!(
            matches!(result.effects[0], Effect::Inject(_)),
            "{:?}",
            result.effects
        );
        assert!(matches!(
            result.effects[1],
            Effect::AuditUserBash {
                exclude_from_context: false,
                exit_code: Some(0),
                ..
            }
        ));
        let entry = app.entries.last().unwrap();
        assert!(matches!(
            entry.kind,
            EntryKind::Bash {
                exclude_from_context: false
            }
        ));
        // The marker column carries the `$ `; the text is the command and output.
        assert!(entry.text.starts_with("echo hi"), "{:?}", entry.text);
        assert!(entry.text.contains("hi"), "{:?}", entry.text);

        let mut app = App::new("m", "s");
        app.run_slash_command("!!echo hi");
        let result = app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: true,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| matches!(effect, Effect::Inject(_))),
            "!! must not enter the context: {:?}",
            result.effects
        );
        assert!(matches!(
            app.entries.last().unwrap().kind,
            EntryKind::Bash {
                exclude_from_context: true
            }
        ));
    }

    /// The `!` message is shown live as a block; when it comes back through the transcript it
    /// is not drawn a second time (ADR 0033).
    #[test]
    fn the_injected_bang_message_is_not_drawn_twice() {
        let mut app = App::new("m", "s");
        app.run_slash_command("!echo hi");
        app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: false,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        let before = app.entries.len();
        let text = bash_context_text("echo hi", &a_run(Some(0), "hi\n"));
        app.push_message(&Message::user(text));
        assert_eq!(app.entries.len(), before, "no duplicate block");
    }

    /// A resumed `!` context message (no live block) renders as a Bash block, not user prose.
    #[test]
    fn a_resumed_bang_context_message_renders_as_a_block() {
        let mut app = App::new("m", "s");
        let text = bash_context_text("echo hi", &a_run(Some(0), "hi\n"));
        app.push_message(&Message::user(text));
        assert!(matches!(
            app.entries.last().unwrap().kind,
            EntryKind::Bash {
                exclude_from_context: false
            }
        ));
    }

    fn filler(count: usize) -> Vec<agent_core::Message> {
        (0..count)
            .map(|i| agent_core::Message::user(format!("message {i}")))
            .collect()
    }

    fn scans() -> u64 {
        LIVE_SCANS.with(std::cell::Cell::get)
    }

    /// resume-perf A: a resume pushes every message with no live entry, so the live-entry scan
    /// must not run. The number of entries walked is the property; no wall clock.
    #[test]
    fn push_message_does_not_walk_the_transcript_without_a_live_entry() {
        let mut app = App::new("m", "s");
        for message in filler(2_000) {
            app.push_message(&message);
        }
        assert_eq!(app.entries.len(), 2_000);
        let before = scans();
        for message in filler(1_000) {
            app.push_message(&message);
        }
        assert_eq!(
            scans(),
            before,
            "no live entry exists: push_message must not walk the transcript"
        );
    }

    /// The counter is live: with a live entry the walk does happen, and the entry is dropped.
    #[test]
    fn push_message_walks_the_transcript_to_drop_a_live_entry() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "partial");
        let before = scans();
        app.push_message(&agent_core::Message::user("committed"));
        assert!(scans() > before, "a live entry makes the walk run");
        assert!(app.entries.iter().all(|entry| !entry.live));
        assert_eq!(app.live_count, 0);
    }

    /// A live entry is not always a suffix: a compaction notice can sit after it. The drop
    /// must still find it (a count, not a trailing-only scan).
    #[test]
    fn push_message_drops_a_live_entry_that_is_not_at_the_tail() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "partial");
        app.entries.push(Entry::new(
            EntryKind::Notice,
            "context over the budget; compacting\u{2026}",
        ));
        app.push_message(&agent_core::Message::user("committed"));
        assert!(
            app.entries.iter().all(|entry| !entry.live),
            "the live entry is dropped wherever it sits"
        );
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.kind == EntryKind::Notice),
            "the non-live entry after it survives"
        );
        assert_eq!(app.live_count, 0);
    }
}

//! The transcript's data model: an [`Entry`] and its kind, a compaction summary's display data,
//! the Markdown caches an entry carries, the `!`/`!!` block builders, the startup phase, and the
//! status bar's budget/usage/MCP wording. Pure data and the functions that only format it; the
//! behavior that mutates it lives in `app.rs` and the modules beside this one.
//!
//! The entry's own data and the text helpers are the host's (`radian_host::transcript`, ADR 0023
//! U1) and are re-exported here, so `crate::app::Entry` and `crate::app::EntryKind` keep
//! naming one type shared with the folds.

use super::*;

/// The layout cache's key (ADR 0028 R1/R6): the width, the transcript area's height (ADR
/// 0028 R10: an image's run follows it), the entry's text length, the display
/// modes, the per-entry fold override, and the memory card's shape. The override is part of it,
/// so a search hit's expansion invalidates the count. So is the shape, because a settled call
/// turns a one-row head into the memory card without necessarily changing the text length
/// (ADR 0035). The last element is the entry's memory ref as `(read, line)`, `None` for every
/// ordinary entry.
pub(crate) type LayoutKey = (
    usize,
    usize,
    usize,
    crate::tools::DisplayModes,
    Option<bool>,
    Option<(bool, String)>,
);

/// A compaction summary's display data (ADR 0028 R5). The type lives in the host's folded
/// transcript (ADR 0023 U1), because a fold of a finished journal produces it too; the TUI
/// adds nothing to it.
pub use radian_host::transcript::CompactionInfo;

/// What kind of transcript line an entry is. The type is the host's (`radian_host::transcript`),
/// so the two folds and the renderer name one set of kinds (ADR 0023 U1).
pub use radian_host::transcript::EntryKind;

/// One transcript entry as the renderer sees it: the **folded entry** plus render state.
///
/// The folded part ([`Entry::data`]) is exactly what the host's folds produce
/// (`radian_host::transcript::Entry`) and what the hub sends to a remote client. Field access
/// goes through `Deref`, so the renderer, the live update path and the tests name `entry.text`,
/// `entry.kind` and `entry.tool` exactly as the folds do, and a field can never be added on one
/// side only. What is left here is state only a screen has: the live flag, the decoded images,
/// the `/context` grid's data, the selection's fold override, the reasoning clock and the layout
/// caches.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The folded entry's data: the one type both folds produce.
    pub data: radian_host::transcript::Entry,
    /// True while the entry is fed by live deltas and not yet a committed message.
    pub live: bool,
    /// The images the entry's message carried (ADR 0028 R10), decoded for drawing. A user
    /// message's placeholder line is already in `text`; a tool result's placeholder row is
    /// synthesized by the layout above the image run.
    pub images: Vec<crate::media::EntryImage>,
    /// The `/context` grid's structured data (spec `tui-context-grid`). `None` for every other
    /// entry.
    pub context: Option<crate::blocks::ContextView>,
    /// A **per-entry** fold override (ADR 0028 R6): `Some(true)` expands this entry whatever
    /// the global fold says, which is how a search hit opens exactly one folded entry.
    /// Cleared by the next global fold toggle. Part of the layout cache key.
    pub fold_override: Option<bool>,
    /// True for the wake-budget notice entry of the current degrade burst. `update` rewrites
    /// this entry (not a new line per degrade) while the burst lasts; the next turn start
    /// clears the session's index, leaving the line as plain history.
    pub wake_notice: bool,
    /// When the live reasoning channel's **first delta** arrived (ADR 0028 decision 4).
    /// Live-only: never journaled, so a resumed entry has `None` and shows no duration. The
    /// pair with [`Entry::reasoning_ended`] is the entry's own wall clock; the duration is
    /// their difference.
    pub reasoning_started: Option<Instant>,
    /// When the reasoning entry **ended**: the commit that replaced the live view (ADR 0028).
    /// Live-only, like [`Entry::reasoning_started`]; a resumed entry carries neither.
    pub reasoning_ended: Option<Instant>,
    /// The parsed blocks (ADR 0028 R1), keyed by text length and the reasoning mode. Filled for
    /// assistant prose and the thinking channel's body (ADR 0028: both carry fence grammar).
    pub(crate) block_cache: Option<((usize, ReasoningMode), Vec<Block>)>,
    /// How many lines this entry laid out to, for the width, text length and mode it was last
    /// measured at. The renderer keeps it so a frame only re-lays out entries that changed or
    /// are on screen.
    pub(crate) layout_cache: Option<(LayoutKey, usize)>,
    /// Rendered Mermaid diagrams (ADR 0028 R8), keyed by `(width, text_len)`: the width is in
    /// the key because `TooWide` can succeed on a wider terminal. Filled by the mutable measure
    /// pass; the immutable layout paths render fresh on a miss.
    pub(crate) diagram_cache: Option<(usize, usize, Vec<crate::blocks::DiagramOutcome>)>,
    /// The entry's laid-out Markdown paragraph lines (ADR 0028 R2), keyed by `(width,
    /// text_len)` and indexed by the paragraph block's start. The layout does not depend on
    /// the reasoning/tool folds, so a global fold toggle reuses it instead of re-parsing every
    /// Assistant entry (the R7 toggle gate). Filled lazily by the layout/text paths (interior
    /// mutability), because the measure pass counts Markdown without laying it out.
    pub(crate) md_cache: std::cell::RefCell<Option<MdCache>>,
    /// The Markdown paragraph **line counts**, keyed the same way (resume-perf2). The measure
    /// pass fills this from [`crate::blocks::line_count`], so the first frame (and a fold
    /// toggle) never materializes Markdown lines for an entry it only needs to measure.
    pub(crate) md_count_cache: Option<MdCountCache>,
}

impl std::ops::Deref for Entry {
    type Target = radian_host::transcript::Entry;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl std::ops::DerefMut for Entry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl Entry {
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            data: radian_host::transcript::Entry::new(kind, text),
            live: false,
            images: Vec::new(),
            context: None,
            fold_override: None,
            wake_notice: false,
            reasoning_started: None,
            reasoning_ended: None,
            block_cache: None,
            layout_cache: None,
            diagram_cache: None,
            md_cache: std::cell::RefCell::new(None),
            md_count_cache: None,
        }
    }

    /// The render entry for one entry a host fold produced (ADR 0023 U1): the folded data
    /// as-is, and the images decoded for drawing (`decode` fills `decodable` from the same
    /// cache the live path uses, so a broken payload reserves no rows).
    pub(crate) fn from_fold(
        folded: &radian_host::transcript::Entry,
        images: &mut crate::media::ImageCache,
    ) -> Self {
        let mut entry = Self::new(folded.kind, folded.text.clone());
        entry.data = folded.clone();
        entry.images = folded
            .images
            .iter()
            .map(|part| crate::media::EntryImage::from_part_facts(part, images))
            .collect();
        entry
    }
}

/// The Markdown **line** cache: `(width, text_len, glyphs, paragraphs)`, where each paragraph
/// is `(block start, lines)`. A type alias keeps the `Option` readable in `Entry`. It is filled
/// **lazily** by the paths that actually lay a paragraph out (the window, the selection and
/// the search), never by the measure pass (resume-perf2: measuring counts a paragraph without
/// building its lines). The glyph tier is part of the key (ADR 0035): the bullet,
/// the quote bar, the task checkboxes and the table borders change the layout.
pub(crate) type MdCache = (
    usize,
    usize,
    crate::footer::Glyphs,
    Vec<(usize, Vec<crate::blocks::BodyLine>)>,
);

/// The Markdown **count** cache: `(width, text_len, glyphs, paragraphs)`, where each paragraph
/// is `(block start, line count)`. The measure pass fills it (a count is cheap — no line is
/// built), so re-measuring the same entry after a fold toggle does not re-parse its Markdown
/// (the R7 toggle gate). The glyph tier is in the key too: a task list's checkbox changes the
/// marker width, so the count depends on it (ADR 0035).
pub(crate) type MdCountCache = (usize, usize, crate::footer::Glyphs, Vec<(usize, usize)>);

/// Whether an [`Input`] is something the **user** did — a key, a paste, a mouse action, an
/// approval answer — as opposed to a host fact or an asynchronous result. Only user input
/// disarms the Ctrl-C quit window: a terminal resize or a
/// background `/v1/models` refresh must not silently cancel the pending quit.
///
/// The match is exhaustive on purpose: a new `Input` variant forces a decision here.
/// The transcript block for a `!`/`!!` command (ADR 0033): `$ cmd` first, the output, the exit
/// status. The live-display form; the model-side text is [`bash_context_text`].
pub(super) fn bash_entry(
    command: &str,
    exclude_from_context: bool,
    run: &radian_coding::UserBash,
) -> Entry {
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
pub(super) fn bash_context_text(command: &str, run: &radian_coding::UserBash) -> String {
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

/// Concatenated text parts, for display. The host's fold uses the same function (ADR 0023 U1).
pub use radian_host::transcript::parts::text_of;

/// A resumed `!` context message as a Bash block. The host's fold uses the same function.
pub use radian_host::transcript::parts::bash_entry_from_context;

pub(super) fn is_user_input(input: &Input) -> bool {
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
        | Input::InspectorKey(_)
        | Input::BtwKey(_)
        | Input::LoginKey(_)
        | Input::ConfirmLogout(_)
        | Input::LogoutMove(_)
        | Input::LogoutChosen(_)
        | Input::TrustMove(_)
        | Input::TrustChosen(_)
        | Input::AccessMove(_)
        | Input::AccessChosen(_)
        | Input::MouseDown { .. }
        | Input::MouseDrag { .. }
        | Input::MouseUp { .. } => true,
        Input::QuitDeadline
        | Input::ForkDeadline
        | Input::LoopDeadline
        | Input::IdleDeadline
        | Input::Resize { .. }
        | Input::Stream(_)
        | Input::Event(_)
        | Input::Runtime(_)
        | Input::Pending(..)
        | Input::MentionsExpanded(_)
        | Input::DroppedLoaded { .. }
        | Input::ClipboardRead(_)
        | Input::CompletionReady(_)
        | Input::CompletionsReady { .. }
        | Input::MemoryCommandDone(_)
        | Input::CompactDone(_)
        | Input::BashDone { .. }
        | Input::ThinkingChecked { .. }
        | Input::ModelChecked { .. }
        | Input::SwitchCompacted { .. }
        | Input::SessionDone(_)
        | Input::ContextReady { .. }
        | Input::GoalDone { .. }
        | Input::WakeBudgetNotice(_)
        | Input::JobNoticeLost(_)
        | Input::IntentStopBlocked(_)
        | Input::HookStopBlocked(_)
        | Input::TrustResolved(_)
        | Input::AccessApplied { .. }
        | Input::CommandRefused(_)
        | Input::PickerOpened(_)
        | Input::InspectorOpened { .. }
        | Input::InspectorRefreshed(_)
        | Input::SideDelta(_)
        | Input::SideDone(_)
        | Input::LoginVerified { .. }
        | Input::LoginSaved { .. }
        | Input::LoginOAuthUrlReady { .. }
        | Input::LoginOAuthDeviceReady { .. }
        | Input::LogoutDone { .. }
        | Input::LogoutChoices { .. }
        | Input::ClipboardDone(_)
        | Input::ThemeReady { .. }
        | Input::SkillExpanded { .. } => false,
    }
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

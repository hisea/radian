//! The transcript's data model: an [`Entry`] and its kind, a compaction summary's display data,
//! the Markdown caches an entry carries, the `!`/`!!` block builders, the startup phase, and the
//! status bar's budget/usage/MCP wording. Pure data and the functions that only format it; the
//! behavior that mutates it lives in `app.rs` and the modules beside this one.

use super::*;

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
    /// ADR 0018, decision 3: live only — `true` when the summary path failed and the
    /// deterministic fallback committed instead. A resume replay does not carry it.
    pub deterministic: bool,
    /// The messages the deterministic fallback dropped; live only, like the numbers above.
    pub dropped: Option<u64>,
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
    /// 12000 → 4000) ──`, `── context compacted (rev 3) ──` or `── context compacted ──`. A
    /// deterministic compaction (ADR 0018, decision 3) says "without a summary".
    pub fn line(&self) -> String {
        let label = if self.deterministic {
            "context compacted without a summary"
        } else {
            "context compacted"
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(revision) = self.revision {
            parts.push(format!("rev {revision}"));
        }
        if let (Some(before), Some(after)) = (self.tokens_before, self.tokens_after) {
            parts.push(format!("{before} → {after}"));
        }
        if let Some(dropped) = self.dropped {
            parts.push(format!("{dropped} dropped"));
        }
        if parts.is_empty() {
            format!("── {label} ──")
        } else {
            format!("── {label} ({}) ──", parts.join(", "))
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
    /// True for the wake-budget notice entry of the current degrade burst. `update`
    /// rewrites this entry (not a new line per degrade) while the burst lasts; the next
    /// turn start clears the session's index, leaving the line as plain history.
    pub wake_notice: bool,
    /// When the live reasoning channel's **first delta** arrived (ADR 0028
    /// decision 4). Live-only: never journaled, so a resumed entry has `None` and shows no
    /// duration. The pair with [`Entry::reasoning_ended`] is the entry's own wall clock; the
    /// duration is their difference.
    pub reasoning_started: Option<Instant>,
    /// When the reasoning entry **ended**: the commit that replaced the live view (ADR 0028
    /// revision 5 decision 4). Live-only, like [`Entry::reasoning_started`]; a resumed entry
    /// carries neither.
    pub reasoning_ended: Option<Instant>,
    /// The parsed blocks (ADR 0028 R1), keyed by text length and the reasoning mode. Filled
    /// for assistant prose and the thinking channel's body (ADR 0028:
    /// both carry fence grammar).
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
    /// every Assistant entry (the R7 toggle gate). Filled lazily by the layout/text paths
    /// (interior mutability), because the measure pass counts Markdown without laying it out.
    pub(crate) md_cache: std::cell::RefCell<Option<MdCache>>,
    /// The Markdown paragraph **line counts**, keyed the same way (resume-perf2). The measure
    /// pass fills this from [`crate::blocks::line_count`], so the first frame (and a fold
    /// toggle) never materializes Markdown lines for an entry it only needs to measure.
    pub(crate) md_count_cache: Option<MdCountCache>,
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

/// The command in a `Ran \`cmd\`` context message, or `None` when the text is not one.
fn bash_context_command(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("Ran `")?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// A resumed `!` context message as a Bash block: `$ cmd` (its first line) and the fenced body.
pub(super) fn bash_entry_from_context(text: &str) -> Option<Entry> {
    let command = bash_context_command(text)?;
    let body = text.find('\n').map_or("", |i| &text[i + 1..]);
    Some(Entry::new(
        EntryKind::Bash {
            exclude_from_context: false,
        },
        format!("{command}\n{}", body.trim_end_matches('\n')),
    ))
}

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
        | Input::GoalDone { .. }
        | Input::WakeBudgetNotice(_)
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

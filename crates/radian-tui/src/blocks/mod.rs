//! The display model (ADR 0028 R1): an entry parses into **blocks**, blocks lay out into
//! **lines**. Everything the transcript shows about an entry goes through [`layout`], and the
//! selection mapping ([`crate::select`]) reads the same [`BodyLine`]s — one geometry, two
//! consumers, so what is highlighted is what is copied.
//!
//! Caches live on [`crate::app::Entry`]:
//!
//! - the **block** cache (parsed ranges, keyed by text length and the reasoning mode — the
//!   fold state is part of the key, ADR 0028 R1). Parsing is incremental: closed blocks are
//!   reused, only the trailing open block is re-parsed, so a streaming frame's parse cost does
//!   not grow with the entry's length (the text only ever grows while live);
//! - the **line count** cache (keyed by width, text length and the mode, as before);
//! - the **Markdown count** cache (keyed by width and text length): the measure pass fills it
//!   with per-paragraph line counts, so it never lays Markdown out (resume-perf2);
//! - the **Markdown line** cache, filled lazily by the paths that lay a paragraph out (the
//!   window, the selection, the search).

use unicode_width::UnicodeWidthStr;

use crate::app::{Entry, EntryKind};
use crate::tools::{self, DisplayModes};

mod ask;
mod context_grid;
mod layout;
pub(crate) mod markdown;
mod thinking;
mod tool_card;

pub(crate) use ask::AskGlyphs;
use ask::{ask_block, ask_pending_line, is_ask_entry};
pub use context_grid::{ContextView, context_entry, context_lines, context_texts};
pub(crate) use layout::parse_blocks_for;
pub use layout::{blocks_for, body_text_lines, layout, layout_with, line_count, line_count_cached};
#[cfg(test)]
pub(crate) use layout::{body_text_lines_for_test, layout_for_test};
pub(crate) use markdown::DiagramOutcome;
use markdown::{
    MdGlyphs, code_body, code_frame_bottom, code_frame_top, code_line_count, code_lines,
    code_text_lines, diagram_for, diagram_outcome, fill_md_count_cache, is_mermaid,
    md_line_count_cached, md_lines_cached, mermaid_sources, prose_lines, syntax_lines_cached,
};
#[cfg(test)]
use markdown::{md_line_count, md_lines};
use thinking::{placeholder, reasoning_body_lines, reasoning_header};
pub use tool_card::{HeadMarker, head_marker};
use tool_card::{body_plan_lines, diff_line, head_line_count, head_rows};

// How many Markdown paragraphs `md_lines` has laid out, for the tests that prove the measure
// pass only counts (resume-perf2). Thread-local, like the highlight counter: parallel tests in
// one binary must not share a process-global counter.
thread_local! {
    static MD_LINE_BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The number of Markdown paragraphs laid out so far **on this thread**.
pub fn md_line_builds() -> u64 {
    MD_LINE_BUILDS.with(std::cell::Cell::get)
}

/// How the reasoning channel displays (ADR 0028 R3). The fold state is part of the block and
/// layout cache keys: toggling it re-lays the affected entries out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningMode {
    /// One placeholder line: `◼ thinking (N lines)` (the ADR's default).
    #[default]
    Collapsed,
    /// The full text, wrapped as prose.
    Expanded,
    /// Nothing at all: the entry contributes zero lines.
    Hidden,
}

impl ReasoningMode {
    /// The `tui.display.reasoning` spelling.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "collapsed" => Some(Self::Collapsed),
            "expanded" => Some(Self::Expanded),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }

    /// Steps `Collapsed → Expanded → Collapsed`; `Hidden` is config-only (Ctrl-T does not
    /// rescue a mode the user hid on purpose — pi's toggle behaves the same).
    pub fn toggled(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::Collapsed,
            Self::Hidden => Self::Hidden,
        }
    }
}

/// One parsed block: a byte range of the entry's display text. Ranges tile the text (each
/// `start` is the previous block's `end`), so incremental parsing can cut at any boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Plain prose: wraps at the body width (today's behavior).
    Paragraph {
        start: usize,
        end: usize,
        closed: bool,
    },
    /// A fenced code block (ADR 0028 R9): never wraps, one source line per display line.
    /// `lang` is the info string after the fence (` ```rust ` → `rust`).
    Code {
        start: usize,
        end: usize,
        closed: bool,
        lang: String,
    },
}

impl Block {
    pub fn start(&self) -> usize {
        match self {
            Block::Paragraph { start, .. } | Block::Code { start, .. } => *start,
        }
    }

    pub fn end(&self) -> usize {
        match self {
            Block::Paragraph { end, .. } | Block::Code { end, .. } => *end,
        }
    }

    pub fn closed(&self) -> bool {
        match self {
            Block::Paragraph { closed, .. } | Block::Code { closed, .. } => *closed,
        }
    }
}

/// The fence markers this slice recognizes (ADR 0028 R9: fenced code only; Markdown is
/// slice 3). A fence is a line whose trimmed text starts with three or more ``` or ~~~.
const FENCE: [char; 2] = ['`', '~'];

/// Parses the whole text fresh. See [`parse_blocks_incremental`] for the streaming path.
pub fn parse_blocks(text: &str) -> Vec<Block> {
    parse_blocks_incremental(text, None)
}

/// Parses `text`, reusing `prev`'s **closed** blocks (ranges stay valid because the text only
/// ever grows while an entry is live). Everything from the first non-closed block on is
/// re-parsed — the "only the last block" rule (ADR 0028 R1).
pub fn parse_blocks_incremental(text: &str, prev: Option<&[Block]>) -> Vec<Block> {
    let reuse: &[Block] = match prev {
        Some(blocks) => {
            let cut = blocks
                .iter()
                .position(|block| !block.closed())
                .unwrap_or(blocks.len());
            &blocks[..cut]
        }
        None => &[],
    };
    let mut blocks = reuse.to_vec();
    let mut at = match blocks.last() {
        Some(last) => last.end(),
        None => 0,
    };
    if at > text.len() {
        // Not append-only (a new entry, or the text was replaced): nothing to reuse.
        blocks.clear();
        at = 0;
    }
    let mut fence: Option<char> = None;
    let mut lang = String::new();
    let mut block_start = at;
    let mut block_open = false;
    while at <= text.len() {
        // One line: [line_start, line_end), `line_end` includes the '\n'.
        let line_start = at;
        let line_end = match text[at..].find('\n') {
            Some(newline) => at + newline + 1,
            None => text.len(),
        };
        let final_line = line_end >= text.len();
        let line = &text[line_start..line_end];
        let trimmed = line.trim_start().trim_end_matches('\n');
        if let Some(open) = fence {
            // Inside a code block: a closing fence is a run of the opener (3+) and nothing
            // else. The closing fence line belongs to the block (it may be the last line).
            if fence_closes(trimmed, open) {
                blocks.push(Block::Code {
                    start: block_start,
                    end: line_end,
                    closed: true,
                    lang: std::mem::take(&mut lang),
                });
                fence = None;
                block_start = line_end;
                block_open = false;
            }
        } else if let Some((marker, info)) = fence_open(trimmed) {
            // A fence opens. The block starts at the fence line (any blank line above it
            // already closed into the previous paragraph, so the ranges keep tiling).
            if block_open {
                blocks.push(Block::Paragraph {
                    start: block_start,
                    end: line_start,
                    closed: true,
                });
            }
            fence = Some(marker);
            lang = info;
            block_start = line_start;
            block_open = false;
        } else if trimmed.is_empty() {
            // A blank line closes the paragraph above it and belongs to it (it shows as one
            // empty display line). With no paragraph open — after a fence, or after a reused
            // closed block — it is its own one-line paragraph, so the ranges keep tiling
            // without tearing a code block apart.
            if block_open {
                blocks.push(Block::Paragraph {
                    start: block_start,
                    end: line_end,
                    closed: true,
                });
                block_open = false;
            } else {
                blocks.push(Block::Paragraph {
                    start: line_start,
                    end: line_end,
                    closed: true,
                });
            }
            block_start = line_end;
        } else {
            // Prose. Open a paragraph if none is running; it closes at a blank line, a
            // fence, or — only when the text ends without a newline — here. A final line
            // with its newline is a complete line: closed, appending starts a new block. A
            // final line without one is still being written: open, appending extends it.
            if !block_open {
                block_start = line_start;
                block_open = true;
            }
            if final_line && line_end == text.len() && !text.ends_with('\n') {
                blocks.push(Block::Paragraph {
                    start: block_start,
                    end: line_end,
                    closed: false,
                });
                // The assignment below (`block_open = false`) is what the trailing block
                // flush checks; this arm leaves it open so the flush does not double-push.
                break;
            }
        }
        if line_end >= text.len() {
            if block_open {
                blocks.push(Block::Paragraph {
                    start: block_start,
                    end: line_end,
                    closed: true,
                });
            }
            break;
        }
        at = line_end;
    }
    // A code block still open when the text ran out (the closing fence has not streamed in
    // yet): it is the block being written, so it stays open and re-parses on the next delta.
    if fence.is_some() {
        blocks.push(Block::Code {
            start: block_start,
            end: text.len(),
            closed: false,
            lang,
        });
    }
    blocks
}

/// `Some((marker_char, info_string))` when the trimmed line opens a fence.
fn fence_open(trimmed: &str) -> Option<(char, String)> {
    let marker = trimmed.chars().next()?;
    if !FENCE.contains(&marker) {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == marker).count();
    if run < 3 {
        return None;
    }
    let info = trimmed[run..].trim().to_owned();
    // A fence with text after it (other than the info string) is not a fence in CommonMark;
    // this slice keeps it simple: backticks in the info string disqualify.
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some((marker, info))
}

/// Whether the trimmed line closes a fence opened with `marker`.
fn fence_closes(trimmed: &str, marker: char) -> bool {
    let run = trimmed.chars().take_while(|&c| c == marker).count();
    run >= 3 && trimmed[run..].trim().is_empty()
}

/// Where a tool entry sits in a call/result pair (ADR 0028's merge; no
/// longer draws a box, so the block is just the call's head row followed by the result's `│ `
/// rows). The data stays two entries (the journal and the projection are untouched); this only
/// records the pairing, which the `ask_user` record reads to decide whether to draw its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Card {
    /// Not a tool entry.
    #[default]
    None,
    /// The call that heads a block its result closes.
    Head,
    /// The result that closes the block its call opened.
    Tail,
    /// A tool entry with no pair: a call still running, or a torn transcript.
    Solo,
}

/// The tool block's line shape (ADR 0028). A block is one head line and
/// `│ `-prefixed body rows; the markers live in the marker column and are **drawn**, never part
/// of the line's text, so a selection copies none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Frame {
    #[default]
    None,
    /// The block's head row: the fold marker in the marker column, `<name> <title> · <meta>
    /// <status>` from the body column.
    Head,
    /// A body row: `│ ` in the marker column, the content from the body column.
    Body,
}

/// One laid-out line of an entry's body. `chars`/`end` are the selection mapping's view (the
/// same semantics as [`crate::render::WrappedLine`]: byte offsets into the entry's display
/// text, `end` the gap after the line); `text` is what the body span renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLine {
    /// The source byte offset of each char on this line, in order (`None` when the renderer
    /// synthesized the char — a bullet, a heading's `##`, a link's `(url)`, an indent, a tool
    /// block's marker, meta or status glyph). A synthesized cell still occupies its display
    /// columns but has no source text: it never copies and never anchors a selection.
    pub chars: Vec<Option<usize>>,
    pub end: usize,
    /// What renders. Code lines are tab-expanded and truncated to the width with `…` (the
    /// full line still copies: `chars` only covers the kept prefix, and `end` reaches the
    /// line's true end, ADR 0028 R9).
    pub text: String,
    /// This line is a code line (rendered without the prefix lead, no wrap).
    pub code: bool,
    /// The tool-block row this line draws (ADR 0028); `None` for every
    /// line that is not part of a tool block.
    pub frame: Frame,
    /// The role a tool body line renders with (ADR 0028 R4's diff roles and the warning
    /// channel); `Normal` for everything that is not a tool body line.
    pub role: tools::LineRole,
    /// A synthesized tool hint (no source characters: it neither copies nor matches).
    pub hint: bool,
    /// A rendered Mermaid diagram line (ADR 0028 R8): per-piece text and role. `None` for
    /// every other line; `text` still holds the concatenation, so search and the line count
    /// agree with what is drawn.
    pub diagram: Option<Vec<(String, DiagramRole)>>,
    /// A weak one-line explanation (a Mermaid block that could not be drawn): the renderer
    /// styles it with the theme's `truncation` (dim) role.
    pub reason: bool,
    /// A Markdown line (ADR 0028 R2): per-piece text, role and source offset. `None` for
    /// every other line; `text` still holds the concatenation, so search and the line count
    /// agree with what is drawn.
    pub md: Option<Vec<MdPiece>>,
    /// A syntax-highlighted code line (ADR 0028 R2): per-piece text and Radian syntax role
    /// (`None` when the block is not highlighted). `text` still holds the concatenation.
    pub syntax: Option<crate::highlight::HighlightedLine>,
}

/// One Markdown piece: its displayed text, its role, and its source offset (`None` when the
/// renderer synthesized it — a bullet, a bar, a border, a link's URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdPiece {
    pub text: String,
    pub style: MdStyle,
    pub offset: Option<usize>,
}

/// A `/context` grid category's own color (spec `tui-context-grid`): system
/// prompt, context files, memory, built-in tool definitions, skills, MCP
/// tools, user messages, assistant text, reasoning, tool results, free space
/// and the compaction buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxTone {
    System,
    Files,
    Memory,
    Tools,
    Skills,
    Mcp,
    User,
    Assistant,
    Reasoning,
    Results,
    Free,
    Buffer,
}

/// A Markdown piece's render role (ADR 0028 R2, ADR 0029 decision 3: pi's `md*` roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdStyle {
    /// A level-2 heading's text: the heading role (`mdHeading`).
    Heading,
    /// A level-1 heading's text: the heading role plus underline (ADR 0035
    /// decision 4).
    Heading1,
    /// A level-3-or-lower heading's text: the text role plus bold (ADR 0035
    /// decision 4). `md_heading_mark` is no longer drawn; the role stays in the theme for
    /// old theme files.
    HeadingMinor,
    /// `~~struck~~`: the crossed-out modifier.
    Strikethrough,
    /// A task list's checkbox (`☐`/`☑`, `[ ]`/`[x]` under ascii).
    TaskUnchecked,
    TaskChecked,
    /// A code block's frame line and rail (ADR 0035): the `frame` role
    /// (ADR 0029).
    Frame,
    Bold,
    Italic,
    /// Inline code (`mdCode`).
    Code,
    /// A link's text (`mdLink`).
    Link,
    /// The synthesized ` (url)` (`mdLinkUrl`).
    LinkUrl,
    /// A quote's text (`mdQuote`).
    Quote,
    /// The `ask_user` block's own roles (ADR 0024 R1): the emphasis, a weak line,
    /// and a structural line. Not Markdown; the same per-piece mechanism.
    Accent,
    Muted,
    Dim,
    /// A diff row's own roles (ADR 0028): the synthesized line number,
    /// the sign and the code, which one row carries at once. Not Markdown either; the same
    /// per-piece mechanism.
    Diff(crate::tools::DiffRole),
    /// A tool head's tool name (ADR 0028 / ADR 0029's
    /// `tool_name`).
    ToolName,
    /// A tool head's status glyph when the call succeeded (`✓`, `status_ok`).
    StatusOk,
    /// A tool head's status glyph when the call failed (`×`, `status_error`).
    StatusError,
    /// A tool head's running indicator: the live pulse frame, drawn in place of the
    /// placeholder cell the layout carries (ADR 0027).
    StatusRunning,
    /// A quote's `│ ` bar (`mdQuoteBorder`).
    QuoteBar,
    /// One `/context` grid cell or legend glyph (spec `tui-context-grid`): the
    /// estimated category's own theme role. Not Markdown; the same per-piece
    /// mechanism as the tool head's name and status glyphs.
    Ctx(CtxTone),
    /// A list bullet or number (`mdListBullet`).
    Bullet,
    /// A thematic break (`mdHr`).
    Rule,
    /// A table's `│`/`─`/`┌` borders (`mdCodeBlockBorder`).
    TableBorder,
    Plain,
}

impl BodyLine {
    fn prose(chars: Vec<Option<usize>>, end: usize, text: String) -> Self {
        Self {
            chars,
            end,
            text,
            code: false,
            frame: Frame::None,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: false,
            md: None,
            syntax: None,
        }
    }

    /// A diagram line: every cell maps to the block's start (the diagram has no source
    /// offsets of its own), so selecting it selects the block — a copy yields the Mermaid
    /// source, the content that actually exists in the text.
    fn diagram(end: usize, offset: usize, spans: Vec<(String, DiagramRole)>) -> Self {
        let text: String = spans.iter().map(|(piece, _)| piece.as_str()).collect();
        let cells = text.chars().count();
        Self {
            chars: vec![Some(offset); cells],
            end,
            text,
            code: true,
            frame: Frame::None,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: Some(spans),
            reason: false,
            md: None,
            syntax: None,
        }
    }

    /// A Markdown line: its pieces, and the `chars` mapping derived from their offsets.
    fn md(chars: Vec<Option<usize>>, end: usize, pieces: Vec<MdPiece>) -> Self {
        let text: String = pieces.iter().map(|piece| piece.text.as_str()).collect();
        Self {
            chars,
            end,
            text,
            code: false,
            frame: Frame::None,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: false,
            md: Some(pieces),
            syntax: None,
        }
    }

    fn reason(end: usize, offset: usize, text: String) -> Self {
        Self {
            chars: vec![Some(offset)],
            end,
            text,
            code: false,
            frame: Frame::None,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: true,
            md: None,
            syntax: None,
        }
    }
}

/// A diagram piece's semantic role (ADR 0042 decision 4): the vendored renderer's five roles
/// collapse to these three, so `radian-tui` does not expose the dependency's type in its own API
/// and the theme keeps its three mermaid roles (ADR 0029).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramRole {
    Node,
    Edge,
    Text,
}

/// The display modes the search matches with (ADR 0028 R6): **everything expanded**, so a
/// folded thinking block, tool body or compaction summary is still found. The jump maps the
/// expanded line onto the display after opening the one entry that matched.
pub fn search_modes(modes: DisplayModes) -> DisplayModes {
    DisplayModes {
        reasoning: ReasoningMode::Expanded,
        tools: crate::tools::ToolDisplay {
            fold: crate::tools::ToolFold::Expanded,
            preview_lines: modes.tools.preview_lines,
        },
        // A diagram is drawn (or not) the same way while searching: its lines are what the
        // search matches. The same for syntax highlighting.
        mermaid: modes.mermaid,
        highlight: modes.highlight,
        glyphs: modes.glyphs,
    }
}

/// Whether this entry's display **hides content** the search's expanded geometry shows
/// (ADR 0028 R6 decides the per-entry override from this): a folded thinking channel, a folded
/// tool body that really cuts lines, or a folded compaction summary. A body that fits its
/// preview hides nothing and needs no override.
pub fn is_folded(entry: &Entry, modes: DisplayModes) -> bool {
    match entry.kind {
        EntryKind::Reasoning => !expanded(entry, modes.reasoning == ReasoningMode::Expanded),
        EntryKind::Compaction => !expanded(entry, modes.tools.fold == tools::ToolFold::Expanded),
        EntryKind::ToolResult => {
            if expanded(entry, modes.tools.fold == tools::ToolFold::Expanded) {
                return false;
            }
            let Some(card) = entry.tool.as_deref() else {
                return false;
            };
            let text = crate::render::display_text(entry);
            let full = tools::body_plan(
                card,
                &text,
                entry.is_error,
                tools::ToolFold::Expanded,
                modes.tools.preview_lines,
            );
            let shown = tools::body_plan(
                card,
                &text,
                entry.is_error,
                modes.tools.fold,
                modes.tools.preview_lines,
            );
            shown.lines.len() < full.lines.len()
        }
        _ => false,
    }
}

/// Strips an entry's **backgroundless blank** edge lines (ADR 0028 rule 1): the
/// container owns the spacing between blocks, so a text-derived blank line at an entry's
/// start or end (a leading/trailing newline in the message) must not add a second one. The
/// layout is the one place this happens, so counting, drawing, the selection mapping and the
/// search all see the stripped form.
fn strip_edge_blanks(lines: &mut Vec<BodyLine>) {
    // `chars.is_empty()` would also match a **synthesized** line that draws text but has no
    // source characters — the thinking channel's `◻ thinking · …` header or a tool block's
    // head (ADR 0028). `text.is_empty()` is the real "blank line"
    // test; a synthesized line with text is content and must survive.
    let blank = |line: &BodyLine| !line.hint && line.text.is_empty();
    let start = lines
        .iter()
        .position(|line| !blank(line))
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !blank(line))
        .map_or(start, |index| index + 1);
    lines.drain(end..);
    lines.drain(..start);
}

/// The blank lines the container inserts between two adjacent entries (ADR 0028
/// rule 1). Zero when the two entries are one visual block:
///
/// * a tool call and **its own** result (matched by call id, rule 4) - the result hugs its
///   call, with no gap in between;
/// * the startup block: one block built from several entries (see [`is_startup_block`]);
/// * a skipped empty assistant entry ([`is_empty_assistant`]): it draws no line, so it has no
///   blank line before it either.
///
/// `prev` must be the entry the blank line really follows: the last one that is **not**
/// skipped. [`gap_at`] resolves it from the entries; call it, not this, for a transcript
/// entry's own gap.
///
/// A user row is an ordinary block now (ADR 0035: no padding rows used to be its
/// separation), so it gets the container's one blank line on both sides like every other block.
///
/// Otherwise exactly one line -- the thinking channel included: ADR 0035 replaced
/// 's "the folded one-liner hugs the block it introduces" with one blank line on both
/// sides, because a folded `> thinking (N lines)` against the reply read as one cramped block.
/// The counting ([`line_count`]'s callers), the drawing
/// ([`crate::render::visible_lines`]), the selection mapping and the find geometry all call
/// [`gap_at`], so a gap cannot exist in one and not the other.
pub fn block_gap(prev: Option<&Entry>, next: &Entry) -> usize {
    if is_empty_assistant(next) {
        return 0;
    }
    let Some(prev) = prev else {
        return 0;
    };
    if is_call_result_pair(prev, next) {
        return 0;
    }
    // The startup block is **one** block built from several entries (the wordmark, the folded
    // line or the full lines, the safety lines that never fold, the Tip): no gaps inside it.
    if is_startup_block(prev.kind) && is_startup_block(next.kind) {
        return 0;
    }
    1
}

/// Whether the entry is a **text-less assistant turn**: a committed (not live) assistant entry
/// whose text is empty or whitespace only (a reply that was only tool calls or only reasoning).
/// It lays out as zero lines, and the container gives it no blank line and does not count it as
/// a `prev` for the entry after it ([`gap_at`]), so a reasoning block followed by a text-less
/// reply followed by a tool call shows **one** blank row, not two (ADR 0028 rule 1,
/// ADR 0035).
///
/// The entry itself stays in `entries` (`push_message` keeps it: the interrupted marker, usage
/// and copy-last-reply read it); only its layout and its blank lines are skipped. A **live**
/// empty entry is drawn as it always was, so a reply that is still starting does not shift, and
/// an **interrupted** one is never skipped either: it has to keep the `[interrupted]` marker
/// visible (ADR 0003), and the layout gives it one line to carry it ([`layout_with`]).
pub fn is_empty_assistant(entry: &Entry) -> bool {
    entry.kind == EntryKind::Assistant
        && !entry.live
        && !entry.interrupted
        && entry.text.trim().is_empty()
}

/// The index of the last entry before `entries[index]` that is not skipped ([`is_empty_assistant`]).
/// That entry is what a blank line before `entries[index]` follows and what a selection on that
/// blank line anchors to. `None` when no entry precedes it.
pub fn previous_shown(entries: &[Entry], index: usize) -> Option<usize> {
    entries[..index.min(entries.len())]
        .iter()
        .rposition(|entry| !is_empty_assistant(entry))
}

/// The blank lines before `entries[index]` (ADR 0028 rule 1): [`block_gap`] against
/// the last entry that is **not** skipped, so a skipped empty assistant entry contributes no
/// line of its own and no gap of its own. Every gap consumer (the drawing
/// [`crate::render::visible_lines`], the counting, the selection mapping and the find geometry)
/// calls this, so they cannot disagree.
pub fn gap_at(entries: &[Entry], index: usize) -> usize {
    let Some(next) = entries.get(index) else {
        return 0;
    };
    let prev = previous_shown(entries, index).map(|prev| &entries[prev]);
    block_gap(prev, next)
}

/// The entries the startup block is made of (ADR 0027 §标志 / the startup
/// screen): one visual block, so the container inserts no gaps between them.
fn is_startup_block(kind: EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::Banner | EntryKind::Startup | EntryKind::Warning | EntryKind::Tip
    )
}

/// Whether `prev` is the tool call `next` answers (same call id).
pub fn is_call_result_pair(prev: &Entry, next: &Entry) -> bool {
    prev.kind == EntryKind::ToolCall
        && next.kind == EntryKind::ToolResult
        && prev
            .tool
            .as_ref()
            .zip(next.tool.as_ref())
            .is_some_and(|(call, result)| call.call_id == result.call_id)
}

/// The gap before each entry: `gaps[i]` is the blank lines before entry `i` (`gaps[0]` is 0).
/// The container's total line count is the sum of the entries' counts plus these.
pub fn gaps(entries: &[Entry]) -> Vec<usize> {
    (0..entries.len())
        .map(|index| gap_at(entries, index))
        .collect()
}

/// Whether the entry's display text has a blank **edge paragraph** (a leading or trailing
/// newline, or an empty text): the layout strips those lines (rule 1), so a counting path that
/// does not want to lay the entry out has to fall back to [`layout`] in this case. Every other
/// text keeps the fast path.
fn has_blank_edge(text: &str) -> bool {
    let blank = |paragraph: &str| paragraph.trim().is_empty();
    text.split('\n').next().is_some_and(blank) || text.rsplit('\n').next().is_some_and(blank)
}

/// Whether a foldable entry shows its content: the global fold says so, or a per-entry
/// override (ADR 0028 R6: a search hit opens exactly one entry).
pub fn expanded(entry: &Entry, global: bool) -> bool {
    entry.fold_override.unwrap_or(global)
}

#[cfg(test)]
mod tests {
    use super::layout::tests::EXPANDED;
    use super::*;

    /// A streaming append re-parses only the trailing open block: the closed prefix is copied
    /// from `prev` verbatim (ADR 0028 R1). A sentinel in `prev` proves it, because a re-parse
    /// would replace it with the real block.
    #[test]
    fn a_streaming_append_reuses_the_closed_blocks() {
        let text = "# Title\n\nfirst paragraph\n\n```\ncode\n```\n";
        let first = parse_blocks(text).remove(0);
        let sentinel = Block::Code {
            start: first.start(),
            end: first.end(),
            closed: true,
            lang: "SENTINEL".to_owned(),
        };
        let appended = format!("{text}second paragraph\n");
        let parsed = parse_blocks_incremental(&appended, Some(std::slice::from_ref(&sentinel)));
        assert_eq!(
            parsed.first(),
            Some(&sentinel),
            "the closed prefix must be reused, not re-parsed: {parsed:?}"
        );
        assert!(
            parsed.len() > 1,
            "the appended text is parsed fresh: {parsed:?}"
        );
        // And a fresh parse (no `prev`) does not keep the sentinel.
        let fresh = parse_blocks_incremental(&appended, None);
        assert_ne!(fresh.first(), Some(&sentinel));
    }

    #[test]
    fn the_container_gap_is_one_line_except_within_a_block() {
        let user = Entry::new(EntryKind::User, "hi");
        let assistant = Entry::new(EntryKind::Assistant, "hello");
        let notice = Entry::new(EntryKind::Notice, "note");
        let thinking = Entry::new(EntryKind::Reasoning, "a thought");
        let banner = Entry::new(EntryKind::Banner, "radian");
        let startup = Entry::new(EntryKind::Startup, "profile: x");
        let mut call = Entry::new(EntryKind::ToolCall, "-> shell");
        call.tool = Some(Box::new(tools::ToolCard::new(
            "c1",
            "shell",
            serde_json::json!({}),
        )));
        let mut result = Entry::new(EntryKind::ToolResult, "ok");
        result.tool = Some(Box::new(tools::ToolCard::new(
            "c1",
            "shell",
            serde_json::json!({}),
        )));
        let mut other = Entry::new(EntryKind::ToolResult, "other");
        other.tool = Some(Box::new(tools::ToolCard::new(
            "c2",
            "shell",
            serde_json::json!({}),
        )));

        assert_eq!(block_gap(None, &assistant), 0, "the first entry has no gap");
        assert_eq!(block_gap(Some(&assistant), &notice), 1, "ordinary blocks");
        // The thin user row is an ordinary block (ADR 0035): the container's one
        // blank line above and below it, no padding rows.
        assert_eq!(block_gap(Some(&assistant), &user), 1, "above a user row");
        assert_eq!(block_gap(Some(&user), &assistant), 1, "below a user row");
        // ADR 0035: thinking keeps the container's one blank line below it, next to
        // the reply and next to a tool call.
        assert_eq!(
            block_gap(Some(&thinking), &assistant),
            1,
            "one line between thinking and the reply"
        );
        assert_eq!(
            block_gap(Some(&thinking), &call),
            1,
            "one line between thinking and a call"
        );
        assert_eq!(
            block_gap(Some(&thinking), &thinking),
            1,
            "two thinking entries are two blocks"
        );
        assert_eq!(
            block_gap(Some(&assistant), &thinking),
            1,
            "one line above thinking too"
        );
        assert_eq!(block_gap(Some(&call), &result), 0, "a result hugs its call");
        assert_eq!(
            block_gap(Some(&call), &other),
            1,
            "another call's result is another block"
        );
        assert_eq!(block_gap(Some(&banner), &startup), 0, "the startup block");
        assert_eq!(block_gap(Some(&startup), &assistant), 1, "after it");
        assert_eq!(gaps(&[assistant, notice]).len(), 2);
    }

    /// A committed text-less assistant reply (only tool calls, or only reasoning) is skipped by
    /// the gap geometry: it has no blank line of its own, and the entry after it is measured
    /// against the last entry that is not skipped (ADR 0028 rule 1). A **live** or an
    /// **interrupted** empty entry is not skipped, and the pair rules (a call and its own result)
    /// still hold across a skip.
    #[test]
    fn an_empty_committed_assistant_is_skipped_by_the_gaps() {
        assert!(is_empty_assistant(&Entry::new(EntryKind::Assistant, "")));
        assert!(
            is_empty_assistant(&Entry::new(EntryKind::Assistant, "  \n ")),
            "whitespace only counts as empty"
        );
        let mut live = Entry::new(EntryKind::Assistant, "");
        live.live = true;
        assert!(!is_empty_assistant(&live), "a live entry is never skipped");
        // An interrupted empty reply is not skipped either: it keeps one line for the
        // `[interrupted]` marker (ADR 0003).
        let mut interrupted = Entry::new(EntryKind::Assistant, "");
        interrupted.interrupted = true;
        assert!(
            !is_empty_assistant(&interrupted),
            "an interrupted reply keeps its block"
        );
        assert_eq!(
            line_count(&interrupted, EXPANDED, 20),
            1,
            "and it lays out one line for the marker"
        );
        assert!(!is_empty_assistant(&Entry::new(EntryKind::User, "")));
        assert!(!is_empty_assistant(&Entry::new(EntryKind::Assistant, "x")));

        // The skipped entry has no gap before it, and the tool call takes the reasoning block's
        // one blank line instead of adding a second one.
        let entries = vec![
            Entry::new(EntryKind::User, "q"),
            Entry::new(EntryKind::Reasoning, "a thought"),
            Entry::new(EntryKind::Assistant, ""),
            Entry::new(EntryKind::ToolCall, "-> shell"),
        ];
        assert_eq!(gap_at(&entries, 2), 0, "the skipped reply has no gap");
        assert_eq!(
            gap_at(&entries, 3),
            1,
            "the tool call is measured against the reasoning block"
        );
        assert_eq!(gaps(&entries), vec![0, 1, 0, 1]);
        assert_eq!(previous_shown(&entries, 3), Some(1));

        // Skipping the very first entry leaves the next one a first block (no gap).
        let entries = vec![
            Entry::new(EntryKind::Assistant, ""),
            Entry::new(EntryKind::User, "q"),
        ];
        assert_eq!(gaps(&entries), vec![0, 0]);
        assert_eq!(previous_shown(&entries, 1), None);

        // A call/result pair still hugs across a skip: the result's gap is measured against
        // the call, not against the skipped reply between them.
        let card = |id: &str| {
            Some(Box::new(tools::ToolCard::new(
                id,
                "shell",
                serde_json::json!({}),
            )))
        };
        let mut call = Entry::new(EntryKind::ToolCall, "-> shell");
        call.tool = card("c1");
        let mut result = Entry::new(EntryKind::ToolResult, "ok");
        result.tool = card("c1");
        let entries = vec![call, Entry::new(EntryKind::Assistant, ""), result];
        assert_eq!(
            gaps(&entries),
            vec![0, 0, 0],
            "the result still hugs its call"
        );
        assert_eq!(previous_shown(&entries, 2), Some(0));
    }

    #[test]
    fn fences_split_paragraphs_and_carry_the_language() {
        let blocks = parse_blocks("prose one\n\n```rust\nlet x = 1;\n```\nprose two");
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0], Block::Paragraph { closed: true, .. }));
        assert!(matches!(
            &blocks[1],
            Block::Code { closed: true, lang, .. } if lang == "rust"
        ));
        // The final paragraph has no trailing newline: it is still being written, so it
        // stays open and the next delta re-parses only it.
        assert!(matches!(blocks[2], Block::Paragraph { closed: false, .. }));
        // The ranges tile the text.
        assert_eq!(blocks[0].end(), blocks[1].start());
        assert_eq!(blocks[1].end(), blocks[2].start());
    }

    #[test]
    fn tildes_fence_too_and_an_unclosed_fence_stays_open() {
        let blocks = parse_blocks("~~~\ncode\n");
        assert!(matches!(blocks[0], Block::Code { closed: false, .. }));
        // Closing it closes the block without changing the prefix.
        let blocks = parse_blocks_incremental("~~~\ncode\n~~~\n", Some(&blocks));
        assert!(matches!(blocks[0], Block::Code { closed: true, .. }));
    }

    #[test]
    fn incremental_parsing_reuses_only_closed_blocks() {
        let before = parse_blocks("first para\n\nsecond para keeps growing");
        assert_eq!(before.len(), 2);
        assert!(before[0].closed());
        assert!(!before[1].closed(), "the last line is still being written");
        let after = parse_blocks_incremental(
            "first para\n\nsecond para keeps growing, and grows",
            Some(&before),
        );
        // The first block is byte-identical; only the open one was re-parsed.
        assert_eq!(after[0], before[0]);
        assert!(after[1].end() > before[1].end());
        // A completed line (with its newline) closes its block: appending starts a new one.
        let newline =
            parse_blocks_incremental("first para\n\nsecond para keeps growing\n", Some(&after));
        assert_eq!(newline[0], before[0]);
        assert_eq!(newline[1].start(), after[1].start());
        assert!(newline[1].closed());
        assert_eq!(newline.len(), 2);
        // Closing the second paragraph with a blank line + a fence keeps its start.
        let done = parse_blocks_incremental(
            "first para\n\nsecond para keeps growing\n\n```\ncode\n```",
            Some(&newline),
        );
        assert_eq!(done[0], before[0]);
        let paragraph = done
            .iter()
            .find(|block| matches!(block, Block::Paragraph { start, .. } if *start == after[1].start()))
            .expect("the second paragraph is still there");
        assert!(paragraph.closed());
        let code = done
            .iter()
            .find(|block| matches!(block, Block::Code { .. }))
            .expect("the fence became a code block");
        assert!(code.closed());
        // Everything tiles.
        for pair in done.windows(2) {
            assert_eq!(pair[0].end(), pair[1].start(), "{pair:?}");
        }
    }
}

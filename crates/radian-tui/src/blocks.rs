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

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{Entry, EntryKind};
use crate::tools::{self, DisplayModes};

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
    /// One placeholder line: `▶ thinking (N lines)` (the ADR's default).
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

/// Where a tool entry sits in a call/result pair (ADR 0028's merge; revision 5 no
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
    /// The block's head row: `• ` in the marker column, `<name> <title> · <meta> <status>` from
    /// the body column.
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
    /// placeholder cell the layout carries (ADR 0027/13).
    StatusRunning,
    /// A quote's `│ ` bar (`mdQuoteBorder`).
    QuoteBar,
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

/// A diagram piece's semantic role (ADR 0028 R8): the vendor's `Node`/`Edge`/`Text`, mapped
/// so `radian-tui` does not expose the dependency's type in its own API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramRole {
    Node,
    Edge,
    Text,
}

/// One Mermaid block's render result. `Failed` keeps the reason for the dim line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiagramOutcome {
    Rendered(Vec<Vec<(String, DiagramRole)>>),
    Failed(String),
}

/// Renders `source` at `width` (ADR 0028 R8): a pure function of the two, so the cache key is
/// exactly that pair. No partial diagram: `render_spans` either returns every line or an error.
pub(crate) fn diagram_for(source: &str, width: usize) -> DiagramOutcome {
    let t = std::time::Instant::now();
    let rendered = codex_mermaid::render_spans(source, width);
    radian_session::trace::took("first-frame/mermaid", t);
    match rendered {
        Ok(lines) => DiagramOutcome::Rendered(
            lines
                .into_iter()
                .map(|line| {
                    line.into_iter()
                        .map(|span| {
                            let role = match span.role {
                                codex_mermaid::Role::Node => DiagramRole::Node,
                                codex_mermaid::Role::Edge => DiagramRole::Edge,
                                codex_mermaid::Role::Text => DiagramRole::Text,
                            };
                            (span.text, role)
                        })
                        .collect()
                })
                .collect(),
        ),
        Err(error) => DiagramOutcome::Failed(error.to_string()),
    }
}

/// Whether a code block is a Mermaid diagram this renderer should draw: the info string is
/// `mermaid` (ASCII case-insensitive — CommonMark keeps info strings case-sensitive, but
/// every fence a model writes is lowercase; accepting any case is more forgiving and never
/// changes a non-mermaid block), the block is closed, and `tui.display.mermaid` is on.
fn is_mermaid(lang: &str, closed: bool, mermaid: bool) -> bool {
    mermaid && closed && lang.eq_ignore_ascii_case("mermaid")
}

/// The outcome for one Mermaid block, from the entry's cache when the `(width, text_len)` key
/// matches, else rendered fresh (the immutable paths — selection, search, the reference
/// renderer — may not have run the mutable measure pass).
fn diagram_outcome(entry: &Entry, index: usize, source: &str, width: usize) -> DiagramOutcome {
    if let Some((cached_width, cached_len, outcomes)) = &entry.diagram_cache
        && *cached_width == width
        && *cached_len == entry.text.len()
        && let Some(outcome) = outcomes.get(index)
    {
        return outcome.clone();
    }
    diagram_for(source, width)
}

/// A fenced code block's **body** (the fences are markup), joined by newlines — what a
/// renderer (Mermaid) sees. Mirrors [`code_lines`]'s skipping.
fn code_body(raw: &str, closed: bool) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let body = match (lines.first(), lines.last()) {
        (Some(first), Some(last))
            if fence_open(first.trim()).is_some()
                && closed
                && lines.len() > 1
                && fence_run(last.trim()).is_some() =>
        {
            &lines[1..lines.len() - 1]
        }
        (Some(first), _) if fence_open(first.trim()).is_some() && lines.len() > 1 => &lines[1..],
        _ => &lines[..],
    };
    body.join("\n")
}

/// The Mermaid block sources of an entry's parsed blocks, in order — the index a
/// [`diagram_outcome`] lookup uses.
fn mermaid_sources(blocks: &[Block], text: &str, mermaid: bool) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Code {
                start,
                end,
                closed,
                lang,
            } if is_mermaid(lang, *closed, mermaid) => {
                Some(code_body(&text[*start..*end], *closed))
            }
            _ => None,
        })
        .collect()
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
    // source characters — the thinking channel's `▼ thinking · …` header or a tool block's
    // head (ADR 0028/4). `text.is_empty()` is the real "blank line"
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
/// revision 3's "the folded one-liner hugs the block it introduces" with one blank line on both
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

/// Whether the `bar` input gets its half-cell padding rows under this glyph tier (ADR 0035
/// revision 3 decision 4, kept by revision 6 for the input alone): `unicode` and `nerd` draw
/// `▄`/`▀`, `ascii` has no half-block glyph and so gets no padding row.
pub fn banded_pad(glyphs: crate::footer::Glyphs) -> bool {
    glyphs != crate::footer::Glyphs::Ascii
}

/// The entries the startup block is made of (ADR 0027 §标志 / the owner's 2026-10-14 startup
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

/// How many display lines an entry's body lays out to (a `Hidden` reasoning entry contributes
/// zero). **Counts only**: no line is materialized, so the frame
/// that measures every entry (the first one, or the first after a resize) does not allocate
/// per entry — the virtualization budget (ADR 0028 R7) depends on this.
pub fn line_count(entry: &Entry, modes: DisplayModes, width: usize) -> usize {
    let mode = modes.reasoning;
    // The `ask_user` block is not a card: its shape follows the card's arguments and result
    // (ADR 0024), and its result entry is blank when it closes the call. Count it
    // through the layout so the total always equals the drawing.
    if is_ask_entry(entry) {
        return layout(entry, &[], modes, width).len();
    }
    if let Some(count) = cheap_count(entry, modes) {
        return count;
    }
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    // A blank edge line is stripped by the layout; the cheap arms below do not know that, so
    // this entry goes through the layout itself (the one place the rule lives). Highlighting
    // is a color-only pass and must not run here: the measure pass is the cheap one
    // (`tui.display.highlight`'s lazy rule). This arm **does** lay Markdown out; an entry with
    // no blank edge counts without it (resume-perf2, `md_line_count`).
    if has_blank_edge(&text) {
        // Keep the caller's glyph tier: the `bar` input's padding rows follow it (ADR 0035
        // revision 3), so the count must use the same one the draw does. Only the costly color
        // pass is skipped.
        let plain = DisplayModes {
            highlight: false,
            ..modes
        };
        return layout(entry, &parse_blocks_for(entry, mode), plain, width).len();
    }
    match entry.kind {
        EntryKind::User => {
            // The thin user row (ADR 0035): just the wrapped text, no padding rows.
            crate::render::wrap_line_count(&text, body_width)
        }
        EntryKind::Reasoning => {
            // The expanded thinking channel (ADR 0028): the header line
            // plus the Markdown body. **Counts only** (resume-perf2): `md_line_count_cached`
            // counts a paragraph without building a line; code blocks count through
            // `code_line_count`, the same mirror the Assistant arm uses.
            let mut count = 1usize;
            for block in parse_blocks_for(entry, mode) {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        count += md_line_count_cached(
                            entry,
                            start,
                            end,
                            body_width,
                            modes.glyphs,
                            &text,
                        );
                    }
                    Block::Code {
                        start, end, closed, ..
                    } => {
                        count += code_line_count(&text[start..end], closed);
                    }
                }
            }
            count
        }
        EntryKind::Assistant => {
            let mut diagram_index = 0;
            parse_blocks_for(entry, mode)
                .iter()
                .map(|block| match block {
                    Block::Paragraph { start, end, .. } => {
                        md_line_count_cached(entry, *start, *end, body_width, modes.glyphs, &text)
                    }
                    Block::Code {
                        start,
                        end,
                        closed,
                        lang,
                    } => {
                        let source = &text[*start..*end];
                        // ADR 0035: the frame lines are counted too
                        // (the layout draws them).
                        let frame = if *closed { 2 } else { 1 };
                        if is_mermaid(lang, *closed, modes.mermaid) {
                            let index = diagram_index;
                            diagram_index += 1;
                            match diagram_outcome(entry, index, &code_body(source, *closed), width)
                            {
                                DiagramOutcome::Rendered(lines) => lines.len() + frame,
                                DiagramOutcome::Failed(_) => 1 + code_line_count(source, *closed),
                            }
                        } else {
                            code_line_count(source, *closed)
                        }
                    }
                })
                .sum()
        }
        // The head line, never wrapped - unless a shell command carries its own newlines (ADR
        // 0028 revision 5 decision 1: a multi-line command wraps, one continuation row per
        // line). The meta and the status glyph are part of the head's own row.
        EntryKind::ToolCall => text.lines().count().max(1),
        EntryKind::ToolResult => {
            let card = entry.tool.as_deref();
            let plan = tool_plan(card, &text, entry.is_error, modes, entry.fold_override);
            plan.lines
                .iter()
                .map(|line| match line {
                    tools::BodyLine::Text { source, .. } => {
                        crate::render::wrap_line_count(&text[source.clone()], body_width)
                    }
                    // A diff row never wraps: one row, one transcript line (ADR 0028
                    // decision 5, R9).
                    tools::BodyLine::Diff(_) => 1,
                })
                .sum()
        }
        _ => crate::render::wrap_line_count(&text, body_width),
    }
}

/// The tool body plan a layout would use (shared by the count-only and the laying-out paths,
/// so they cannot disagree).
fn tool_plan(
    card: Option<&tools::ToolCard>,
    text: &str,
    is_error: bool,
    modes: DisplayModes,
    fold_override: Option<bool>,
) -> tools::BodyPlan {
    let fold = if fold_override.unwrap_or(modes.tools.fold == tools::ToolFold::Expanded) {
        tools::ToolFold::Expanded
    } else {
        modes.tools.fold
    };
    match card {
        Some(card) => tools::body_plan(card, text, is_error, fold, modes.tools.preview_lines),
        // No card data: the raw text, always expanded (a renderer copes with "no card").
        None => tools::body_plan(
            &tools::ToolCard::new("", "", serde_json::Value::Null),
            text,
            is_error,
            tools::ToolFold::Expanded,
            0,
        ),
    }
}

/// How many display lines a fenced code block lays out to: its body's line count plus the
/// frame lines (the fences are markup). Mirrors [`code_lines`]'s skipping exactly.
fn code_line_count(raw: &str, closed: bool) -> usize {
    // The frame is shown (ADR 0035): the top line always, the bottom
    // line when the block is closed.
    let frame = if closed { 2 } else { 1 };
    let lines: Vec<&str> = raw.lines().collect();
    let mut count = lines.len();
    if lines
        .first()
        .is_some_and(|line| fence_open(line.trim()).is_some())
    {
        count = count.saturating_sub(1);
    }
    if closed
        && count > 0
        && lines
            .last()
            .is_some_and(|line| fence_run(line.trim()).is_some())
    {
        count -= 1;
    }
    count.max(1) + frame
}

/// The cached count (the renderer's per-frame path for **every** entry, on screen or not).
pub fn line_count_cached(entry: &mut Entry, modes: DisplayModes, width: usize) -> usize {
    let text_len = entry.text.len();
    let key = (width, text_len, modes, entry.fold_override);
    if let Some(lines) = entry
        .layout_cache
        .as_ref()
        .filter(|(cached, _)| *cached == key)
        .map(|(_, lines)| *lines)
    {
        return lines;
    }
    if entry.kind == EntryKind::Assistant {
        let blocks = blocks_for(entry, modes.reasoning);
        // Render this width's diagrams once and keep them: the key is `(width, text_len)`, so a
        // resize re-renders (a `TooWide` failure can succeed on a wider terminal) and an append
        // re-renders (a longer source is a different diagram).
        if modes.mermaid {
            let text = crate::render::display_text(entry);
            let sources = mermaid_sources(&blocks, &text, modes.mermaid);
            let fresh = entry
                .diagram_cache
                .as_ref()
                .filter(|(cached_width, cached_len, _)| {
                    *cached_width == width && *cached_len == text_len
                })
                .is_none();
            if fresh {
                let outcomes = sources
                    .iter()
                    .map(|source| diagram_for(source, width))
                    .collect();
                entry.diagram_cache = Some((width, text_len, outcomes));
            }
        }
        // The Markdown paragraph counts do not depend on the folds, so cache them by
        // `(width, text_len)`: a global fold toggle re-counts the entries but does not re-parse
        // their Markdown (the R7 toggle gate). **Counts only** (resume-perf2): the first frame
        // must not lay every Assistant paragraph out.
        // The same body width `layout`/`line_count` look up with, or the cache never hits.
        let body_width = width
            .saturating_sub(crate::render::marker_prefix(entry.kind).width())
            .max(1);
        fill_md_count_cache(entry, &blocks, body_width, modes.glyphs);
    }
    let lines = line_count(entry, modes, width);
    entry.layout_cache = Some((key, lines));
    lines
}

/// Whether the entry is part of an `ask_user` pair (either the call or its result): its block
/// has its own shape, so the count goes through the layout.
fn is_ask_entry(entry: &Entry) -> bool {
    entry
        .tool
        .as_deref()
        .is_some_and(|card| card.name == "ask_user")
}

/// Counts that need no layout pass at all.
fn cheap_count(entry: &Entry, modes: DisplayModes) -> Option<usize> {
    // A per-entry override (a search hit, ADR 0028 R6) expands a folded entry — a hidden
    // reasoning channel included: the user asked for the content by searching for it.
    let reasoning_expanded = expanded(entry, modes.reasoning == ReasoningMode::Expanded);
    let tools_expanded = expanded(entry, modes.tools.fold == tools::ToolFold::Expanded);
    match entry.kind {
        EntryKind::Reasoning if !reasoning_expanded => Some(match modes.reasoning {
            ReasoningMode::Hidden => 0,
            _ => 1,
        }),
        EntryKind::Compaction if !tools_expanded => Some(1),
        _ => None,
    }
}

/// The entry's parsed blocks, reusing the entry's cache when the key matches (the streaming
/// path: closed blocks are not re-parsed, ADR 0028 R1). Fresh parse otherwise. Assistant prose
/// and the thinking channel carry fence grammar (ADR 0028: a thinking
/// body lays its code blocks out too).
pub fn blocks_for(entry: &mut Entry, mode: ReasoningMode) -> Vec<Block> {
    if !matches!(entry.kind, EntryKind::Assistant | EntryKind::Reasoning) {
        // Every other kind is one paragraph.
        return Vec::new();
    }
    let key = (entry.text.len(), mode);
    if let Some((cached, blocks)) = &entry.block_cache
        && *cached == key
    {
        return blocks.clone();
    }
    let prev = entry
        .block_cache
        .as_ref()
        .filter(|(cached, _)| cached.1 == mode)
        .map(|(_, blocks)| blocks.as_slice());
    let blocks = parse_blocks_incremental(&entry.text, prev);
    entry.block_cache = Some((key, blocks.clone()));
    blocks
}

/// [`blocks_for`] for an immutable entry (the selection, search and reference paths:
/// discrete events, a fresh parse is fine when the cache misses).
pub(crate) fn parse_blocks_for(entry: &Entry, mode: ReasoningMode) -> Vec<Block> {
    if !matches!(entry.kind, EntryKind::Assistant | EntryKind::Reasoning) {
        return Vec::new();
    }
    if let Some((cached, blocks)) = &entry.block_cache
        && *cached == (entry.text.len(), mode)
    {
        return blocks.clone();
    }
    parse_blocks(&entry.text)
}

/// Lays the entry's body out at `width`. The renderer's spans and the selection's cell →
/// offset mapping both read these lines.
pub fn layout(entry: &Entry, blocks: &[Block], modes: DisplayModes, width: usize) -> Vec<BodyLine> {
    layout_with(
        entry,
        blocks,
        modes,
        width,
        crate::spinner::Frame::default(),
    )
}

/// [`layout`] with the live spinner frame (ADR 0027): the render path passes the
/// frame for the current tick, so a live reasoning line's indicator animates. The line count
/// and the text-only paths use [`layout`], because the glyph never changes the shape.
pub fn layout_with(
    entry: &Entry,
    blocks: &[Block],
    modes: DisplayModes,
    width: usize,
    spinner: crate::spinner::Frame,
) -> Vec<BodyLine> {
    // The container owns the spacing (ADR 0028 rule 1): a text-derived blank line
    // at an entry's start or end is stripped here, so counting, drawing, the selection mapping
    // and the search all see the same shape.
    let mut lines = layout_unstripped(entry, blocks, modes, width, spinner);
    strip_edge_blanks(&mut lines);
    // The `[interrupted]` marker is drawn on the entry's **last** line (ADR 0003, `render.rs`'s
    // `entry_lines` / `visible_lines`): an interrupted reply whose text is empty (cut off during
    // reasoning only) would otherwise lose the whole body here and with it the marker. It keeps
    // one empty line to carry it -- the one blank line the strip above is not allowed to take.
    // It is not a skipped entry either ([`is_empty_assistant`] excludes interrupted ones).
    if lines.is_empty() && entry.interrupted && entry.kind == EntryKind::Assistant {
        lines.push(BodyLine::prose(Vec::new(), 0, String::new()));
    }
    lines
}

/// [`layout`] without the edge stripping.
fn layout_unstripped(
    entry: &Entry,
    blocks: &[Block],
    modes: DisplayModes,
    width: usize,
    spinner: crate::spinner::Frame,
) -> Vec<BodyLine> {
    let mode = modes.reasoning;
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    let reasoning_expanded = expanded(entry, mode == ReasoningMode::Expanded);
    match (entry.kind, mode) {
        (EntryKind::Reasoning, ReasoningMode::Hidden) if !reasoning_expanded => Vec::new(),
        (EntryKind::Reasoning, ReasoningMode::Collapsed) if !reasoning_expanded => {
            vec![BodyLine {
                chars: Vec::new(),
                end: 0,
                text: placeholder(entry, spinner),
                code: false,
                frame: Frame::None,
                role: tools::LineRole::Normal,
                hint: true,
                diagram: None,
                reason: false,
                md: None,
                syntax: None,
            }]
        }
        (EntryKind::Reasoning, _) => {
            // The expanded thinking channel (ADR 0028): a header line,
            // then the body as a **single-colour** Markdown layout behind the `│ ` rail.
            reasoning_body_lines(entry, blocks, modes, width, &text, spinner)
        }
        (EntryKind::User, _) => {
            // The thin user row (ADR 0035/3): `❯ 文本`, no background,
            // no half-cell padding and no rail. Just the wrapped text; the marker column is
            // drawn, not part of the body.
            prose_lines(&text, body_width)
        }
        (EntryKind::Assistant, _) => {
            let mut lines = Vec::new();
            let mut diagram_index = 0;
            for block in blocks {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        lines.extend(md_lines_cached(
                            entry,
                            *start,
                            *end,
                            body_width,
                            modes.glyphs,
                            &text,
                        ));
                    }
                    Block::Code {
                        start,
                        end,
                        closed,
                        lang,
                    } => {
                        let source = &text[*start..*end];
                        let md_glyphs = MdGlyphs::for_glyphs(modes.glyphs);
                        // A Mermaid block is drawn, not highlighted; every other closed block
                        // whose language is in the table is (ADR 0028 R2).
                        let highlight = if is_mermaid(lang, *closed, modes.mermaid) {
                            None
                        } else {
                            syntax_lines_cached(*start, *end, &text, lang, *closed, modes.highlight)
                        };
                        // ADR 0035: the block is an open frame — a top
                        // line titled with the language, a rail on every row, and a bottom
                        // line when the block is closed. The frame is in the marker column.
                        lines.push(code_frame_top(lang, *start, width, &md_glyphs));
                        if is_mermaid(lang, *closed, modes.mermaid) {
                            let index = diagram_index;
                            diagram_index += 1;
                            match diagram_outcome(entry, index, &code_body(source, *closed), width)
                            {
                                DiagramOutcome::Rendered(rendered) => {
                                    lines.extend(
                                        rendered
                                            .into_iter()
                                            .map(|spans| BodyLine::diagram(*end, *start, spans)),
                                    );
                                }
                                DiagramOutcome::Failed(reason) => {
                                    lines.push(BodyLine::reason(
                                        *start,
                                        *start,
                                        format!("mermaid: {reason}"),
                                    ));
                                    lines.extend(code_lines(
                                        source,
                                        *start,
                                        *closed,
                                        width,
                                        &md_glyphs,
                                        highlight.as_deref().map(Vec::as_slice),
                                    ));
                                }
                            }
                        } else {
                            lines.extend(code_lines(
                                source,
                                *start,
                                *closed,
                                width,
                                &md_glyphs,
                                highlight.as_deref().map(Vec::as_slice),
                            ));
                        }
                        // A closed block is a separate line even when it is identical to the
                        // opening (the usual ``` pair).
                        if *closed && source.lines().count() > 1 {
                            lines.push(code_frame_bottom(*start, &md_glyphs));
                        }
                    }
                }
            }
            lines
        }
        (EntryKind::ToolCall, _) => {
            // ADR 0024 R1: an `ask_user` call is the question record, with no
            // answers yet (the result entry carries them).
            match entry.tool.as_deref() {
                // ADR 0024 rules 1-2 / ADR 0028: a pending ask
                // is one weak line - the open frame's top (the questions are drawn in the panel
                // below, not twice); once it is answered, declined or cancelled the **same**
                // entry draws the full open frame. The result entry stays blank (below), so the
                // ask never appears twice.
                Some(card) if card.name == "ask_user" => match entry.result {
                    None => ask_pending_line(card, body_width, modes.glyphs),
                    Some(is_error) => ask_block(card, &text, is_error, body_width, modes.glyphs),
                },
                // The head line (ADR 0028): `• <name> <title> · <meta>
                // <status>`. The entry's text **is** the `<name> <title>` part (set at push
                // time from the same function), so those characters copy; the meta and the
                // status are synthesized.
                _ => head_rows(entry, &text, body_width, modes),
            }
        }
        (EntryKind::Compaction, _) => {
            // ADR 0028 R5: one line when folded, the summary and the user's verbatim words
            // (the entry's own text) when expanded.
            if expanded(entry, modes.tools.fold == tools::ToolFold::Expanded) {
                prose_lines(&text, body_width)
            } else {
                let line = entry
                    .compaction
                    .as_deref()
                    .map(crate::app::CompactionInfo::line)
                    .unwrap_or_else(|| "── context compacted ──".to_string());
                vec![BodyLine {
                    chars: Vec::new(),
                    end: 0,
                    text: line,
                    code: false,
                    frame: Frame::None,
                    role: tools::LineRole::Normal,
                    hint: true,
                    diagram: None,
                    reason: false,
                    md: None,
                    syntax: None,
                }]
            }
        }
        (EntryKind::ToolResult, _) => {
            let card = entry.tool.as_deref();
            // ADR 0024 R1: the result of `ask_user` is the same block, now with the
            // answers (or the declined/cancelled status).
            if let Some(card) = card
                && card.name == "ask_user"
            {
                // ADR 0024: the call entry draws the whole block, so a result that
                // closes it (a merged `Tail`) draws nothing. A torn result whose call is gone
                // has no block above it, so it still draws one.
                return if entry.card == crate::blocks::Card::Tail {
                    Vec::new()
                } else {
                    ask_block(card, &text, entry.is_error, body_width, modes.glyphs)
                };
            }
            let plan = tool_plan(
                entry.tool.as_deref(),
                &text,
                entry.is_error,
                modes,
                entry.fold_override,
            );
            body_plan_lines(&plan.lines, &text, body_width)
        }
        _ => prose_lines(&text, body_width),
    }
}

/// The frame and mark glyphs of the `ask_user` record (ADR 0028 /
/// ADR 0035): an **open** frame - `╭─ <label> ───…` for the top line and
/// the three-cell `╰──` tail, unicode by default and pure ASCII under `glyphs = ascii`. The one
/// marker source for the transcript record **and** the panel (ADR 0024
/// implementation note): single, multi and `Other` share the circle pair (ADR 0028
/// decision 2 - `○`/`◉`, never bold).
pub(crate) struct AskGlyphs {
    pub(crate) top_left: char,
    pub(crate) bottom_left: char,
    pub(crate) horizontal: char,
    pub(crate) vertical: char,
    pub(crate) single_on: &'static str,
    pub(crate) single_off: &'static str,
    pub(crate) multi_on: &'static str,
    pub(crate) multi_off: &'static str,
    pub(crate) desc: &'static str,
}

impl AskGlyphs {
    pub(crate) fn for_glyphs(glyphs: crate::footer::Glyphs) -> Self {
        match glyphs {
            crate::footer::Glyphs::Ascii => Self {
                top_left: '+',
                bottom_left: '+',
                horizontal: '-',
                vertical: '|',
                single_on: "(*)",
                single_off: "( )",
                multi_on: "(*)",
                multi_off: "( )",
                desc: "->",
            },
            crate::footer::Glyphs::Unicode | crate::footer::Glyphs::Nerd => Self {
                top_left: '╭',
                bottom_left: '╰',
                horizontal: '─',
                vertical: '│',
                single_on: "◉",
                single_off: "○",
                multi_on: "◉",
                multi_off: "○",
                desc: "↳",
            },
        }
    }
}

/// A synthesized `ask_user` frame line: no source characters (it neither copies nor matches),
/// styled by `md` pieces. `code` is set so the renderer emits an empty lead and the frame's
/// `╭`/`│`/`╰` sits in the marker column at column 1 (ADR 0035); the
/// block draws its own marker column, so the entry's `->` marker is not used.
fn ask_line(pieces: Vec<(String, MdStyle)>) -> BodyLine {
    let text: String = pieces.iter().map(|(text, _)| text.as_str()).collect();
    let md: Vec<MdPiece> = pieces
        .into_iter()
        .map(|(text, style)| MdPiece {
            text,
            style,
            offset: None,
        })
        .collect();
    BodyLine {
        chars: Vec::new(),
        end: 0,
        text,
        code: true,
        frame: Frame::None,
        role: tools::LineRole::Normal,
        hint: true,
        diagram: None,
        reason: false,
        md: Some(md),
        syntax: None,
    }
}

/// A pending `ask_user` call (ADR 0024 rule 1): one weak line, now the open frame's
/// top line (`╭─ ask · N question(s) · answering below ───…`). The questions live in the panel
/// below until they are answered, so the same ask is never drawn twice. The count comes from
/// the call's own arguments.
fn ask_pending_line(
    card: &tools::ToolCard,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> Vec<BodyLine> {
    let count = card
        .arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let label = format!(
        "ask · {count} question{} · answering below",
        if count == 1 { "" } else { "s" }
    );
    let g = AskGlyphs::for_glyphs(glyphs);
    vec![ask_line(ask_top(&g, width, &label))]
}

/// The `ask_user` record as an open frame (ADR 0028): the request from
/// the call's `arguments`, the answers from the result's `structured`. Three states:
/// unanswered (the call, no structured result), answered, and declined/cancelled (an error
/// result). The content is the ADR 0024 R1 structure (prompt, options, `↳`
/// description, answer/note, status); only the frame is the open one.
fn ask_block(
    card: &tools::ToolCard,
    text: &str,
    is_error: bool,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> Vec<BodyLine> {
    let g = AskGlyphs::for_glyphs(glyphs);
    let questions: Vec<&serde_json::Value> = card
        .arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .map(|questions| questions.iter().collect())
        .unwrap_or_default();
    let answers = card
        .structured
        .as_ref()
        .and_then(|value| value.get("answers"))
        .and_then(serde_json::Value::as_array);
    let mut out = Vec::new();
    // ADR 0028 / ADR 0035: the open frame's top
    // line carries the bare label `ask`; the per-question `[id] · kind · options:N` divider is
    // gone (the prompt, options, `↳` description and answer are the only content).
    out.push(ask_line(ask_top(&g, width, "ask")));
    for question in &questions {
        let id = question
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let kind = question
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("single");
        let options = question
            .get("options")
            .and_then(serde_json::Value::as_array);
        let prompt = question
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // The prompt is the frame's first body line, at the body column (no extra indent).
        out.extend(ask_body(
            &g,
            width,
            (String::new(), MdStyle::Dim),
            (prompt.to_owned(), MdStyle::Accent),
        ));
        let reply = answers.and_then(|answers| {
            answers
                .iter()
                .find(|reply| reply.get("id").and_then(serde_json::Value::as_str) == Some(id))
        });
        let selected: Vec<&str> = reply
            .and_then(|reply| reply.get("selected").and_then(serde_json::Value::as_array))
            .map(|selected| {
                selected
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect()
            })
            .unwrap_or_default();
        if kind != "text" {
            for option in options.map(Vec::as_slice).unwrap_or(&[]) {
                let option_id = option
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let chosen = selected.contains(&option_id);
                let mark = match (kind, chosen) {
                    ("multi", true) => g.multi_on,
                    ("multi", false) => g.multi_off,
                    (_, true) => g.single_on,
                    (_, false) => g.single_off,
                };
                let mut label = option
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if option
                    .get("recommended")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    label.push_str(" (Recommended)");
                }
                // The circle marker sits at the body column (column 3); the label follows.
                out.extend(ask_body(
                    &g,
                    width,
                    (format!("{mark} "), MdStyle::Dim),
                    (label, MdStyle::Plain),
                ));
                if let Some(description) = option
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                {
                    // The `↳` description is indented two cells under the option's label.
                    out.extend(ask_body(
                        &g,
                        width,
                        (format!("  {} ", g.desc), MdStyle::Dim),
                        (description.to_owned(), MdStyle::Muted),
                    ));
                }
            }
        }
        if let Some(reply) = reply {
            for (key, style) in [("other", MdStyle::Muted), ("note", MdStyle::Muted)] {
                if let Some(value) = reply.get(key).and_then(serde_json::Value::as_str) {
                    out.extend(ask_body(
                        &g,
                        width,
                        (format!("  {key}: "), MdStyle::Dim),
                        (value.to_owned(), style),
                    ));
                }
            }
        }
    }
    // Declined/cancelled (an error result) is one status line; the text is the tool failure.
    if is_error {
        let status = text.lines().next().unwrap_or("(no answer)").to_owned();
        out.extend(ask_body(
            &g,
            width,
            (String::new(), MdStyle::Dim),
            (status, MdStyle::Muted),
        ));
    }
    out.push(ask_line(ask_bottom(&g)));
    out
}

/// The open frame's top line (ADR 0028 / ADR 0035):
/// `╭─ <label> ───…`, with the rule running to the body width and **no** right corner. The whole
/// line is the `frame` role (ADR 0029's structural level).
fn ask_top(g: &AskGlyphs, width: usize, label: &str) -> Vec<(String, MdStyle)> {
    let prefix = format!("{}{} ", g.top_left, g.horizontal);
    let mut pieces = vec![
        (prefix.clone(), MdStyle::Frame),
        (label.to_owned(), MdStyle::Frame),
        (" ".to_string(), MdStyle::Frame),
    ];
    let used = UnicodeWidthStr::width(prefix.as_str()) + UnicodeWidthStr::width(label) + 1;
    let fill = width.saturating_sub(used);
    pieces.push((g.horizontal.to_string().repeat(fill), MdStyle::Frame));
    pieces
}

/// Wraps an `ask_user` prompt to `width` display cells (CJK counts 2).
fn ask_wrap(text: &str, width: usize) -> Vec<String> {
    crate::render::wrap_char_offsets(text, width.max(1))
        .into_iter()
        .map(|line| {
            line.chars
                .into_iter()
                .filter_map(|i| text.get(i..).and_then(|r| r.chars().next()))
                .collect()
        })
        .collect()
}

/// One content row of the open frame (ADR 0028 / ADR 0035
/// decision 1): the marker column `│ ` in columns 1-2, `prefix` (the marker or indent, at
/// column 3), and the value wrapped to whatever the prefix leaves of the body width (CJK by
/// display width). A continuation row repeats the prefix's cells so the value's column holds.
/// There is no right border and no trailing padding - the owner's picture leaves the body rows
/// open (a banded theme pads them on screen, which is slice ①'s concern, not this one).
fn ask_body(
    g: &AskGlyphs,
    width: usize,
    prefix: (String, MdStyle),
    value: (String, MdStyle),
) -> Vec<BodyLine> {
    let marker = format!("{} ", g.vertical);
    let inner = width.saturating_sub(UnicodeWidthStr::width(marker.as_str()));
    let prefix_width = UnicodeWidthStr::width(prefix.0.as_str());
    let avail = inner.saturating_sub(prefix_width).max(1);
    ask_wrap(&value.0, avail)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let lead = if index == 0 {
                prefix.0.clone()
            } else {
                " ".repeat(prefix_width)
            };
            ask_line(vec![
                (marker.clone(), MdStyle::Frame),
                (lead, prefix.1),
                (chunk, value.1),
            ])
        })
        .collect()
}

/// The open frame's tail (ADR 0028 / ADR 0035):
/// exactly three cells, `╰──` (ascii `+--`), in the frame role. It does not run to the top
/// line's width.
fn ask_bottom(g: &AskGlyphs) -> Vec<(String, MdStyle)> {
    vec![(
        format!("{}{}{}", g.bottom_left, g.horizontal, g.horizontal),
        MdStyle::Frame,
    )]
}

/// The head row(s) of a tool block (ADR 0028):
/// `• <tool name> <title> · <meta> <status>`. The entry's text is the `<name> <title>` part
/// (the call entry's text, set at push time from the same function) so those characters copy;
/// the name's own style, the meta and the status glyph are synthesized cells (no source).
///
/// One row per line of the entry's text: a multi-line `shell` command wraps with its
/// continuation rows from the body column. The meta and the status sit at the **end** of the
/// head (the last row).
fn head_rows(entry: &Entry, text: &str, width: usize, modes: DisplayModes) -> Vec<BodyLine> {
    let card = entry.tool.as_deref();
    let name = card.map(|card| card.name.as_str()).unwrap_or("");
    let mut meta = card_meta(entry).unwrap_or_default();
    // `collapsed` hides the whole body, so the head says how many rows it hid (the count is
    // the paired result's, which only its own entry lays out: `ToolCard::result_lines`).
    if modes.tools.fold == tools::ToolFold::Collapsed
        && entry.fold_override != Some(true)
        && let Some(lines) = card.and_then(|card| card.result_lines)
        && lines > 0
    {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str(&format!("{lines} lines"));
    }
    let status = head_status(entry, card);
    let (ok, failed) = status_marks(modes.glyphs);
    // The name is the head text's own first word (`call_text` puts it there), so its cells keep
    // their source offsets and the whole head copies.
    let named = !name.is_empty() && text.starts_with(name);
    let lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    let last = lines.len() - 1;
    let mut rows = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let mut cells: Vec<(char, MdStyle, Option<usize>)> = Vec::new();
        for (at, ch) in line.char_indices() {
            let style = if index == 0 && named && at < name.len() {
                MdStyle::ToolName
            } else {
                MdStyle::Plain
            };
            cells.push((ch, style, Some(offset + at)));
        }
        // The tail — the meta and the status glyph — is never truncated away: the title is.
        let mut tail: Vec<(char, MdStyle, Option<usize>)> = Vec::new();
        if index == last {
            if !meta.is_empty() {
                for ch in format!(" · {meta}").chars() {
                    tail.push((ch, MdStyle::Dim, None));
                }
            }
            let glyph = match status {
                HeadStatus::Ok => Some((ok, MdStyle::StatusOk)),
                HeadStatus::Error => Some((failed, MdStyle::StatusError)),
                HeadStatus::Running => Some((running_glyph(modes.glyphs), MdStyle::StatusRunning)),
                HeadStatus::None => None,
            };
            if let Some((glyph, style)) = glyph {
                tail.push((' ', MdStyle::Plain, None));
                tail.push((glyph, style, None));
            }
        }
        rows.push(styled_row(
            cells,
            tail,
            offset + line.len(),
            width,
            Frame::Head,
        ));
        offset += line.len() + 1;
    }
    rows
}

/// A tool call's state as the head's end shows it (ADR 0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadStatus {
    /// The call has no result yet: the live pulse frame.
    Running,
    Ok,
    Error,
    /// A cancelled call: no glyph at all, the meta says `cancelled`.
    None,
}

/// Whether the call succeeded, failed, is still running or was cancelled. A cancelled call is
/// the meta's (`card_meta`), never a glyph.
fn head_status(entry: &Entry, card: Option<&tools::ToolCard>) -> HeadStatus {
    let cancelled = entry.error_code.as_deref() == Some("interrupted")
        || card.is_some_and(|card| {
            card.structured
                .as_ref()
                .and_then(|value| value.get("cancelled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });
    if cancelled {
        return HeadStatus::None;
    }
    match entry.result {
        Some(true) => HeadStatus::Error,
        Some(false) => HeadStatus::Ok,
        None => HeadStatus::Running,
    }
}

/// The status glyphs `✓`/`×` (ADR 0028 with the owner's 2026-10-16 glyph
/// note: `×` replaces the crossed mark U+2717, which misses Cascadia Code, Fira Code and Source
/// Code Pro). `ascii` draws `+`/`x`; the `nerd` tier keeps the footer's Font Awesome pair. The
/// **running** state draws the live spinner frame instead ([`running_glyph`] is the
/// non-animated placeholder the layout carries).
pub(crate) fn status_marks(glyphs: crate::footer::Glyphs) -> (char, char) {
    match glyphs {
        crate::footer::Glyphs::Unicode => ('✓', '×'),
        crate::footer::Glyphs::Nerd => ('\u{f00c}', '\u{f00d}'),
        crate::footer::Glyphs::Ascii => ('+', 'x'),
    }
}

/// The placeholder a running head row carries where the renderer then draws the live frame
/// (ADR 0027/13). Every spinner glyph is one cell, so the head's shape is stable.
fn running_glyph(glyphs: crate::footer::Glyphs) -> char {
    match glyphs {
        crate::footer::Glyphs::Ascii => '*',
        _ => '⏺',
    }
}

/// One synthesized row laid out like a code line (tabs expanded, truncated to `width` with
/// `…`) while each cell keeps its own role and source offset. `tail` is the row's own ending
/// (the head's meta and status glyph): it is appended whole, so a long title truncates but the
/// status never disappears. The head's own builder; the diff rows have theirs ([`diff_line`]).
fn styled_row(
    cells: Vec<(char, MdStyle, Option<usize>)>,
    tail: Vec<(char, MdStyle, Option<usize>)>,
    end: usize,
    width: usize,
    frame: Frame,
) -> BodyLine {
    let tail_width: usize = tail
        .iter()
        .map(|(ch, _, _)| UnicodeWidthChar::width(*ch).unwrap_or(0))
        .sum();
    let body_width = width.saturating_sub(tail_width).max(1);
    let plain: String = cells.iter().map(|(ch, _, _)| *ch).collect();
    let (rendered, kept, _cut) = code_line_layout(&plain, body_width);
    let starts: Vec<usize> = plain.char_indices().map(|(byte, _)| byte).collect();
    let mut pieces: Vec<MdPiece> = Vec::with_capacity(rendered.chars().count() + tail.len());
    let mut cursor = 0usize;
    for (index, ch) in rendered.chars().enumerate() {
        let Some(&byte) = kept.get(index) else {
            // The ellipsis: it takes the last kept cell's role and offset, so a cut tail still
            // copies the characters it stands for (R9).
            if let Some(last) = pieces.last() {
                pieces.push(MdPiece {
                    text: ch.to_string(),
                    style: last.style,
                    offset: last.offset,
                });
            }
            continue;
        };
        while cursor + 1 < cells.len() && starts[cursor] != byte {
            cursor += 1;
        }
        if let Some((_, style, offset)) = cells.get(cursor) {
            pieces.push(MdPiece {
                text: ch.to_string(),
                style: *style,
                offset: *offset,
            });
        }
    }
    pieces.extend(tail.into_iter().map(|(ch, style, offset)| MdPiece {
        text: ch.to_string(),
        style,
        offset,
    }));
    let mut row = BodyLine::md(md_chars(&pieces), end, pieces);
    row.frame = frame;
    row
}

/// The head's meta (ADR 0028): weak, ` · `-separated, only the data that
/// exists. The fields are [`tools::meta_parts`]; `cancelled` is here because the entry's own
/// error code is the paired result's.
fn card_meta(entry: &Entry) -> Option<String> {
    let card = entry.tool.as_deref()?;
    let cancelled = entry.error_code.as_deref() == Some("interrupted");
    let parts = tools::meta_parts(card, cancelled);
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Turns a tool body plan into laid-out rows (ADR 0028's `│ ` body): a
/// text line wraps like prose at the body width (tabs expanded, its characters keeping their
/// source offsets), a diff row is laid out as one line (never wrapped: truncated with `…`,
/// ADR 0028).
fn body_plan_lines(plan: &[tools::BodyLine], text: &str, body_width: usize) -> Vec<BodyLine> {
    let highlights = diff_highlights(plan, text);
    let mut out = Vec::new();
    for (index, line) in plan.iter().enumerate() {
        match line {
            tools::BodyLine::Text { source, role } => {
                let slice = text.get(source.clone()).unwrap_or("");
                for mut laid in prose_lines(slice, body_width) {
                    laid.role = *role;
                    laid.frame = Frame::Body;
                    out.push(laid);
                }
            }
            tools::BodyLine::Diff(row) => {
                let mut row = diff_line(row, highlights[index].as_ref(), text, body_width);
                row.frame = Frame::Body;
                out.push(row);
            }
        }
    }
    out
}

/// One diff row's highlighted code: the highlighted lines of its **run** (consecutive rows of
/// the same language, highlighted as one block so multi-line state carries across them, like a
/// fenced code block) and the row's own line within the run.
struct DiffHighlight {
    lines: std::sync::Arc<Vec<crate::highlight::HighlightedLine>>,
    line: usize,
}

/// Highlights the diff rows' code (the sign stripped), run by run. A row whose language is
/// unknown — or whose side of the diff was removed, so its code is the old file's — gets
/// `None` and draws unhighlighted. Highlighting never changes the text, only the roles.
fn diff_highlights(plan: &[tools::BodyLine], text: &str) -> Vec<Option<DiffHighlight>> {
    let mut out: Vec<Option<DiffHighlight>> = (0..plan.len()).map(|_| None).collect();
    let mut index = 0usize;
    while index < plan.len() {
        let Some(lang) = diff_lang(&plan[index]) else {
            index += 1;
            continue;
        };
        let start = index;
        let mut body = String::new();
        while let Some(row) = plan.get(index).and_then(diff_row) {
            if row.lang != Some(lang) {
                break;
            }
            body.push_str(diff_code(row, text));
            body.push('\n');
            index += 1;
        }
        if let Some(lines) = crate::highlight::highlighted(&body, lang) {
            for (offset, slot) in out[start..index].iter_mut().enumerate() {
                *slot = Some(DiffHighlight {
                    lines: std::sync::Arc::clone(&lines),
                    line: offset,
                });
            }
        }
    }
    out
}

/// The language a plan line's code highlights with, when the line is a diff row that names one.
fn diff_lang(line: &tools::BodyLine) -> Option<&'static str> {
    diff_row(line)?.lang
}

/// The plan line as a diff row, when it is one.
fn diff_row(line: &tools::BodyLine) -> Option<&tools::DiffRow> {
    match line {
        tools::BodyLine::Diff(row) => Some(row),
        _ => None,
    }
}

/// A diff row's code: its text without the sign (everything after the row's first character).
fn diff_code<'a>(row: &tools::DiffRow, text: &'a str) -> &'a str {
    let slice = text.get(row.source.clone()).unwrap_or("");
    let sign_len = slice.chars().next().map_or(0, char::len_utf8);
    &slice[sign_len..]
}

/// Lays one diff row out (ADR 0028): `│ `-free (the card's frame draws the
/// marker column), the line number right-aligned in four cells plus a space, the sign, a space,
/// then the code. The number and the two spaces are **synthesized**: they are drawn but carry no
/// source offset, so a copy of the row is the row's own text — the sign and the code — exactly as
/// it was before. The code never wraps: the row is truncated to `width` with `…` like a code line
/// (R9), and the pieces of a highlighted code line take the syntax roles (the same table a code
/// block uses).
fn diff_line(
    row: &tools::DiffRow,
    highlight: Option<&DiffHighlight>,
    text: &str,
    width: usize,
) -> BodyLine {
    let slice = text.get(row.source.clone()).unwrap_or("");
    let sign_len = slice.chars().next().map_or(0, char::len_utf8);
    let (sign, code) = slice.split_at(sign_len);
    // The gutter is four cells and a space; the numberless fallback has none, and keeps today's
    // form: the sign column, then the code.
    let gutter = row
        .number
        .map(|number| format!("{number:>4} "))
        .unwrap_or_default();
    let gap = if row.number.is_some() { " " } else { "" };
    let plain = format!("{gutter}{sign}{gap}{code}");
    // One role and one source offset per drawn character, in `plain`'s own order.
    let mut cells: Vec<(char, tools::DiffRole, Option<usize>)> =
        Vec::with_capacity(plain.chars().count());
    for ch in gutter.chars() {
        cells.push((ch, tools::DiffRole::Number, None));
    }
    for ch in sign.chars() {
        cells.push((ch, row.kind.sign_role(), Some(row.source.start)));
    }
    for ch in gap.chars() {
        cells.push((ch, row.kind.sign_role(), None));
    }
    // The code's roles: the highlighted piece covering each of its characters, or the row's own
    // code role when the language is unknown (the entry's text is the only source of the text
    // either way — highlighting changes the roles, never the characters).
    let plain_code = tools::DiffRole::Syntax(crate::highlight::SyntaxRole::Plain);
    let mut code_roles: Vec<tools::DiffRole> = Vec::new();
    match highlight.and_then(|highlight| highlight.lines.get(highlight.line)) {
        Some(pieces) => {
            for (text, role) in pieces {
                for _ in text.chars() {
                    code_roles.push(tools::DiffRole::Syntax(*role));
                }
            }
            code_roles.truncate(code.chars().count());
            code_roles.resize(code.chars().count(), plain_code);
        }
        None => code_roles.resize(code.chars().count(), row.kind.code_role()),
    }
    for (index, (offset, ch)) in code.char_indices().enumerate() {
        cells.push((
            ch,
            code_roles[index],
            Some(row.source.start + sign_len + offset),
        ));
    }
    // The layout: the same rule as a code line (tabs expand to four cells, a row that does not
    // fit keeps `width - 1` cells and ends in `…`).
    let (rendered, kept, _cut) = code_line_layout(&plain, width);
    let starts: Vec<usize> = plain.char_indices().map(|(byte, _)| byte).collect();
    let mut pieces: Vec<MdPiece> = Vec::with_capacity(rendered.chars().count());
    let mut cursor = 0usize;
    for (index, ch) in rendered.chars().enumerate() {
        let Some(&byte) = kept.get(index) else {
            // The ellipsis: it takes the last kept cell's role and offset, so the cut tail
            // still copies the characters it stands for (R9).
            if let Some(last) = pieces.last() {
                pieces.push(MdPiece {
                    text: ch.to_string(),
                    style: last.style,
                    offset: last.offset,
                });
            }
            continue;
        };
        while cursor + 1 < cells.len() && starts[cursor] != byte {
            cursor += 1;
        }
        if let Some((_, role, offset)) = cells.get(cursor) {
            pieces.push(MdPiece {
                text: ch.to_string(),
                style: MdStyle::Diff(*role),
                offset: *offset,
            });
        }
    }
    BodyLine::md(md_chars(&pieces), row.source.end, pieces)
}

/// Wraps prose into body lines (the [`crate::render::wrap_char_offsets`] view).
fn prose_lines(text: &str, width: usize) -> Vec<BodyLine> {
    crate::render::wrap_char_offsets(text, width)
        .into_iter()
        .map(|line| {
            let crate::render::WrappedLine { chars, end } = line;
            // A tab renders as four spaces (ADR 0028 R9); each of the four cells maps back to
            // the tab's own offset, so a copy of them yields the source tab. Every cell is
            // `Some(offset)` - these are real source characters - and a read that is out of
            // range (a synthesized index) renders a space rather than panicking, the same
            // defensive read the Markdown path uses.
            let mut body = String::new();
            let mut mapped: Vec<Option<usize>> = Vec::with_capacity(chars.len());
            for &i in &chars {
                let ch = text
                    .get(i..)
                    .and_then(|rest| rest.chars().next())
                    .unwrap_or(' ');
                if ch == '\t' {
                    body.push_str("    ");
                    for _ in 0..4 {
                        mapped.push(Some(i));
                    }
                } else {
                    body.push(ch);
                    mapped.push(Some(i));
                }
            }
            BodyLine::prose(mapped, end, body)
        })
        .collect()
}

/// The reasoning channel's measured wall clock (ADR 0028): the
/// difference between the entry's first-delta instant and its end. `None` for a resumed
/// entry (neither instant was ever stamped) and while the entry is still live (no end yet).
fn reasoning_duration(entry: &Entry) -> Option<String> {
    let started = entry.reasoning_started?;
    let ended = entry.reasoning_ended?;
    Some(crate::timing::format_duration(
        ended.saturating_duration_since(started),
    ))
}

/// The expanded thinking channel's header line: `thinking · <duration>`, `thinking · <spinner
/// frame>` while streaming, and bare `thinking` for a resumed entry with no duration (ADR
/// 0028 revision 5 decision 4). The `▼ ` marker is the marker column's, drawn by the renderer.
fn reasoning_header(entry: &Entry, spinner: crate::spinner::Frame) -> String {
    if entry.live {
        return format!("thinking · {}", spinner.glyph);
    }
    match reasoning_duration(entry) {
        Some(duration) => format!("thinking · {duration}"),
        None => "thinking".to_string(),
    }
}

/// The collapsed reasoning placeholder (ADR 0028): `thinking · <duration>
/// · N lines`, `thinking · <spinner frame>` while streaming, and `thinking · N lines` for a
/// resumed entry. The line count is the entry's own text; the `▶ ` marker is the marker
/// column's (ADR 0028 rule 5), so the body column is the same folded and expanded.
fn placeholder(entry: &Entry, spinner: crate::spinner::Frame) -> String {
    let lines = entry.text.lines().count().max(1);
    if entry.live {
        return format!("thinking · {}", spinner.glyph);
    }
    match reasoning_duration(entry) {
        Some(duration) => format!("thinking · {duration} · {lines} lines"),
        None => format!("thinking · {lines} lines"),
    }
}

/// The expanded thinking channel's body (ADR 0028): the header line,
/// then the entry's Markdown laid out at `body_width`. It is the **same** Markdown layout as
/// an Assistant reply ([`md_lines_cached`], fences through [`code_lines`]) — the thinking
/// channel only differs in how the renderer colours it (one role, no syntax highlighting) and
/// in the `│ ` rail the renderer puts in the marker column. Fenced code blocks keep their
/// structure (the weak fence lines, `code_lines`) but are **never highlighted**.
fn reasoning_body_lines(
    entry: &Entry,
    blocks: &[Block],
    modes: DisplayModes,
    width: usize,
    text: &str,
    spinner: crate::spinner::Frame,
) -> Vec<BodyLine> {
    // The prose rows carry the `│ ` rail, so they lay out two columns narrower; a code block
    // draws its own frame in the marker column (like an Assistant reply), so it uses the full
    // width.
    let body_width = width
        .saturating_sub(crate::render::marker_prefix(EntryKind::Reasoning).width())
        .max(1);
    let mut lines = vec![BodyLine::prose(
        Vec::new(),
        0,
        reasoning_header(entry, spinner),
    )];
    for block in blocks {
        match block {
            Block::Paragraph { start, end, .. } => {
                lines.extend(md_lines_cached(
                    entry,
                    *start,
                    *end,
                    body_width,
                    modes.glyphs,
                    text,
                ));
            }
            Block::Code {
                start,
                end,
                closed,
                lang,
            } => {
                let source = &text[*start..*end];
                let md_glyphs = MdGlyphs::for_glyphs(modes.glyphs);
                // ADR 0035: the same open code frame as an Assistant
                // reply — a top line titled with the language, a rail on every code row, a
                // bottom line when the block is closed. `highlight = None`: the thinking body
                // is never syntax-highlighted (ADR 0028).
                lines.push(code_frame_top(lang, *start, width, &md_glyphs));
                lines.extend(code_lines(source, *start, *closed, width, &md_glyphs, None));
                if *closed && source.lines().count() > 1 {
                    lines.push(code_frame_bottom(*start, &md_glyphs));
                }
            }
        }
    }
    lines
}

/// One code source line laid out for `width`: the rendered text (tab-expanded to 4 spaces,
/// ADR 0028 R9), the char indices of `line` that survived (each expanded cell maps to its
/// source char), and whether an ellipsis replaced a tail. A line that **fits** — including
/// one exactly `width` columns wide — renders as-is; only a line that does not fit keeps
/// `width - 1` columns and ends in `…`. Shared by the offset-mapping layout and the text-only
/// search path, so the two can never show different text.
fn code_line_layout(line: &str, width: usize) -> (String, Vec<usize>, bool) {
    let total: usize = line
        .chars()
        .map(|ch| {
            if ch == '\t' {
                4
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            }
        })
        .sum();
    if total <= width {
        let mut text = String::new();
        let mut chars = Vec::new();
        for (idx, ch) in line.char_indices() {
            if ch == '\t' {
                text.push_str("    ");
            } else {
                text.push(ch);
            }
            // A char may expand to more than one rendered cell (only the tab does); the
            // mapping repeats its source offset once per rendered char.
            let rendered = if ch == '\t' { 4 } else { 1 };
            for _ in 0..rendered {
                chars.push(idx);
            }
        }
        return (text, chars, false);
    }
    // It does not fit: keep what fits before the ellipsis's column.
    let limit = width.saturating_sub(1);
    let mut text = String::new();
    let mut chars = Vec::new();
    let mut used = 0usize;
    for (idx, ch) in line.char_indices() {
        let w = if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if used + w > limit {
            break;
        }
        if ch == '\t' {
            text.push_str("    ");
        } else {
            text.push(ch);
        }
        let rendered = if ch == '\t' { 4 } else { 1 };
        for _ in 0..rendered {
            chars.push(idx);
        }
        used += w;
    }
    text.push('…');
    (text, chars, true)
}

/// One code source line's rendered text (see [`code_line_layout`]).
fn code_line_text(line: &str, width: usize) -> String {
    code_line_layout(line, width).0
}

/// The text of a fenced code block's display lines (the rail and the fences are markup).
/// Mirrors [`code_lines`]: the code is truncated at `width - 2` to leave the two rail columns.
fn code_text_lines(raw: &str, closed: bool, width: usize) -> Vec<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let body = match (lines.first(), lines.last()) {
        (Some(first), Some(last))
            if fence_open(first.trim()).is_some()
                && closed
                && lines.len() > 1
                && fence_run(last.trim()).is_some() =>
        {
            &lines[1..lines.len() - 1]
        }
        (Some(first), _) if fence_open(first.trim()).is_some() && lines.len() > 1 => &lines[1..],
        _ => &lines[..],
    };
    let inner = width.saturating_sub(2).max(1);
    let mut out: Vec<String> = body
        .iter()
        .map(|line| code_line_text(line, inner))
        .collect();
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// The entry's **text** lines only (no per-char offsets): what the search matches against and
/// what the renderer shows. Cheaper than [`layout`] because it never builds the selection
/// mapping, and it takes the same branch structure so the two cannot disagree.
pub fn body_text_lines(entry: &Entry, modes: DisplayModes, width: usize) -> Vec<String> {
    let mode = modes.reasoning;
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    // The stripped edge lines are part of the shape: the text-only path takes them from the
    // layout when there are any (see `has_blank_edge`), so the search geometry and the drawing
    // cannot disagree.
    if has_blank_edge(&text) {
        // Keep the caller's glyph tier (ADR 0035): it decides the band padding, so
        // the text-only path must match the drawing.
        let plain = DisplayModes {
            highlight: false,
            ..modes
        };
        return layout(entry, &parse_blocks_for(entry, mode), plain, width)
            .into_iter()
            .map(|line| line.text)
            .collect();
    }
    let reasoning_expanded = expanded(entry, mode == ReasoningMode::Expanded);
    match (entry.kind, mode) {
        (EntryKind::Reasoning, ReasoningMode::Hidden) if !reasoning_expanded => Vec::new(),
        (EntryKind::Reasoning, ReasoningMode::Collapsed) if !reasoning_expanded => {
            vec![placeholder(entry, crate::spinner::Frame::default())]
        }
        (EntryKind::Reasoning, _) => {
            // The expanded channel's text (ADR 0028): the header, then
            // the Markdown body's text, exactly as [`reasoning_body_lines`] lays it out. The
            // `│ ` rail is the renderer's marker, so it is not part of the text.
            let mut lines = vec![reasoning_header(entry, crate::spinner::Frame::default())];
            for block in parse_blocks_for(entry, mode) {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        lines.extend(
                            md_lines_cached(entry, start, end, body_width, modes.glyphs, &text)
                                .into_iter()
                                .map(|line| line.text),
                        );
                    }
                    Block::Code {
                        start, end, closed, ..
                    } => {
                        let source = &text[start..end];
                        // The code rows' text, exactly as `reasoning_body_lines` lays them out
                        // through `code_lines` (the full width; a code block draws its own
                        // frame). The frame lines are synthesized, absent here as in the
                        // Assistant path.
                        lines.extend(code_text_lines(source, closed, width));
                    }
                }
            }
            lines
        }
        (EntryKind::User, _) => {
            // The thin user row (ADR 0035): just the wrapped text, no padding rows
            // (they were empty text that occupied rows without matching a query).
            crate::render::wrap(&text, body_width)
        }
        (EntryKind::Assistant, _) => {
            let mut lines = Vec::new();
            let mut diagram_index = 0;
            for block in parse_blocks_for(entry, mode) {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        lines.extend(
                            md_lines_cached(entry, start, end, body_width, modes.glyphs, &text)
                                .into_iter()
                                .map(|line| line.text),
                        );
                    }
                    Block::Code {
                        start,
                        end,
                        closed,
                        lang,
                    } => {
                        let source = &text[start..end];
                        if is_mermaid(&lang, closed, modes.mermaid) {
                            let index = diagram_index;
                            diagram_index += 1;
                            match diagram_outcome(entry, index, &code_body(source, closed), width) {
                                DiagramOutcome::Rendered(rendered) => {
                                    lines.extend(rendered.into_iter().map(|spans| {
                                        spans
                                            .into_iter()
                                            .map(|(piece, _)| piece)
                                            .collect::<String>()
                                    }));
                                }
                                DiagramOutcome::Failed(reason) => {
                                    lines.push(format!("mermaid: {reason}"));
                                    lines.extend(code_text_lines(source, closed, width));
                                }
                            }
                        } else {
                            lines.extend(code_text_lines(source, closed, width));
                        }
                    }
                }
            }
            lines
        }
        (EntryKind::ToolCall, _) => head_rows(entry, &text, body_width, modes)
            .into_iter()
            .map(|line| line.text)
            .collect(),
        (EntryKind::Compaction, _) => {
            if expanded(entry, modes.tools.fold == tools::ToolFold::Expanded) {
                crate::render::wrap(&text, body_width)
            } else {
                vec![
                    entry
                        .compaction
                        .as_deref()
                        .map(crate::app::CompactionInfo::line)
                        .unwrap_or_else(|| "── context compacted ──".to_string()),
                ]
            }
        }
        (EntryKind::ToolResult, _) => {
            let plan = tool_plan(
                entry.tool.as_deref(),
                &text,
                entry.is_error,
                modes,
                entry.fold_override,
            );
            let mut content = Vec::new();
            for line in &plan.lines {
                match line {
                    tools::BodyLine::Text { source, .. } => {
                        // The same lines the layout draws (tabs expanded included).
                        let slice = text.get(source.clone()).unwrap_or("");
                        content.extend(prose_lines(slice, body_width).into_iter().map(|l| l.text));
                    }
                    // A diff row never wraps: the same single line the layout draws, with the
                    // same synthesized gutter (and never highlighted: the measure phase must
                    // not highlight, ADR 0028 R2's lazy rule).
                    tools::BodyLine::Diff(row) => {
                        content.push(diff_line(row, None, &text, body_width).text);
                    }
                }
            }
            content
        }
        _ => crate::render::wrap(&text, body_width),
    }
}

/// The code frame's top line (ADR 0035): `╭─ <lang> ───…`, or `╭───…`
/// when the block has no language, filled to `width`. Synthesized: it has no source offset,
/// so it never selects or copies (like the fence it replaces).
fn code_frame_top(lang: &str, start: usize, width: usize, glyphs: &MdGlyphs) -> BodyLine {
    let text = if lang.is_empty() {
        let mut text = String::from(glyphs.code_open);
        text.push_str(&glyphs.dash.to_string().repeat(width.saturating_sub(1)));
        text
    } else {
        let prefix = format!("{}{} {lang} ", glyphs.code_open, glyphs.dash);
        let used = UnicodeWidthStr::width(prefix.as_str());
        let mut text = prefix;
        text.push_str(&glyphs.dash.to_string().repeat(width.saturating_sub(used)));
        text
    };
    frame_line(text, start)
}

/// The code frame's bottom line (ADR 0035): `╰──`.
fn code_frame_bottom(start: usize, glyphs: &MdGlyphs) -> BodyLine {
    frame_line(glyphs.code_bottom.to_owned(), start)
}

/// A synthesized frame line: its own single `Frame` piece, no source offsets.
fn frame_line(text: String, start: usize) -> BodyLine {
    BodyLine {
        chars: Vec::new(),
        end: start,
        text: text.clone(),
        code: true,
        frame: Frame::None,
        role: tools::LineRole::Normal,
        // `hint: true` so `strip_edge_blanks` never treats a frame as a blank edge line.
        hint: true,
        diagram: None,
        reason: false,
        md: Some(vec![MdPiece {
            text,
            style: MdStyle::Frame,
            offset: None,
        }]),
        syntax: None,
    }
}

/// Lays one fenced code block out (ADR 0028 R9, ADR 0035): the fence
/// lines themselves are markup; each source line is one display line, prefixed by the rail
/// `│ ` (the frame's left edge) and starting its code at column 3, tab-expanded (4 spaces)
/// and truncated to `width - 2` with `…`. The `chars` cover the kept characters only — the
/// rail and what is cut cannot be clicked — while `end` still reaches the source line's end,
/// so a selection that spans the line copies the full text. `raw` is the whole block range
/// (fences included) starting at `offset` in the display text.
fn code_lines(
    raw: &str,
    offset: usize,
    closed: bool,
    width: usize,
    glyphs: &MdGlyphs,
    highlight: Option<&[crate::highlight::HighlightedLine]>,
) -> Vec<BodyLine> {
    let lines: Vec<&str> = raw.lines().collect();
    let base = raw.as_ptr() as usize;
    // The first line is the opening fence; when the block is closed, the last line is the
    // closing fence. Both are markup.
    let body = match (lines.first(), lines.last()) {
        (Some(first), Some(last))
            if fence_open(first.trim()).is_some()
                && closed
                && lines.len() > 1
                && fence_run(last.trim()).is_some() =>
        {
            &lines[1..lines.len() - 1]
        }
        (Some(first), _) if fence_open(first.trim()).is_some() && lines.len() > 1 => &lines[1..],
        _ => &lines[..],
    };
    // The rail takes the first two columns, so the code has two fewer.
    let inner = width.saturating_sub(2).max(1);
    let mut out = Vec::new();
    for (index, line) in body.iter().enumerate() {
        // Byte offset of the line's first char within the display text.
        let offset = line.as_ptr() as usize - base + offset;
        let (plain, kept, cut) = code_line_layout(line, inner);
        // The rail's two cells are synthesized, never source: a click or a copy skips them.
        let mut chars: Vec<Option<usize>> = vec![None, None];
        chars.extend(kept.into_iter().map(|idx| Some(offset + idx)));
        if cut {
            // The ellipsis is decoration; map it to the last kept char (the rail's `None` for
            // an empty cut, so the counts still match).
            if let Some(&last) = chars.last() {
                chars.push(last);
            }
        }
        let end = offset + line.len();
        // `styled_line` mirrors `code_line_layout` (same tab expansion, same width rule), so
        // the drawn text is the same either way; only the roles differ. With no highlight the
        // code is one `Plain` piece, so the rail and the code render in order.
        let syntax = match highlight.and_then(|lines| lines.get(index)) {
            Some(hl) => crate::highlight::styled_line(hl, inner),
            None => vec![(plain, crate::highlight::SyntaxRole::Plain)],
        };
        let code: String = syntax.iter().map(|(text, _)| text.as_str()).collect();
        out.push(BodyLine {
            chars,
            end,
            text: format!("{}{}", glyphs.rail, code),
            code: true,
            frame: Frame::None,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: false,
            md: Some(vec![MdPiece {
                text: glyphs.rail.to_owned(),
                style: MdStyle::Frame,
                offset: None,
            }]),
            syntax: Some(syntax),
        });
    }
    if out.is_empty() {
        // An empty code block still shows its rail on one line.
        out.push(frame_line(glyphs.rail.to_owned(), offset));
    }
    out
}

/// A pure fence run (three or more of one marker), i.e. a closing fence.
fn fence_run(trimmed: &str) -> Option<char> {
    let marker = trimmed.chars().next()?;
    if FENCE.contains(&marker)
        && trimmed.chars().take_while(|&c| c == marker).count() >= 3
        && trimmed[trimmed.chars().take_while(|&c| c == marker).count()..]
            .trim()
            .is_empty()
    {
        Some(marker)
    } else {
        None
    }
}

// ---------- Markdown layout (ADR 0028 R2) ----------

/// A piece to lay out: displayed text, role, and the source offset of its first character
/// (`None` for synthesized text).
type Cell = (char, MdStyle, Option<usize>);

/// Lays an Assistant paragraph's Markdown out (ADR 0028 R2). `source` is the paragraph's
/// source range (starting at `base` in the entry's display text). Every piece that exists in
/// the source keeps its offset, so a selection copies the **source** it covers; a synthesized
/// prefix (a bullet, a quote bar, a rule, a table border, a heading's `#`) selects the block.
/// The Markdown glyphs for the footer's glyph set (ADR 0035): the list bullet, the
/// quote bar, the task checkboxes, the rule character and the table borders.
struct MdGlyphs {
    bullet: &'static str,
    quote: &'static str,
    task_unchecked: &'static str,
    task_checked: &'static str,
    rule: char,
    top: (&'static str, &'static str, &'static str),
    mid: (&'static str, &'static str, &'static str),
    bottom: (&'static str, &'static str, &'static str),
    horizontal: &'static str,
    vertical: &'static str,
    /// The code frame (ADR 0035): the top-left corner, the bottom line
    /// (`╰──`) and the rail (`│ `) that prefixes every code row.
    code_open: &'static str,
    code_bottom: &'static str,
    rail: &'static str,
    dash: char,
}

impl MdGlyphs {
    fn for_glyphs(glyphs: crate::footer::Glyphs) -> Self {
        match glyphs {
            crate::footer::Glyphs::Ascii => Self {
                bullet: "- ",
                quote: "| ",
                task_unchecked: "[ ] ",
                task_checked: "[x] ",
                rule: '-',
                top: ("+", "+", "+"),
                mid: ("+", "+", "+"),
                bottom: ("+", "+", "+"),
                horizontal: "-",
                vertical: "|",
                code_open: "+",
                code_bottom: "+--",
                rail: "| ",
                dash: '-',
            },
            // ADR 0035: nerd uses the heavier `│`, unicode the lighter `▏`.
            crate::footer::Glyphs::Nerd => Self {
                bullet: "- ",
                quote: "│ ",
                task_unchecked: "☐ ",
                task_checked: "☑ ",
                rule: '─',
                top: ("┌", "┬", "┐"),
                mid: ("├", "┼", "┤"),
                bottom: ("└", "┴", "┘"),
                horizontal: "─",
                vertical: "│",
                code_open: "╭",
                code_bottom: "╰──",
                rail: "│ ",
                dash: '─',
            },
            crate::footer::Glyphs::Unicode => Self {
                bullet: "- ",
                quote: "▏ ",
                task_unchecked: "☐ ",
                task_checked: "☑ ",
                rule: '─',
                top: ("┌", "┬", "┐"),
                mid: ("├", "┼", "┤"),
                bottom: ("└", "┴", "┘"),
                horizontal: "─",
                vertical: "│",
                code_open: "╭",
                code_bottom: "╰──",
                rail: "│ ",
                dash: '─',
            },
        }
    }
}

pub(crate) fn md_lines(
    source: &str,
    base: usize,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> Vec<BodyLine> {
    MD_LINE_BUILDS.with(|calls| calls.set(calls.get() + 1));
    let blocks = crate::markdown::parse(source, base);
    let glyphs = MdGlyphs::for_glyphs(glyphs);
    let mut out = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        // ADR 0035 V1: exactly one blank line between block-level Markdown elements, managed
        // here and not by the source's blank lines. The trailing blank line (the gap to the
        // next entry block) is restored below.
        if index > 0 {
            out.push(BodyLine::prose(Vec::new(), base, String::new()));
        }
        md_blocks(
            std::slice::from_ref(block),
            width,
            base,
            0,
            &glyphs,
            &mut out,
        );
    }
    // A block range carries its trailing newline (the ranges tile the text); pulldown drops
    // it, so restore it: the gap between this block and the next entry block.
    if source.ends_with('\n') && out.last().is_some_and(|line| !line.text.is_empty()) {
        out.push(BodyLine::prose(
            Vec::new(),
            base + source.len(),
            String::new(),
        ));
    }
    if out.is_empty() {
        out.push(BodyLine::prose(Vec::new(), base, String::new()));
    }
    out
}

/// The Markdown lines of the paragraph at `[start, end)`: the entry's `(width, text_len,
/// glyphs)` cache when it holds them, else rendered fresh. A miss **fills** the cache
/// (resume-perf2: the measure pass counts Markdown without laying it out, so the layout/text
/// paths — the window, the selection and the search — are what fill it). The layout does not
/// depend on the folds, so a fold toggle reuses the cache and stays inside the R7 gate.
fn md_lines_cached(
    entry: &Entry,
    start: usize,
    end: usize,
    width: usize,
    glyphs: crate::footer::Glyphs,
    text: &str,
) -> Vec<BodyLine> {
    let text_len = entry.text.len();
    if let Some((cached_width, cached_len, cached_glyphs, paragraphs)) = &*entry.md_cache.borrow()
        && *cached_width == width
        && *cached_len == text_len
        && *cached_glyphs == glyphs
        && let Some((_, lines)) = paragraphs.iter().find(|(at, _)| *at == start)
    {
        return lines.clone();
    }
    let lines = md_lines(&text[start..end], start, width, glyphs);
    {
        let mut cache = entry.md_cache.borrow_mut();
        let fresh = cache
            .as_ref()
            .is_none_or(|(cached_width, cached_len, cached_glyphs, _)| {
                *cached_width != width || *cached_len != text_len || *cached_glyphs != glyphs
            });
        if fresh {
            *cache = Some((width, text_len, glyphs, vec![(start, lines.clone())]));
        } else if let Some((_, _, _, paragraphs)) = cache.as_mut()
            && !paragraphs.iter().any(|(at, _)| *at == start)
        {
            paragraphs.push((start, lines.clone()));
        }
    }
    lines
}

/// The Markdown line count of the paragraph at `[start, end)`: from the measure pass's count
/// cache, else from the line cache, else **counted without building a line** (resume-perf2).
/// The first frame and a fold toggle take the first or last arm, so neither lays out Markdown
/// for an entry it only needs to measure.
fn md_line_count_cached(
    entry: &Entry,
    start: usize,
    end: usize,
    width: usize,
    glyphs: crate::footer::Glyphs,
    text: &str,
) -> usize {
    let text_len = entry.text.len();
    if let Some((cached_width, cached_len, cached_glyphs, counts)) = &entry.md_count_cache
        && *cached_width == width
        && *cached_len == text_len
        && *cached_glyphs == glyphs
        && let Some((_, count)) = counts.iter().find(|(at, _)| *at == start)
    {
        return *count;
    }
    if let Some((cached_width, cached_len, cached_glyphs, paragraphs)) = &*entry.md_cache.borrow()
        && *cached_width == width
        && *cached_len == text_len
        && *cached_glyphs == glyphs
        && let Some((_, lines)) = paragraphs.iter().find(|(at, _)| *at == start)
    {
        return lines.len();
    }
    md_line_count(&text[start..end], start, width, glyphs)
}

/// Builds the entry's Markdown paragraph **counts** for `width` (the mutable measure pass), so
/// the following `line_count` and a later fold toggle reuse them instead of re-parsing. Counts
/// only: no line is built (resume-perf2, the first-frame cost). The glyph tier is in the key.
fn fill_md_count_cache(
    entry: &mut Entry,
    blocks: &[Block],
    width: usize,
    glyphs: crate::footer::Glyphs,
) {
    let text_len = entry.text.len();
    let fresh = entry
        .md_count_cache
        .as_ref()
        .filter(|(cached_width, cached_len, cached_glyphs, _)| {
            *cached_width == width && *cached_len == text_len && *cached_glyphs == glyphs
        })
        .is_none();
    if !fresh {
        return;
    }
    let text = crate::render::display_text(entry);
    let mut counts = Vec::new();
    for block in blocks {
        if let Block::Paragraph { start, end, .. } = block {
            counts.push((
                *start,
                md_line_count(&text[*start..*end], *start, width, glyphs),
            ));
        }
    }
    entry.md_count_cache = Some((width, text_len, glyphs, counts));
}

/// The number of display lines [`md_lines`] would lay out for `source`, **without building a
/// line** (resume-perf2: the first frame and a fold toggle count Markdown; they must not lay
/// it out). This mirrors `md_lines` exactly — a count that drifts from the layout would give
/// the wrong total and the wrong scroll position — and `md_line_count_matches_the_layout` holds
/// the two together. The glyph tier is passed in: a task list's checkbox and the table rules
/// change the geometry (ADR 0035).
pub(crate) fn md_line_count(
    source: &str,
    base: usize,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> usize {
    let blocks = crate::markdown::parse(source, base);
    let glyphs = MdGlyphs::for_glyphs(glyphs);
    let (mut lines, mut last_non_empty) = (0usize, false);
    for (index, block) in blocks.iter().enumerate() {
        // ADR 0035 V1: exactly one blank line between block-level Markdown elements, managed by
        // `md_lines` and not by the source's blank lines. It counts even when the block itself
        // lays out no line (an empty quote, a lone task marker).
        if index > 0 {
            lines += 1;
            last_non_empty = false;
        }
        let (count, last) = md_block_count(block, width, base, 0, &glyphs);
        if count > 0 {
            lines += count;
            last_non_empty = last;
        }
    }
    // `md_lines` restores the trailing newline pulldown drops as one empty line, unless the
    // layout already ends on an empty line.
    if source.ends_with('\n') && lines > 0 && last_non_empty {
        lines += 1;
    }
    if lines == 0 {
        lines = 1;
    }
    lines
}

/// [`md_line_count`] over a list of parsed blocks: the lines, and whether the **last** line is
/// non-empty (only meaningful when the count is non-zero).
fn md_blocks_count(
    blocks: &[crate::markdown::MdBlock],
    width: usize,
    base: usize,
    indent: usize,
    glyphs: &MdGlyphs,
) -> (usize, bool) {
    let mut count = 0usize;
    let mut last_non_empty = false;
    for block in blocks {
        let (lines, last) = md_block_count(block, width, base, indent, glyphs);
        if lines > 0 {
            count += lines;
            last_non_empty = last;
        }
    }
    (count, last_non_empty)
}

/// The lines one parsed Markdown block lays out to, and whether its last line is non-empty.
/// Mirrors [`md_blocks`] arm by arm, glyph tier included.
fn md_block_count(
    block: &crate::markdown::MdBlock,
    width: usize,
    base: usize,
    indent: usize,
    glyphs: &MdGlyphs,
) -> (usize, bool) {
    use crate::markdown::MdBlock;
    match block {
        MdBlock::Paragraph(pieces) => md_wrap_count(
            pieces.iter().map(|piece| piece.text.as_str()),
            0,
            width,
            indent,
        ),
        MdBlock::Heading { level: _, pieces } => {
            // ADR 0035: the `#` prefix is gone, so no synthesized width
            // precedes the heading text.
            md_wrap_count(
                pieces.iter().map(|piece| piece.text.as_str()),
                0,
                width,
                indent,
            )
        }
        // A task marker on its own draws nothing (ADR 0035); inside an item it is
        // the marker, handled by the `List` arm below.
        MdBlock::TaskMarker { .. } => (0, false),
        MdBlock::List { ordered, items } => {
            // Per item: its lines, or one marker line when it has none. The marker/indent pad
            // makes every emitted line non-empty, so the last line is too.
            let mut count = 0usize;
            for (index, item) in items.iter().enumerate() {
                // The marker width is the layout's: `☐ ` is 2 cells, the ascii `[ ] ` is 4, and
                // `10. ` is 4 (ADR 0035). The body is the item with the task marker
                // dropped, exactly as `md_blocks` drops it.
                let (marker_width, body) = match item.first() {
                    Some(MdBlock::TaskMarker { checked }) => {
                        let marker = if *checked {
                            glyphs.task_checked
                        } else {
                            glyphs.task_unchecked
                        };
                        (UnicodeWidthStr::width(marker), &item[1..])
                    }
                    _ => {
                        let marker = match ordered {
                            Some(first) => format!("{}. ", first + index as u64),
                            None => glyphs.bullet.to_owned(),
                        };
                        (UnicodeWidthStr::width(marker.as_str()), item.as_slice())
                    }
                };
                let (item_lines, _) =
                    md_blocks_count(body, width, base, indent + marker_width, glyphs);
                count += item_lines.max(1);
            }
            (count, count > 0)
        }
        MdBlock::Quote(inner) => {
            // `md_blocks` lays the quote out by the bar's width; each line gets the bar, which
            // makes every emitted line non-empty.
            let marker_width = UnicodeWidthStr::width(glyphs.quote);
            let (count, _) = md_blocks_count(
                inner,
                width.saturating_sub(marker_width).max(1),
                base,
                indent,
                glyphs,
            );
            (count, count > 0)
        }
        MdBlock::Rule => (1, true),
        MdBlock::Table { head, rows } => {
            // Tables are rare and their geometry is intricate: build and count (this path is
            // not the resume corpus; it must stay correct, not fastest). `md_table` now draws a
            // border between every two rows, so the built length is the count.
            let built = md_table(head, rows, base, width, indent, glyphs);
            let count = built.len();
            (
                count,
                built.last().is_some_and(|line| !line.text.is_empty()),
            )
        }
    }
}

/// The rows [`md_wrap`] would produce for `texts` (with a synthesized `prefix_width`-wide
/// prefix), without building a cell or a line. Mirrors `md_wrap` exactly: no space collapsing,
/// a newline forces a row, a space at a soft break is dropped, and the indent narrows the
/// width.
fn md_wrap_count<'a>(
    texts: impl Iterator<Item = &'a str>,
    prefix_width: usize,
    width: usize,
    indent: usize,
) -> (usize, bool) {
    let width = width.saturating_sub(indent).max(1);
    let mut lines = 0usize;
    let mut used = prefix_width;
    let mut row_non_empty = prefix_width > 0;
    let mut trailing_break = false;
    let mut last_non_empty = false;
    for text in texts {
        for ch in text.chars() {
            if ch == '\n' {
                lines += 1;
                last_non_empty = row_non_empty;
                used = 0;
                row_non_empty = false;
                trailing_break = true;
                continue;
            }
            let cell = ascii_width(ch).unwrap_or_else(|| UnicodeWidthChar::width(ch).unwrap_or(0));
            if used + cell > width && row_non_empty {
                lines += 1;
                last_non_empty = row_non_empty;
                used = 0;
                row_non_empty = false;
                trailing_break = false;
                if ch == ' ' {
                    continue;
                }
            }
            used += cell;
            row_non_empty = true;
            trailing_break = false;
        }
    }
    if row_non_empty || lines == 0 || trailing_break {
        lines += 1;
        last_non_empty = row_non_empty;
    }
    (lines, last_non_empty)
}

/// The display width of an ASCII character, or `None` for a non-ASCII one (the caller falls
/// back to `UnicodeWidthChar::width`). `md_wrap` treats a tab as width 0 (`width('\t')` is
/// `None`), unlike the prose [`crate::render::wrap`], which expands it to four.
fn ascii_width(ch: char) -> Option<usize> {
    if ch.is_ascii() {
        Some(usize::from((0x20..0x7f).contains(&(ch as u8))))
    } else {
        None
    }
}

/// The highlighted lines of the closed code block at `[start, end)`, or `None` when
/// highlighting is off, the block is open (streaming), the language is not one we highlight,
/// or the block is over the size cap. The result is a pure function of the body and language,
/// so `highlight`'s **content-keyed** cache is used — no width in the key. This is reached
/// only while laying out a visible line; the measure pass never calls it (a count does not
/// need colors).
fn syntax_lines_cached(
    start: usize,
    end: usize,
    text: &str,
    lang: &str,
    closed: bool,
    highlight: bool,
) -> Option<std::sync::Arc<Vec<crate::highlight::HighlightedLine>>> {
    if !highlight || !closed {
        radian_session::trace::took("first-frame/no-highlight", std::time::Instant::now());
        return None;
    }
    let body = code_body(&text[start..end], closed);
    crate::highlight::highlighted(&body, lang)
}

/// Appends the laid-out lines of `blocks`, indented by `indent` columns.
fn md_blocks(
    blocks: &[crate::markdown::MdBlock],
    width: usize,
    base: usize,
    indent: usize,
    glyphs: &MdGlyphs,
    out: &mut Vec<BodyLine>,
) {
    use crate::markdown::MdBlock;
    for block in blocks {
        match block {
            MdBlock::Paragraph(pieces) => {
                out.extend(md_wrap(
                    md_pieces(pieces, MdStyle::Plain, glyphs),
                    width,
                    indent,
                ));
            }
            MdBlock::Heading { level, pieces } => {
                // ADR 0035: no `#` is drawn. Level 1 is the heading role
                // with an underline, level 2 the heading role, level 3 and below the text role
                // plus bold.
                let text_style = match level {
                    1 => MdStyle::Heading1,
                    2 => MdStyle::Heading,
                    _ => MdStyle::HeadingMinor,
                };
                let line = md_pieces(pieces, text_style, glyphs);
                out.extend(md_wrap(line, width, indent));
            }
            // A task marker is only meaningful as the first block of a task item; on its own
            // it draws nothing.
            MdBlock::TaskMarker { .. } => {}
            MdBlock::List { ordered, items } => {
                for (index, item) in items.iter().enumerate() {
                    let (marker, task) = match item.first() {
                        Some(MdBlock::TaskMarker { checked }) => (
                            if *checked {
                                glyphs.task_checked.to_owned()
                            } else {
                                glyphs.task_unchecked.to_owned()
                            },
                            true,
                        ),
                        _ => (
                            match ordered {
                                Some(first) => format!("{}. ", first + index as u64),
                                None => glyphs.bullet.to_owned(),
                            },
                            false,
                        ),
                    };
                    let body = if task { &item[1..] } else { item };
                    let marker_width = UnicodeWidthStr::width(marker.as_str());
                    let mut item_lines = Vec::new();
                    md_blocks(
                        body,
                        width,
                        base,
                        indent + marker_width,
                        glyphs,
                        &mut item_lines,
                    );
                    let mut first = true;
                    for line in item_lines {
                        if first {
                            // The marker takes the first line's indent columns; the
                            // continuation lines keep the marker's width (ADR 0035:
                            // `10. ` is 4 cells, not 2).
                            let mut pieces = vec![MdPiece {
                                text: " ".repeat(indent),
                                style: MdStyle::Plain,
                                offset: None,
                            }];
                            pieces.push(MdPiece {
                                text: marker.clone(),
                                style: MdStyle::Bullet,
                                offset: None,
                            });
                            pieces.extend(
                                line.md
                                    .clone()
                                    .unwrap_or_default()
                                    .into_iter()
                                    // Drop the line's leading pad: the marker replaces it.
                                    .skip(1),
                            );
                            let chars = md_chars(&pieces);
                            out.push(BodyLine::md(chars, line.end, pieces));
                            first = false;
                        } else {
                            out.push(line);
                        }
                    }
                    if first {
                        out.push(BodyLine::md(
                            vec![None],
                            base,
                            vec![
                                MdPiece {
                                    text: " ".repeat(indent),
                                    style: MdStyle::Plain,
                                    offset: None,
                                },
                                MdPiece {
                                    text: marker,
                                    style: MdStyle::Bullet,
                                    offset: None,
                                },
                            ],
                        ));
                    }
                }
            }
            MdBlock::Quote(inner) => {
                let marker_width = UnicodeWidthStr::width(glyphs.quote);
                let mut inner_lines = Vec::new();
                md_blocks(
                    inner,
                    width.saturating_sub(marker_width).max(1),
                    base,
                    indent,
                    glyphs,
                    &mut inner_lines,
                );
                for line in inner_lines {
                    let mut pieces = vec![MdPiece {
                        text: glyphs.quote.to_owned(),
                        style: MdStyle::QuoteBar,
                        offset: None,
                    }];
                    pieces.extend(
                        line.md
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|piece| MdPiece {
                                style: if piece.style == MdStyle::Plain {
                                    MdStyle::Quote
                                } else {
                                    piece.style
                                },
                                ..piece
                            }),
                    );
                    let chars = md_chars(&pieces);
                    out.push(BodyLine::md(chars, line.end, pieces));
                }
            }
            MdBlock::Rule => out.push(BodyLine::md(
                vec![None],
                base,
                vec![MdPiece {
                    // ADR 0035: the rule is at most 40 cells (omp uses 80).
                    text: glyphs.rule.to_string().repeat(width.clamp(1, 40)),
                    style: MdStyle::Rule,
                    offset: None,
                }],
            )),
            MdBlock::Table { head, rows } => {
                out.extend(md_table(head, rows, base, width, indent, glyphs))
            }
        }
    }
}

fn md_pieces(pieces: &[crate::markdown::Piece], plain: MdStyle, glyphs: &MdGlyphs) -> Vec<MdPiece> {
    pieces
        .iter()
        .map(|piece| {
            let (text, style) = match piece.inline {
                crate::markdown::Inline::Bold => (piece.text.clone(), MdStyle::Bold),
                crate::markdown::Inline::Italic => (piece.text.clone(), MdStyle::Italic),
                crate::markdown::Inline::Code => (piece.text.clone(), MdStyle::Code),
                crate::markdown::Inline::LinkText => (piece.text.clone(), MdStyle::Link),
                crate::markdown::Inline::LinkUrl => (piece.text.clone(), MdStyle::LinkUrl),
                crate::markdown::Inline::Strikethrough => {
                    (piece.text.clone(), MdStyle::Strikethrough)
                }
                // The task checkbox's own text is empty: the renderer supplies the glyph.
                crate::markdown::Inline::TaskUnchecked => {
                    (glyphs.task_unchecked.to_owned(), MdStyle::TaskUnchecked)
                }
                crate::markdown::Inline::TaskChecked => {
                    (glyphs.task_checked.to_owned(), MdStyle::TaskChecked)
                }
                crate::markdown::Inline::Plain => (piece.text.clone(), plain),
            };
            MdPiece {
                text,
                style,
                offset: piece.offset,
            }
        })
        .collect()
}

/// The `chars` mapping for a piece list: a cell from the source maps to its byte offset; a
/// synthesized cell (an indent, a bullet, a heading's `##`, a link's `(url)`) maps to `None`.
fn md_chars(pieces: &[MdPiece]) -> Vec<Option<usize>> {
    let mut out = Vec::new();
    for piece in pieces {
        for (index, ch) in piece.text.char_indices() {
            if ch == '\n' {
                continue;
            }
            out.push(piece.offset.map(|offset| offset + index));
        }
    }
    out
}

/// Wraps Markdown pieces to `width`, one body line per wrapped row, with `indent` leading
/// spaces on every row.
fn md_wrap(pieces: Vec<MdPiece>, width: usize, indent: usize) -> Vec<BodyLine> {
    md_wrap_marked(pieces, width, indent, None)
}

/// Wraps with an optional first-line `marker`: the marker takes the first line's indent, and
/// the continuation lines align to the marker's actual display width (ADR 0035).
fn md_wrap_marked(
    pieces: Vec<MdPiece>,
    width: usize,
    indent: usize,
    marker: Option<MdPiece>,
) -> Vec<BodyLine> {
    let marker_width = marker
        .as_ref()
        .map_or(0, |piece| UnicodeWidthStr::width(piece.text.as_str()));
    let first_pad = " ".repeat(indent);
    let continuation_pad = " ".repeat(indent + marker_width);
    let width = width.saturating_sub(indent + marker_width).max(1);
    // Flatten to cells so wrapping is per character; a newline forces a row break.
    let mut cells: Vec<Cell> = Vec::new();
    for piece in pieces {
        for (index, ch) in piece.text.char_indices() {
            if ch == '\n' {
                cells.push(('\n', piece.style, None));
            } else {
                cells.push((ch, piece.style, piece.offset.map(|offset| offset + index)));
            }
        }
    }
    let mut out = Vec::new();
    let mut row: Vec<Cell> = Vec::new();
    let mut used = 0usize;
    let mut first = true;
    let flush = |row: &mut Vec<Cell>, out: &mut Vec<BodyLine>, first: &mut bool| {
        let mut pieces: Vec<MdPiece> = Vec::new();
        for (ch, style, offset) in row.iter() {
            match pieces.last_mut() {
                Some(piece)
                    if piece.style == *style && piece.offset.is_some() == offset.is_some() =>
                {
                    let contiguous = match (piece.offset, offset) {
                        (Some(a), Some(b)) => a + piece.text.len() == *b,
                        (None, None) => true,
                        _ => false,
                    };
                    if contiguous {
                        piece.text.push(*ch);
                        continue;
                    }
                }
                _ => {}
            }
            pieces.push(MdPiece {
                text: ch.to_string(),
                style: *style,
                offset: *offset,
            });
        }
        let pad = if *first {
            &first_pad
        } else {
            &continuation_pad
        };
        let mut all = vec![MdPiece {
            text: pad.clone(),
            style: MdStyle::Plain,
            offset: None,
        }];
        if *first && let Some(marker) = &marker {
            all.push(marker.clone());
        }
        all.extend(pieces);
        let chars = md_chars(&all);
        let end = row
            .iter()
            .filter_map(|(ch, _, offset)| offset.map(|offset| offset + ch.len_utf8()))
            .max()
            .unwrap_or(0);
        out.push(BodyLine::md(chars, end, all));
        row.clear();
        *first = false;
    };
    let mut trailing_break = false;
    for (ch, style, offset) in cells {
        if ch == '\n' {
            flush(&mut row, &mut out, &mut first);
            used = 0;
            trailing_break = true;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width && !row.is_empty() {
            flush(&mut row, &mut out, &mut first);
            used = 0;
            if ch == ' ' {
                trailing_break = false;
                continue;
            }
        }
        used += w;
        row.push((ch, style, offset));
        trailing_break = false;
    }
    if !row.is_empty() || out.is_empty() || trailing_break {
        flush(&mut row, &mut out, &mut first);
    }
    out
}

/// A table: column widths from the cells, wrapping a cell that does not fit; if even the
/// minimum does not fit (too many columns), the raw `|` source comes back as plain text
/// (ADR 0028 R2's fallback). The header cells take the heading role and the body cells keep
/// their inline styles (ADR 0035, ADR 0028).
/// Every two rows are separated by a border line (ADR 0035).
fn md_table(
    head: &[Vec<crate::markdown::Piece>],
    rows: &[Vec<Vec<crate::markdown::Piece>>],
    base: usize,
    width: usize,
    indent: usize,
    glyphs: &MdGlyphs,
) -> Vec<BodyLine> {
    let cell_text = |cell: &[crate::markdown::Piece]| -> String {
        cell.iter().map(|piece| piece.text.as_str()).collect()
    };
    let columns = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Vec::new();
    }
    let natural = |column: usize| -> usize {
        let head_w = head
            .get(column)
            .map_or(0, |cell| UnicodeWidthStr::width(cell_text(cell).as_str()));
        rows.iter()
            .filter_map(|row| row.get(column))
            .map(|cell| UnicodeWidthStr::width(cell_text(cell).as_str()))
            .chain(std::iter::once(head_w))
            .max()
            .unwrap_or(0)
    };
    let mut widths: Vec<usize> = (0..columns).map(natural).collect();
    let overhead = columns + 1 + columns * 2;
    let available = width.saturating_sub(indent);
    let min_column = 3;
    if overhead + columns * min_column > available {
        return md_table_fallback(head, rows, base, width, indent);
    }
    while widths.iter().sum::<usize>() + overhead > available {
        let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[widest] <= min_column {
            return md_table_fallback(head, rows, base, width, indent);
        }
        widths[widest] -= 1;
    }
    let (left, mid, right) = glyphs.mid;
    let border = |left: &str, mid: &str, right: &str| -> String {
        let mut out = String::from(left);
        for (index, w) in widths.iter().enumerate() {
            out.push_str(&glyphs.horizontal.repeat(w + 2));
            out.push_str(if index + 1 == widths.len() {
                right
            } else {
                mid
            });
        }
        out
    };
    let pad = " ".repeat(indent);
    let border_line = |text: String| -> BodyLine {
        let mut pieces = vec![MdPiece {
            text: pad.clone(),
            style: MdStyle::Plain,
            offset: None,
        }];
        pieces.push(MdPiece {
            text,
            style: MdStyle::TableBorder,
            offset: None,
        });
        BodyLine::md(vec![None; indent + 1], base, pieces)
    };
    let (top_left, top_mid, top_right) = glyphs.top;
    let (bottom_left, bottom_mid, bottom_right) = glyphs.bottom;
    let mut out = vec![border_line(border(top_left, top_mid, top_right))];
    let row_cells: Vec<Vec<&[crate::markdown::Piece]>> = std::iter::once(head)
        .chain(rows.iter().map(Vec::as_slice))
        .map(|row| {
            (0..columns)
                .map(|column| row.get(column).map(Vec::as_slice).unwrap_or(&[]))
                .collect()
        })
        .collect();
    let last_row = row_cells.len().saturating_sub(1);
    for (row_index, row) in row_cells.iter().enumerate() {
        let header = row_index == 0;
        // Each cell's styled pieces, wrapped to its column width. The header's whole cell is
        // the heading role; a body cell keeps its inline roles (ADR 0035 decision
        // 8, ADR 0028).
        let wrapped: Vec<Vec<Vec<MdPiece>>> = row
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let pieces = md_pieces(
                    cell,
                    if header {
                        MdStyle::Heading
                    } else {
                        MdStyle::Plain
                    },
                    glyphs,
                );
                let pieces: Vec<MdPiece> = if header {
                    pieces
                        .into_iter()
                        .map(|piece| MdPiece {
                            style: MdStyle::Heading,
                            ..piece
                        })
                        .collect()
                } else {
                    pieces
                };
                // `md_wrap_marked`'s indent is 0, so its leading pad piece is empty: drop it
                // (and any other empty piece) before the row is assembled.
                md_wrap_marked(pieces, widths[column].max(1), 0, None)
                    .into_iter()
                    .map(|line| {
                        line.md
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|piece| !piece.text.is_empty())
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut pieces = vec![
                MdPiece {
                    text: pad.clone(),
                    style: MdStyle::Plain,
                    offset: None,
                },
                MdPiece {
                    text: glyphs.vertical.to_owned(),
                    style: MdStyle::TableBorder,
                    offset: None,
                },
            ];
            // A header cell's own chrome (the padding around its text) is the heading role
            // too; a body cell's chrome is plain.
            let chrome = if header {
                MdStyle::Heading
            } else {
                MdStyle::Plain
            };
            for (column, column_lines) in wrapped.iter().enumerate() {
                let empty: Vec<MdPiece> = Vec::new();
                let cell = column_lines.get(line_index).unwrap_or(&empty);
                let used = UnicodeWidthStr::width(
                    cell.iter()
                        .map(|piece| piece.text.as_str())
                        .collect::<String>()
                        .as_str(),
                );
                let trailing = widths[column].saturating_sub(used);
                pieces.push(MdPiece {
                    text: " ".to_owned(),
                    style: chrome,
                    offset: None,
                });
                pieces.extend(cell.iter().cloned());
                pieces.push(MdPiece {
                    text: " ".repeat(trailing + 1),
                    style: chrome,
                    offset: None,
                });
                pieces.push(MdPiece {
                    text: glyphs.vertical.to_owned(),
                    style: MdStyle::TableBorder,
                    offset: None,
                });
            }
            out.push(BodyLine::md(vec![None; indent + 1], base, pieces));
        }
        // The header separator and, after every row but the last, another (ADR 0035 revision
        // 1: every two rows are separated).
        if row_index < last_row {
            out.push(border_line(border(left, mid, right)));
        }
    }
    out.push(border_line(border(bottom_left, bottom_mid, bottom_right)));
    out
}

/// The raw `|` source of a table that cannot be drawn (ADR 0028 R2's fallback), as plain
/// wrapped text with real offsets.
fn md_table_fallback(
    head: &[Vec<crate::markdown::Piece>],
    rows: &[Vec<Vec<crate::markdown::Piece>>],
    base: usize,
    width: usize,
    indent: usize,
) -> Vec<BodyLine> {
    let mut text = String::new();
    let mut offsets: Vec<Option<usize>> = Vec::new();
    let push =
        |text: &mut String, offsets: &mut Vec<Option<usize>>, cell: &[crate::markdown::Piece]| {
            for piece in cell {
                for (index, ch) in piece.text.char_indices() {
                    text.push(ch);
                    offsets.push(piece.offset.map(|start| start + index));
                }
            }
        };
    let mut first = true;
    for row in std::iter::once(head).chain(rows.iter().map(Vec::as_slice)) {
        if !first {
            text.push('\n');
            offsets.push(None);
        }
        first = false;
        text.push_str("| ");
        offsets.extend([None, None]);
        for (column, cell) in row.iter().enumerate() {
            if column > 0 {
                text.push_str(" | ");
                offsets.extend([None, None, None]);
            }
            push(&mut text, &mut offsets, cell);
        }
        text.push_str(" |");
        offsets.extend([None, None]);
    }
    // Wrap, keeping each cell's offset.
    let mut out = Vec::new();
    let mut row_text = String::new();
    let mut row_offsets: Vec<Option<usize>> = Vec::new();
    let flush =
        |row_text: &mut String, row_offsets: &mut Vec<Option<usize>>, out: &mut Vec<BodyLine>| {
            if row_text.is_empty() {
                return;
            }
            let mut pieces = vec![MdPiece {
                text: " ".repeat(indent),
                style: MdStyle::Plain,
                offset: None,
            }];
            for (index, ch) in row_text.char_indices() {
                pieces.push(MdPiece {
                    text: ch.to_string(),
                    style: MdStyle::Plain,
                    offset: row_offsets.get(index).copied().flatten(),
                });
            }
            out.push(BodyLine::md(
                row_offsets.clone(),
                row_offsets
                    .last()
                    .copied()
                    .and_then(|offset| offset)
                    .map_or(base + 1, |offset| offset + 1),
                pieces,
            ));
            row_text.clear();
            row_offsets.clear();
        };
    let limit = width.saturating_sub(indent).max(1);
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            flush(&mut row_text, &mut row_offsets, &mut out);
            used = 0;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > limit && !row_text.is_empty() {
            flush(&mut row_text, &mut row_offsets, &mut out);
            used = 0;
        }
        used += w;
        row_text.push(ch);
        row_offsets.push(offsets.get(index).copied().flatten());
    }
    flush(&mut row_text, &mut row_offsets, &mut out);
    out
}

/// The text lines a tool card's body lays out to, for tests that need the body geometry
/// without building an `Entry`.
#[cfg(test)]
pub(crate) fn body_text_lines_for_test(
    card: &tools::ToolCard,
    text: &str,
    width: usize,
) -> Vec<String> {
    let entry = Entry::new(EntryKind::ToolResult, text.to_owned());
    let _ = card;
    body_text_lines(&entry, DisplayModes::default(), width)
}

/// The laid-out body lines of a tool card, for tests that need the mapping.
#[cfg(test)]
pub(crate) fn layout_for_test(_card: &tools::ToolCard, text: &str, width: usize) -> Vec<BodyLine> {
    let entry = Entry::new(EntryKind::ToolResult, text.to_owned());
    layout(&entry, &[], DisplayModes::default(), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolDisplay, ToolFold};

    /// Both folds expanded: a geometry test that does not want the fold defaults.
    const EXPANDED: DisplayModes = DisplayModes {
        reasoning: ReasoningMode::Expanded,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
        highlight: true,
        glyphs: crate::footer::Glyphs::Unicode,
    };

    /// The given reasoning mode with the tools expanded.
    fn modes_with(reasoning: ReasoningMode) -> DisplayModes {
        DisplayModes {
            glyphs: crate::footer::Glyphs::Unicode,
            reasoning,
            ..EXPANDED
        }
    }

    fn assistant(text: &str) -> Entry {
        Entry::new(EntryKind::Assistant, text)
    }

    /// A corpus that exercises every `MdBlock` arm `md_line_count` mirrors, plus the edge
    /// cases `md_lines` post-processes (a trailing newline, an empty source).
    const MARKDOWN_SOURCES: &[&str] = &[
        "",
        "\n",
        "plain prose with several words that will wrap at a narrow width\n",
        "# Heading one\n\nbody text under it\n",
        "- item one\n- item two\n  - nested item\n",
        "1. first\n2. second\n10. tenth\n",
        // ADR 0035: a task list, whose `☐ `/`☑ ` (ascii `[ ] `/`[x] `) markers
        // change width with the glyph tier.
        "- [ ] todo\n- [x] done\n- [ ] a longer task that wraps at a narrow width\n",
        "> a quoted line that is long enough to wrap\n",
        "---\n",
        "| column a | column b |\n| --- | --- |\n| value one | value two |\n",
        // ADR 0035: every two rows get a border now, not only the header.
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n",
        // ADR 0035: every heading level (no `#` prefix, three roles).
        "# One\n\n## Two\n\n### Three\n\n###### Six\n",
        // ADR 0035: a table whose cells carry inline styles.
        "| `code` | b |\n| --- | --- |\n| **bold** | plain |\n",
        // The HTML subset: `<br>` is a break, `<hr>` a rule, `<blockquote>` a quote.
        "a<br>b\n\n<hr>\n\n<blockquote>q</blockquote>\n",
        "text with a [link](https://example.com/a/b) and **bold** and `code`\n",
        "line one\nline two\nline three\n",
        "a\n\n\nb\n",
        "unbrokenverylongsinglewordthatmusthardsplitacrosslines\n",
        "这是一段中文说明，用来测试在终端宽度下的换行与显示宽度计算是否正确。\n",
        "a table too wide | for the | terminal | to draw | here\n|---|---|---|---|\n|1|2|3|4|\n",
        "tab\there and a control \x07 char\n",
        "**bold** then *italic* then `code` then normal\n",
        "```\nnot a fence in md_lines\n```\n",
    ];

    /// resume-perf2: the measure pass counts Markdown; the count must equal the layout's line
    /// count exactly, or the total (and the scroll position) would drift. The window-vs-full
    /// test in `tests/virtual.rs` covers the entry path; this one pins the mirrored count
    /// function against `md_lines` directly, across every block shape, width and glyph tier.
    /// The user row is the thin one now (ADR 0035: no padding rows), so its count
    /// is the wrapped text at every tier — the entry-level `line_count` must agree.
    #[test]
    fn md_line_count_matches_the_layout() {
        for glyphs in [
            crate::footer::Glyphs::Unicode,
            crate::footer::Glyphs::Nerd,
            crate::footer::Glyphs::Ascii,
        ] {
            for source in MARKDOWN_SOURCES {
                for width in [2usize, 4, 10, 20, 40, 80, 120] {
                    assert_eq!(
                        md_line_count(source, 0, width, glyphs),
                        md_lines(source, 0, width, glyphs).len(),
                        "source {source:?} width {width} glyphs {glyphs:?}"
                    );
                }
            }
        }
        // The thin user row: the entry-level counter (what the measure pass runs) equals the
        // layout under every tier, with no padding rows.
        for glyphs in [
            crate::footer::Glyphs::Unicode,
            crate::footer::Glyphs::Nerd,
            crate::footer::Glyphs::Ascii,
        ] {
            let modes = DisplayModes { glyphs, ..EXPANDED };
            let entry = Entry::new(EntryKind::User, "a thin user row that wraps a little");
            for width in [8usize, 12, 30, 80] {
                assert_eq!(
                    line_count(&entry, modes, width),
                    layout(&entry, &[], modes, width).len(),
                    "user row width {width} glyphs {glyphs:?}"
                );
            }
        }
    }

    /// resume-perf2: the frame that measures every entry (the first frame, and the one after a
    /// fold toggle) must not lay Markdown out. The counter is the proof; a visible window does
    /// lay it out. (The highlight slice's `the_measure_pass_never_highlights` is the same
    /// shape.) The text has no blank edge: an entry with one takes `line_count`'s layout
    /// fallback (the edge-stripping rule lives in the layout), which does lay Markdown out.
    #[test]
    fn the_measure_pass_never_lays_out_markdown() {
        let theme = crate::theme::Theme::dark();
        let modes = modes_with(ReasoningMode::Collapsed);
        let mut entries: Vec<Entry> = (0..8)
            .map(|i| {
                assistant(&format!(
                    "# Heading {i}\n\nA paragraph with **bold** and `code` and enough words to \
                     wrap at the width.\n\n- one\n- two\n  - nested\n\n> a quote"
                ))
            })
            .collect();
        let before = md_line_builds();
        // Height 0: measure only, no visible line is laid out.
        let (total, lines) = crate::render::visible_lines(&mut entries, modes, 80, 0, 0, &theme);
        assert!(total > 0 && lines.is_empty(), "measured {total}");
        assert_eq!(
            md_line_builds(),
            before,
            "the measure pass must not lay Markdown out"
        );
        // A visible window does lay its entries out.
        let _ = crate::render::visible_lines(&mut entries, modes, 80, 0, 20, &theme);
        assert!(
            md_line_builds() > before,
            "laying a visible entry out must lay its Markdown out"
        );
    }

    /// resume-perf2: the Markdown line cache is filled by the layout path (the measure pass no
    /// longer fills it), so a repeat frame at the same width reuses it instead of re-parsing.
    #[test]
    fn a_repeat_frame_reuses_the_markdown_line_cache() {
        let theme = crate::theme::Theme::dark();
        let width = 60;
        let mut entries = vec![assistant(
            "# Title\n\nbody words that wrap over a couple of lines at this width\n\n> quote",
        )];
        let _ = crate::render::visible_lines(&mut entries, EXPANDED, width, 0, 20, &theme);
        let after_first = md_line_builds();
        let _ = crate::render::visible_lines(&mut entries, EXPANDED, width, 0, 20, &theme);
        assert_eq!(
            md_line_builds(),
            after_first,
            "the second frame must reuse the Markdown line cache"
        );
    }

    /// resume-perf2 / R7: a fold toggle re-measures every entry. The **count** cache (width and
    /// text length, not the folds) must carry the Markdown counts across the toggle, so the
    /// toggle does not re-parse Markdown.
    #[test]
    fn a_fold_toggle_reuses_the_markdown_counts() {
        let theme = crate::theme::Theme::dark();
        let mut entries = vec![assistant(
            "# Title\n\nbody words that wrap over a couple of lines at this width",
        )];
        let _ = crate::render::visible_lines(
            &mut entries,
            modes_with(ReasoningMode::Expanded),
            80,
            0,
            0,
            &theme,
        );
        let after_measure = md_line_builds();
        let _ = crate::render::visible_lines(
            &mut entries,
            modes_with(ReasoningMode::Collapsed),
            80,
            0,
            0,
            &theme,
        );
        assert_eq!(
            md_line_builds(),
            after_measure,
            "a fold toggle must reuse the Markdown counts, not lay Markdown out"
        );
    }

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
    fn a_blank_edge_line_is_stripped() {
        // A text-derived blank line at either edge is the container's job.
        let mut text = Entry::new(EntryKind::Assistant, "\nhello\n\n");
        assert_eq!(
            layout(&text, &parse_blocks("\nhello\n\n"), EXPANDED, 20).len(),
            1
        );
        assert_eq!(line_count(&text, EXPANDED, 20), 1);
        assert_eq!(
            body_text_lines(&text, EXPANDED, 20),
            vec!["hello".to_string()]
        );
        // The thin user row (ADR 0035) has no padding rows either: the blank edge
        // lines are stripped the same way, leaving just the text.
        text.kind = EntryKind::User;
        text.text = "\nhi\n\n".to_string();
        let lines = layout(&text, &[], EXPANDED, 20);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].text, "hi");
        assert!(!lines[0].hint);
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

    #[test]
    fn a_code_line_never_wraps_and_truncates_with_an_ellipsis() {
        let entry = assistant("```\nshort\nabc def gh ijk\n```");
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 10);
        // The top frame, the body's two rows (each `│ ` plus code) and the bottom frame.
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].text, "╭─────────", "the top frame, no language");
        assert_eq!(lines[1].text, "│ short");
        assert_eq!(
            lines[2].text, "│ abc def…",
            "rail + code cut at 8 columns + …"
        );
        assert_eq!(lines[3].text, "╰──", "the bottom frame");
        assert_eq!(lines[2].chars.len(), lines[2].text.chars().count());
        assert!(
            lines[2].chars[..2].iter().all(|c| c.is_none()),
            "the rail is synthesized: {:?}",
            lines[2].chars
        );
        // The gap still reaches the source line's end (byte offsets are into the entry
        // text, fences included): a selection across it copies all.
        let start = entry.text.find("abc").expect("the line is in the text");
        assert_eq!(lines[2].end, start + "abc def gh ijk".len());
    }

    #[test]
    fn tabs_expand_to_four_spaces_and_map_to_the_tab_offset() {
        let entry = assistant("```\na\tb\n```");
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 20);
        assert_eq!(lines[1].text, "│ a    b");
        let base = entry.text.find('a').expect("the line is in the text");
        assert_eq!(
            lines[1].chars,
            vec![
                None,
                None,
                Some(base),
                Some(base + 1),
                Some(base + 1),
                Some(base + 1),
                Some(base + 1),
                Some(base + 2)
            ]
        );
    }

    #[test]
    fn a_code_line_that_fits_exactly_is_not_truncated() {
        // The code's own width is `width - 2` (the rail takes the first two columns).
        // Exactly that: it fits, so no ellipsis (the `…` costs a column).
        let entry = assistant(&format!("```\n{}\n```", "x".repeat(20)));
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 22);
        assert_eq!(lines[1].text, format!("│ {}", "x".repeat(20)));
        assert!(!lines[1].text.contains('…'));
        // One over: it does not fit; keep width-1 and mark the cut.
        let entry = assistant(&format!("```\n{}\n```", "x".repeat(21)));
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 22);
        assert_eq!(lines[1].text, format!("│ {}…", "x".repeat(19)));
        // The mapping still reaches the source line's end either way (the offset is into the
        // entry text, which starts with the "```\n" fence line).
        assert_eq!(lines[1].end, "```\n".len() + 21);
    }

    #[test]
    fn cjk_lines_follow_the_same_fit_rule() {
        // 6 code columns at width 8 (inner 6): as-is.
        let entry = assistant("```\n日本語\n```");
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 8);
        assert_eq!(lines[1].text, "│ 日本語");
        // 6 code columns at width 7 (inner 5): keep 4 columns (2 wide chars) and the ellipsis.
        let lines = layout(&entry, &blocks, EXPANDED, 7);
        assert_eq!(lines[1].text, "│ 日本…");
        assert_eq!(lines[1].chars.len(), lines[1].text.chars().count());
        // 6 code columns at width 9 (inner 7): as-is.
        let lines = layout(&entry, &blocks, EXPANDED, 9);
        assert_eq!(lines[1].text, "│ 日本語");
    }

    #[test]
    fn cjk_widths_count_in_the_truncation() {
        let entry = assistant("```\n日本語テキスト\n```");
        let blocks = parse_blocks(&entry.text);
        // width 9 → inner 7: 日(2)本(2)語(2) = 6, テ would exceed → cut after 語.
        let lines = layout(&entry, &blocks, EXPANDED, 9);
        assert_eq!(lines[1].text, "│ 日本語…");
    }

    /// ADR 0035: the block is an open frame — a top line titled with
    /// the language, a rail on every row (the code from column 3), and a bottom line.
    #[test]
    fn a_code_block_draws_an_open_frame() {
        let mut entry = Entry::new(EntryKind::Assistant, "```rust\nlet x = 1;\n```\n");
        let blocks = blocks_for(&mut entry, ReasoningMode::Collapsed);
        let lines = layout(&entry, &blocks, DisplayModes::default(), 40);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(
            texts[0].starts_with("╭─ rust "),
            "the top line is titled with the language: {texts:?}"
        );
        assert_eq!(
            UnicodeWidthStr::width(texts[0]),
            40,
            "the top line fills the width: {texts:?}"
        );
        assert!(
            texts.contains(&"│ let x = 1;"),
            "the code row carries the rail and starts at column 3: {texts:?}"
        );
        assert!(texts.contains(&"╰──"), "the bottom frame: {texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("```")),
            "no fence markup is drawn: {texts:?}"
        );
    }

    /// The `ascii` glyph tier's code frame (ADR 0035): `+-` opens,
    /// `|` rails and `+--` closes.
    #[test]
    fn the_ascii_code_frame_uses_plus_and_pipe() {
        let entry = assistant("```\ncode\n```");
        let blocks = parse_blocks(&entry.text);
        let modes = DisplayModes {
            glyphs: crate::footer::Glyphs::Ascii,
            ..EXPANDED
        };
        let lines = layout(&entry, &blocks, modes, 20);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts[0],
            format!("+{}", "-".repeat(19)),
            "the ascii top frame, no language: {texts:?}"
        );
        assert!(texts.contains(&"| code"), "the ascii rail: {texts:?}");
        assert!(texts.contains(&"+--"), "the ascii bottom frame: {texts:?}");
    }

    #[test]
    fn a_user_block_has_no_padding_rows_at_any_glyph_tier() {
        // ADR 0035/3: the user row is `❯ 文本` with no background and
        // no half-cell padding — the block is just the text under every tier.
        let entry = Entry::new(EntryKind::User, "hello");
        for glyphs in [
            crate::footer::Glyphs::Unicode,
            crate::footer::Glyphs::Nerd,
            crate::footer::Glyphs::Ascii,
        ] {
            let modes = DisplayModes { glyphs, ..EXPANDED };
            let lines = layout(&entry, &[], modes, 30);
            assert_eq!(lines.len(), 1, "{glyphs:?}: {lines:?}");
            assert_eq!(lines[0].text, "hello");
            assert!(!lines[0].hint, "{glyphs:?}");
            assert_eq!(line_count(&entry, modes, 30), 1, "{glyphs:?}");
        }
    }

    #[test]
    fn reasoning_modes_change_the_line_count() {
        let mut entry = Entry::new(EntryKind::Reasoning, "one\ntwo\nthree");
        entry.live = true;
        assert_eq!(
            line_count(&entry, modes_with(ReasoningMode::Collapsed), 40),
            1
        );
        assert_eq!(line_count(&entry, modes_with(ReasoningMode::Hidden), 40), 0);
        // Expanded, the channel is the header plus the three Markdown lines (ADR 0028
        // revision 5 decision 4).
        assert_eq!(line_count(&entry, EXPANDED, 40), 4);
        // The collapsed placeholder carries the spinner while streaming, not the line count.
        let lines = layout(&entry, &[], modes_with(ReasoningMode::Collapsed), 40);
        assert!(lines[0].hint, "the placeholder is a synthesized line");
        assert_eq!(
            lines[0].text,
            format!("thinking · {}", crate::spinner::Frame::default().glyph)
        );
        // On a resumed entry (no clock) the line count shows and there is no duration.
        entry.live = false;
        let lines = layout(&entry, &[], modes_with(ReasoningMode::Collapsed), 40);
        assert_eq!(lines[0].text, "thinking · 3 lines");
        // The expanded header is a harness line of its own; the `▼ ` is the marker column's.
        let expanded = layout(&entry, &parse_blocks(&entry.text), EXPANDED, 40);
        assert_eq!(expanded.len(), 4);
        assert_eq!(expanded[0].text, "thinking");
        assert_eq!(expanded[1].text, "one");
        assert!(
            expanded[1..].iter().all(|line| !line.hint),
            "the body is real content"
        );
    }
    // ---- ADR 0035: the Markdown elements ----

    fn md(source: &str, width: usize, glyphs: crate::footer::Glyphs) -> Vec<String> {
        md_lines(source, 0, width, glyphs)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    /// Block-level elements are separated by exactly one blank line, whatever the source.
    #[test]
    fn markdown_elements_have_one_blank_line_between_them() {
        let texts = md(
            "# H\n\npara\n\n- one\n\n> q\n\n---\n",
            40,
            crate::footer::Glyphs::Unicode,
        );
        assert!(
            texts
                .windows(2)
                .all(|w| !(w[0].is_empty() && w[1].is_empty())),
            "{texts:?}"
        );
        let h = texts.iter().position(|t| t.contains('H')).unwrap();
        let p = texts.iter().position(|t| t.contains("para")).unwrap();
        assert_eq!(p, h + 2, "exactly one blank between blocks: {texts:?}");
        // A source with no blank lines still gets the spacing.
        let tight = md(
            "para one\n# H\npara two\n",
            40,
            crate::footer::Glyphs::Unicode,
        );
        let h = tight.iter().position(|t| t.contains('H')).unwrap();
        assert!(tight[h - 1].is_empty(), "{tight:?}");
        assert!(tight[h + 1].is_empty(), "{tight:?}");
    }

    /// The unordered marker is `-`, and a continuation aligns to the marker's actual width.
    #[test]
    fn lists_use_a_dash_and_align_continuations_to_the_marker() {
        let texts = md(
            "- one two three four five six\n",
            12,
            crate::footer::Glyphs::Unicode,
        );
        assert!(texts[0].starts_with("- one"), "{texts:?}");
        assert!(texts[1].starts_with("  "), "{texts:?}");
        let ordered = md("10. aaa bbb ccc ddd\n", 12, crate::footer::Glyphs::Unicode);
        assert!(ordered[0].starts_with("10. aaa"), "{ordered:?}");
        assert!(
            ordered[1].starts_with("    "),
            "`10. ` is 4 cells: {ordered:?}"
        );
    }

    /// Task lists render `☐`/`☑` (ascii `[ ]`/`[x]`) instead of the bullet.
    #[test]
    fn task_lists_render_checkboxes() {
        let texts = md(
            "- [ ] todo\n- [x] done\n",
            40,
            crate::footer::Glyphs::Unicode,
        );
        assert!(texts.iter().any(|t| t.contains("☐ todo")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("☑ done")), "{texts:?}");
        let ascii = md("- [ ] todo\n", 40, crate::footer::Glyphs::Ascii);
        assert!(ascii.iter().any(|t| t.contains("[ ] todo")), "{ascii:?}");
    }

    /// The quote bar and the rule follow the glyph set.
    #[test]
    fn quotes_and_rules_use_the_glyphs() {
        let texts = md("> quoted\n\n---\n", 40, crate::footer::Glyphs::Unicode);
        assert!(texts.iter().any(|t| t.contains("▏ quoted")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains('─')), "{texts:?}");
        let ascii = md("> quoted\n\n---\n", 40, crate::footer::Glyphs::Ascii);
        assert!(ascii.iter().any(|t| t.contains("| quoted")), "{ascii:?}");
        assert!(ascii.iter().any(|t| t.contains('-')), "{ascii:?}");
        let nerd = md("> quoted\n", 40, crate::footer::Glyphs::Nerd);
        assert!(nerd.iter().any(|t| t.contains("│ quoted")), "{nerd:?}");
    }

    /// A table has a border between every two rows, not only under the header.
    #[test]
    fn a_table_has_a_border_between_every_row() {
        let texts = md(
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n",
            40,
            crate::footer::Glyphs::Unicode,
        );
        assert_eq!(
            texts.iter().filter(|t| t.contains('├')).count(),
            2,
            "{texts:?}"
        );
        assert!(texts[0].contains('┌'), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains('└')), "{texts:?}");
        let ascii = md(
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n",
            40,
            crate::footer::Glyphs::Ascii,
        );
        assert!(!ascii.iter().any(|t| t.contains('┌')), "{ascii:?}");
    }

    /// HTML: `<br>` is a break, `<hr>` a rule, `<blockquote>` a quote; other tags are stripped.
    #[test]
    fn html_br_hr_and_blockquote_render() {
        let texts = md(
            "a<br>b\n\n<hr>\n\n<blockquote>q</blockquote>\n",
            40,
            crate::footer::Glyphs::Unicode,
        );
        assert!(texts.iter().any(|t| t == "a"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "b"), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains('─')), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("▏ q")), "{texts:?}");
        // A span's text is kept, its tags stripped.
        let stripped = md(
            "a <span class=\"x\">b</span> c\n",
            40,
            crate::footer::Glyphs::Unicode,
        );
        assert!(stripped.iter().any(|t| t.contains("a b c")), "{stripped:?}");
    }

    /// The synthesized Markdown decorations (bullet, checkbox, quote bar) have no source
    /// offset, so a copy never includes them (the real text does).
    #[test]
    fn markdown_decorations_do_not_copy() {
        let lines = md_lines(
            "- one\n- [ ] todo\n\n> quoted\n",
            0,
            40,
            crate::footer::Glyphs::Unicode,
        );
        let mut found = 0;
        for line in &lines {
            for piece in line.md.as_deref().unwrap_or_default() {
                if ["- ", "☐ ", "▏ "].contains(&piece.text.as_str()) {
                    assert!(piece.offset.is_none(), "a decoration copies: {piece:?}");
                    found += 1;
                }
            }
        }
        assert_eq!(found, 3, "the bullet, the checkbox and the quote bar");
    }

    /// ADR 0035: a heading shows no `#`; level 1 is `Heading1` (the
    /// heading role plus underline), level 2 is `Heading` (the heading role), level 3 and
    /// below are `HeadingMinor` (the text role plus bold).
    #[test]
    fn headings_have_no_hash_and_three_roles() {
        for (source, expected) in [
            ("# H\n", MdStyle::Heading1),
            ("## H\n", MdStyle::Heading),
            ("### H\n", MdStyle::HeadingMinor),
            ("###### H\n", MdStyle::HeadingMinor),
        ] {
            let lines = md_lines(source, 0, 40, crate::footer::Glyphs::Unicode);
            let pieces = lines[0].md.as_ref().expect("md pieces");
            assert!(
                pieces.iter().all(|p| !p.text.contains('#')),
                "{source:?}: no `#` is drawn: {pieces:?}"
            );
            assert_eq!(
                pieces.last().map(|p| p.style),
                Some(expected),
                "{source:?}: {pieces:?}"
            );
        }
    }

    /// ADR 0028 / ADR 0035: the `ask_user` record
    /// is an **open** frame - a `╭─ ask …` top line with no right corner, `│ ` body rows, a
    /// `╰──` tail - and single, multi and `Other` share the circle marker pair `○`/`◉`. The
    /// unanswered, answered and declined states are the same block. The content is the ADR 0024
    /// revision 1 R1 structure (prompt, options, `↳` description, note, status).
    #[test]
    fn the_ask_block_is_an_open_frame() {
        use serde_json::json;
        let arguments = json!({"questions":[{"id":"pick","prompt":"Pick one","kind":"single",
            "options":[{"id":"a","label":"Option A","recommended":true},
                       {"id":"b","label":"Option B","description":"the second"}]}]});
        let card = tools::ToolCard::new("c1", "ask_user", arguments.clone());
        let texts = |card: &tools::ToolCard, error: bool, glyphs| {
            ask_block(card, "user declined to answer", error, 44, glyphs)
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>()
        };

        let unanswered = texts(&card, false, crate::footer::Glyphs::Unicode);
        assert!(unanswered[0].starts_with("╭─ ask "), "{unanswered:?}");
        assert_eq!(
            unanswered[0].chars().last(),
            Some('─'),
            "the top line is open (no right corner): {unanswered:?}"
        );
        assert_eq!(unanswered[0].chars().count(), 44, "{unanswered:?}");
        assert!(
            unanswered.iter().any(|t| t.contains("Pick one")),
            "{unanswered:?}"
        );
        assert!(
            unanswered
                .iter()
                .any(|t| t.contains("○ Option A (Recommended)")),
            "{unanswered:?}"
        );
        assert!(
            unanswered.iter().any(|t| t.contains("↳ the second")),
            "{unanswered:?}"
        );
        assert_eq!(unanswered.last().unwrap(), "╰──", "{unanswered:?}");

        // Answered: the chosen option is `◉` and the note shows.
        let mut answered_card = card.clone();
        answered_card.structured =
            Some(json!({"answers":[{"id":"pick","selected":["a"],"note":"go with A"}]}));
        let answered = texts(&answered_card, false, crate::footer::Glyphs::Unicode);
        assert!(
            answered.iter().any(|t| t.contains("◉ Option A")),
            "{answered:?}"
        );
        assert!(
            answered.iter().any(|t| t.contains("note: go with A")),
            "{answered:?}"
        );

        // Declined: the error text is a status line.
        let declined = texts(&card, true, crate::footer::Glyphs::Unicode);
        assert!(
            declined.iter().any(|t| t.contains("user declined")),
            "{declined:?}"
        );

        // The ascii fallback: `+-`/`|`/`+--` and `( )`/`(*)`.
        let ascii = texts(&card, false, crate::footer::Glyphs::Ascii);
        assert!(ascii[0].starts_with("+- ask "), "{ascii:?}");
        assert_eq!(
            ascii[0].chars().last(),
            Some('-'),
            "the top line is open: {ascii:?}"
        );
        assert!(
            ascii.iter().any(|t| t.contains("( ) Option A")),
            "{ascii:?}"
        );
        assert!(
            ascii.iter().any(|t| t.contains("-> the second")),
            "{ascii:?}"
        );
        assert_eq!(ascii.last().unwrap(), "+--", "{ascii:?}");
        assert!(!ascii.iter().any(|t| t.contains('╭')), "{ascii:?}");
    }

    /// ADR 0028 / ADR 0035: every body row of the
    /// open frame starts with `│ ` in the marker column (columns 1-2) and its content starts at
    /// column 3; the top line runs to the body width and does not close; the tail is `╰──`.
    /// Checked at a narrow and a wide width, unicode and ascii, with a long CJK prompt, options
    /// and a description - and no right-corner or divider glyph anywhere.
    #[test]
    fn the_open_frame_rows_sit_in_the_marker_column() {
        use serde_json::json;
        let unanswered = tools::ToolCard::new(
            "c1",
            "ask_user",
            json!({"questions":[{
                "id":"intent","kind":"text",
                "prompt":"你的整条消息本来就包含了这段话，所以我只是把它原样转述一遍，好让这一段足够长从而必须折行"
            }]}),
        );
        let mut answered = tools::ToolCard::new(
            "c2",
            "ask_user",
            json!({"questions":[{
                "id":"pick","kind":"single",
                "options":[
                    {"id":"a","label":"第一个选项","description":"这是第一个选项的说明文字，也足够长到需要折行"},
                    {"id":"b","label":"Option B","recommended":true}
                ]
            }]}),
        );
        answered.structured = Some(json!({"answers":[
            {"id":"pick","selected":["a"],"other":"我自己写的另一个答案","note":"一条够长的备注，也应该被折到内宽以内"}
        ]}));
        let mut multi = tools::ToolCard::new(
            "c3",
            "ask_user",
            json!({"questions":[{"id":"tiers","kind":"multi","options":[
                {"id":"safe","label":"safe"},{"id":"workspace","label":"workspace"}]}]}),
        );
        multi.structured =
            Some(json!({"answers":[{"id":"tiers","selected":["safe","workspace"]}]}));
        for width in [40usize, 100] {
            for glyphs in [crate::footer::Glyphs::Unicode, crate::footer::Glyphs::Ascii] {
                let g = AskGlyphs::for_glyphs(glyphs);
                let marker = format!("{} ", g.vertical);
                let top = format!("{}{} ask ", g.top_left, g.horizontal);
                let bottom = format!("{}{}{}", g.bottom_left, g.horizontal, g.horizontal);
                for (card, label) in [
                    (&unanswered, "unanswered"),
                    (&answered, "answered"),
                    (&multi, "multi"),
                ] {
                    let lines = ask_block(card, "user declined to answer", false, width, glyphs);
                    let first = &lines.first().unwrap().text;
                    assert!(
                        first.starts_with(&top),
                        "{label} at width {width} with {glyphs:?}: {first:?}"
                    );
                    assert_eq!(
                        first.chars().last(),
                        Some(g.horizontal),
                        "{label} at width {width} with {glyphs:?} top is open: {first:?}"
                    );
                    assert_eq!(
                        UnicodeWidthStr::width(first.as_str()),
                        width,
                        "{label} at width {width} with {glyphs:?} top width: {first:?}"
                    );
                    for line in &lines[1..lines.len() - 1] {
                        assert!(
                            line.text.starts_with(&marker),
                            "{label} at width {width} with {glyphs:?}: a body row does not start \
                             with the {} marker column: {:?}",
                            g.vertical,
                            line.text
                        );
                    }
                    assert_eq!(
                        lines.last().unwrap().text,
                        bottom,
                        "{label} at width {width} with {glyphs:?}"
                    );
                    assert!(
                        !lines
                            .iter()
                            .any(|line| line.text.contains('╮') || line.text.contains('╯')),
                        "{label} at width {width} with {glyphs:?}: no closed corner anywhere"
                    );
                }
            }
        }
    }

    /// ADR 0028: the open frame's tail is exactly three cells (`╰──`,
    /// ascii `+--`), and the top line still runs to the body width.
    #[test]
    fn the_ask_bottom_is_the_three_cell_tail() {
        use serde_json::json;
        let card = tools::ToolCard::new(
            "c1",
            "ask_user",
            json!({"questions":[{"id":"q","kind":"text","prompt":"hi"}]}),
        );
        for width in [40usize, 100] {
            for glyphs in [crate::footer::Glyphs::Unicode, crate::footer::Glyphs::Ascii] {
                let g = AskGlyphs::for_glyphs(glyphs);
                let lines = ask_block(&card, "", false, width, glyphs);
                assert_eq!(
                    lines.last().unwrap().text,
                    format!("{}{}{}", g.bottom_left, g.horizontal, g.horizontal),
                    "{glyphs:?}"
                );
                assert_eq!(
                    UnicodeWidthStr::width(lines.last().unwrap().text.as_str()),
                    3,
                    "the tail is three cells ({glyphs:?})"
                );
                assert_eq!(
                    UnicodeWidthStr::width(lines.first().unwrap().text.as_str()),
                    width,
                    "the top line runs to the body width ({glyphs:?})"
                );
            }
        }
    }

    /// ADR 0028: single, multi and `Other` share one marker pair - the
    /// circle `○`/`◉` under unicode, `( )`/`(*)` under ascii.
    #[test]
    fn the_ask_glyphs_use_one_circle_pair() {
        let g = AskGlyphs::for_glyphs(crate::footer::Glyphs::Unicode);
        assert_eq!(g.single_off, "○");
        assert_eq!(g.single_on, "◉");
        assert_eq!(g.multi_off, g.single_off);
        assert_eq!(g.multi_on, g.single_on);
        let g = AskGlyphs::for_glyphs(crate::footer::Glyphs::Ascii);
        assert_eq!(g.single_off, "( )");
        assert_eq!(g.single_on, "(*)");
        assert_eq!(g.multi_off, g.single_off);
        assert_eq!(g.multi_on, g.single_on);
    }
}

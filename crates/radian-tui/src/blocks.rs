//! The display model (ADR 0028 R1): an entry parses into **blocks**, blocks lay out into
//! **lines**. Everything the transcript shows about an entry goes through [`layout`], and the
//! selection mapping ([`crate::select`]) reads the same [`BodyLine`]s — one geometry, two
//! consumers, so what is highlighted is what is copied.
//!
//! Two caches live on [`crate::app::Entry`]:
//!
//! - the **block** cache (parsed ranges, keyed by text length and the reasoning mode — the
//!   fold state is part of the key, ADR 0028 R1). Parsing is incremental: closed blocks are
//!   reused, only the trailing open block is re-parsed, so a streaming frame's parse cost does
//!   not grow with the entry's length (the text only ever grows while live);
//! - the **line count** cache (keyed by width, text length and the mode, as before).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{Entry, EntryKind};
use crate::tools::{self, DisplayModes};

/// How the reasoning channel displays (ADR 0028 R3). The fold state is part of the block and
/// layout cache keys: toggling it re-lays the affected entries out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningMode {
    /// One placeholder line: `▸ thinking (N lines)` (the ADR's default).
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

/// One laid-out line of an entry's body. `chars`/`end` are the selection mapping's view (the
/// same semantics as [`crate::render::WrappedLine`]: byte offsets into the entry's display
/// text, `end` the gap after the line); `text` is what the body span renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLine {
    /// The source byte offset of each char on this line, in order (`None` when the renderer
    /// synthesized the char — a bullet, a heading's `##`, a link's `(url)`, an indent). A
    /// synthesized cell still occupies its display columns but has no source text: it never
    /// copies and never anchors a selection. A **padding row** has none at all.
    pub chars: Vec<Option<usize>>,
    pub end: usize,
    /// What renders. Code lines are tab-expanded and truncated to the width with `…` (the
    /// full line still copies: `chars` only covers the kept prefix, and `end` reaches the
    /// line's true end, ADR 0028 R9).
    pub text: String,
    /// This line is a code line (rendered without the prefix lead, no wrap).
    pub code: bool,
    /// This line is a padding row around a user block.
    pub pad: bool,
    /// The role a tool body line renders with (ADR 0028 R4's diff roles and the warning
    /// channel); `Normal` for everything that is not a card body line.
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
    /// A heading's text (`mdHeading`).
    Heading,
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
            pad: false,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: false,
            md: None,
            syntax: None,
        }
    }

    fn pad(end: usize) -> Self {
        Self {
            chars: Vec::new(),
            end,
            text: String::new(),
            code: false,
            pad: true,
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
            pad: false,
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
            pad: false,
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
            pad: false,
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
        glyphs: modes.glyphs,
        reasoning: ReasoningMode::Expanded,
        tools: crate::tools::ToolDisplay {
            fold: crate::tools::ToolFold::Expanded,
            preview_lines: modes.tools.preview_lines,
        },
        // A diagram is drawn (or not) the same way while searching: its lines are what the
        // search matches. The same for syntax highlighting.
        mermaid: modes.mermaid,
        highlight: modes.highlight,
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
            shown.len() < full.len()
        }
        _ => false,
    }
}

/// Strips an entry's **backgroundless blank** edge lines (ADR 0028 rule 1): the
/// container owns the spacing between blocks, so a text-derived blank line at an entry's
/// start or end (a leading/trailing newline in the message) must not add a second one. A
/// **pad** row is the block's own shape (a user block's breathing room, painted or not) and
/// stays; the layout is the one place this happens, so counting, drawing, the selection
/// mapping and the search all see the stripped form.
fn strip_edge_blanks(lines: &mut Vec<BodyLine>) {
    let blank = |line: &BodyLine| !line.pad && !line.hint && line.chars.is_empty();
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
/// * either side is a user block: its pad row is the separation already (painted in a theme
///   with a user background, blank in one without - the shape is the same either way).
///
/// Otherwise exactly one line. The counting ([`line_count`]'s callers), the drawing
/// ([`crate::render::visible_lines`]), the selection mapping and the find geometry all call
/// this, so a gap cannot exist in one and not the other.
pub fn block_gap(prev: Option<&Entry>, next: &Entry) -> usize {
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
    if prev.kind == EntryKind::User || next.kind == EntryKind::User {
        return 0;
    }
    1
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
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| block_gap(entries.get(index.wrapping_sub(1)), entry))
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

/// How many display lines an entry's body lays out to (including padding rows; a `Hidden`
/// reasoning entry contributes zero). **Counts only**: no line is materialized, so the frame
/// that measures every entry (the first one, or the first after a resize) does not allocate
/// per entry — the virtualization budget (ADR 0028 R7) depends on this.
pub fn line_count(entry: &Entry, modes: DisplayModes, width: usize) -> usize {
    let mode = modes.reasoning;
    if let Some(count) = cheap_count(entry, modes) {
        return count;
    }
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    // A blank edge line is stripped by the layout; the cheap arms below do not know that, so
    // this entry goes through the layout itself (the one place the rule lives). Highlighting
    // is a color-only pass and must not run here: the measure pass is the cheap one
    // (`tui.display.highlight`'s lazy rule).
    if has_blank_edge(&text) {
        let plain = DisplayModes {
            glyphs: crate::footer::Glyphs::Unicode,
            highlight: false,
            ..modes
        };
        return layout(entry, &parse_blocks_for(entry, mode), plain, width).len();
    }
    match entry.kind {
        EntryKind::User => {
            // pad + text + pad.
            crate::render::wrap_line_count(&text, body_width) + 2
        }
        EntryKind::Assistant => {
            let mut diagram_index = 0;
            parse_blocks_for(entry, mode)
                .iter()
                .map(|block| match block {
                    Block::Paragraph { start, end, .. } => {
                        md_line_count_cached(entry, *start, *end, body_width, &text)
                    }
                    Block::Code {
                        start,
                        end,
                        closed,
                        lang,
                    } => {
                        let source = &text[*start..*end];
                        if is_mermaid(lang, *closed, modes.mermaid) {
                            let index = diagram_index;
                            diagram_index += 1;
                            match diagram_outcome(entry, index, &code_body(source, *closed), width)
                            {
                                DiagramOutcome::Rendered(lines) => lines.len(),
                                DiagramOutcome::Failed(_) => 1 + code_line_count(source, *closed),
                            }
                        } else {
                            code_line_count(source, *closed)
                        }
                    }
                })
                .sum()
        }
        // One header line, never wrapped (the same rule the layout uses).
        EntryKind::ToolCall => 1,
        EntryKind::ToolResult => {
            let card = entry.tool.as_deref();
            let plan = tool_plan(card, &text, entry.is_error, modes, entry.fold_override);
            plan.iter()
                .map(|line| match line {
                    tools::BodyLine::Text { source, .. } => {
                        crate::render::wrap_line_count(&text[source.clone()], body_width)
                    }
                    tools::BodyLine::Hint(_) => 1,
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
) -> Vec<tools::BodyLine> {
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

/// How many display lines a fenced code block lays out to: its body's line count (the fence
/// lines are markup). Mirrors [`code_lines`]'s skipping exactly.
fn code_line_count(raw: &str, closed: bool) -> usize {
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
    count.max(1)
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
    // Render this width's diagrams once and keep them: the key is `(width, text_len)`, so a
    // resize re-renders (a `TooWide` failure can succeed on a wider terminal) and an append
    // re-renders (a longer source is a different diagram).
    if modes.mermaid && entry.kind == EntryKind::Assistant {
        let blocks = blocks_for(entry, modes.reasoning);
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
    // The Markdown paragraph lines do not depend on the folds, so cache them by
    // `(width, text_len)`: a global fold toggle re-counts the entries but does not re-parse
    // them (the R7 toggle gate).
    if entry.kind == EntryKind::Assistant {
        let blocks = blocks_for(entry, modes.reasoning);
        // The same body width `layout`/`line_count` look up with, or the cache never hits.
        let body_width = width
            .saturating_sub(crate::render::marker_prefix(entry.kind).width())
            .max(1);
        fill_md_cache(entry, &blocks, body_width);
    }
    let lines = line_count(entry, modes, width);
    entry.layout_cache = Some((key, lines));
    lines
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
/// path: closed blocks are not re-parsed, ADR 0028 R1). Fresh parse otherwise.
pub fn blocks_for(entry: &mut Entry, mode: ReasoningMode) -> Vec<Block> {
    if entry.kind != EntryKind::Assistant {
        // Only assistant prose carries fences today; every other kind is one paragraph.
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
    if entry.kind != EntryKind::Assistant {
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
    // The container owns the spacing (ADR 0028 rule 1): a text-derived blank line
    // at an entry's start or end is stripped here, so counting, drawing, the selection mapping
    // and the search all see the same shape.
    let mut lines = layout_unstripped(entry, blocks, modes, width);
    strip_edge_blanks(&mut lines);
    lines
}

/// [`layout`] without the edge stripping.
fn layout_unstripped(
    entry: &Entry,
    blocks: &[Block],
    modes: DisplayModes,
    width: usize,
) -> Vec<BodyLine> {
    let mode = modes.reasoning;
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    let tool_expanded = expanded(entry, modes.tools.fold == tools::ToolFold::Expanded);
    let reasoning_expanded = expanded(entry, mode == ReasoningMode::Expanded);
    match (entry.kind, mode) {
        (EntryKind::Reasoning, ReasoningMode::Hidden) if !reasoning_expanded => Vec::new(),
        (EntryKind::Reasoning, ReasoningMode::Collapsed) if !reasoning_expanded => {
            vec![BodyLine {
                chars: Vec::new(),
                end: 0,
                text: placeholder(entry),
                code: false,
                pad: false,
                role: tools::LineRole::Normal,
                hint: true,
                diagram: None,
                reason: false,
                md: None,
                syntax: None,
            }]
        }
        (EntryKind::User, _) => {
            let mut lines = vec![BodyLine::pad(0)];
            lines.extend(prose_lines(&text, body_width));
            let end = text.len();
            lines.push(BodyLine::pad(end));
            lines
        }
        (EntryKind::Assistant, _) => {
            let mut lines = Vec::new();
            let mut diagram_index = 0;
            for block in blocks {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        lines.extend(md_lines_cached(entry, *start, *end, body_width, &text));
                    }
                    Block::Code {
                        start,
                        end,
                        closed,
                        lang,
                    } => {
                        let source = &text[*start..*end];
                        // A Mermaid block is drawn, not highlighted; every other closed block
                        // whose language is in the table is (ADR 0028 R2).
                        let highlight = if is_mermaid(lang, *closed, modes.mermaid) {
                            None
                        } else {
                            syntax_lines_cached(*start, *end, &text, lang, *closed, modes.highlight)
                        };
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
                                highlight.as_deref().map(Vec::as_slice),
                            ));
                        }
                    }
                }
            }
            lines
        }
        (EntryKind::ToolCall, _) => {
            // ADR 0024 R1: an `ask_user` call is the boxed question block, with no
            // answers yet (the result entry carries them).
            match entry.tool.as_deref() {
                Some(card) if card.name == "ask_user" => {
                    ask_block(card, &text, false, body_width, modes.glyphs)
                }
                // One header line, laid out like a code line: never wrapped, truncated with `…`
                // when it does not fit. The entry's text **is** this header (set at push time
                // from the same function), so the characters map back and copy correctly.
                _ => tool_line(&text, width),
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
                    pad: false,
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
                return ask_block(card, &text, entry.is_error, body_width, modes.glyphs);
            }
            let plan = match card {
                Some(card) => tools::body_plan(
                    card,
                    &text,
                    entry.is_error,
                    // The per-entry override (a search hit) opens this body.
                    if tool_expanded {
                        tools::ToolFold::Expanded
                    } else {
                        modes.tools.fold
                    },
                    modes.tools.preview_lines,
                ),
                // No card data (an older entry, or a shape the TUI did not model): the raw
                // text, always expanded — a renderer must cope with "no card".
                None => tools::body_plan(
                    &tools::ToolCard::new("", "", serde_json::Value::Null),
                    &text,
                    entry.is_error,
                    tools::ToolFold::Expanded,
                    0,
                ),
            };
            body_plan_lines(&plan, &text, body_width)
        }
        _ => prose_lines(&text, body_width),
    }
}

/// The box and mark glyphs of the `ask_user` block (ADR 0024 R1): unicode by
/// default, pure ASCII under `glyphs = ascii`.
struct AskGlyphs {
    top_left: char,
    top_right: char,
    bottom_left: char,
    bottom_right: char,
    horizontal: char,
    tee_left: char,
    tee_right: char,
    single_on: &'static str,
    single_off: &'static str,
    multi_on: &'static str,
    multi_off: &'static str,
    desc: &'static str,
}

impl AskGlyphs {
    fn for_glyphs(glyphs: crate::footer::Glyphs) -> Self {
        match glyphs {
            crate::footer::Glyphs::Ascii => Self {
                top_left: '+',
                top_right: '+',
                bottom_left: '+',
                bottom_right: '+',
                horizontal: '-',
                tee_left: '+',
                tee_right: '+',
                single_on: "(*)",
                single_off: "( )",
                multi_on: "[x]",
                multi_off: "[ ]",
                desc: "->",
            },
            crate::footer::Glyphs::Unicode | crate::footer::Glyphs::Nerd => Self {
                top_left: '╭',
                top_right: '╮',
                bottom_left: '╰',
                bottom_right: '╯',
                horizontal: '─',
                tee_left: '├',
                tee_right: '┤',
                single_on: "◉",
                single_off: "○",
                multi_on: "☑",
                multi_off: "☐",
                desc: "↳",
            },
        }
    }
}

/// A synthesized `ask_user` line: no source characters (it neither copies nor matches), styled
/// by `md` pieces.
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
        code: false,
        pad: false,
        role: tools::LineRole::Normal,
        hint: true,
        diagram: None,
        reason: false,
        md: Some(md),
        syntax: None,
    }
}

/// The `ask_user` card as a boxed block (ADR 0024 R1): the request from the call's
/// `arguments`, the answers from the result's `structured`. Three states: unanswered (the call,
/// no structured result), answered, and declined/cancelled (an error result).
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
    let count = questions.len();
    let title = format!("Ask {count} question{}", if count == 1 { "" } else { "s" });
    out.push(ask_line(ask_top_border(&g, width, &title)));
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
        let mut label = format!("[{id}]");
        if kind != "single" {
            label.push_str(&format!(" · {kind}"));
        }
        if let Some(options) = options
            && !options.is_empty()
        {
            label.push_str(&format!(" · options:{}", options.len()));
        }
        out.push(ask_line(ask_divider(&g, width, &label)));
        let prompt = question
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        for line in ask_wrap(prompt, width.saturating_sub(2)) {
            out.push(ask_line(vec![
                (" ".to_string(), MdStyle::Dim),
                (line, MdStyle::Accent),
            ]));
        }
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
                out.push(ask_line(vec![
                    (format!("  {mark} "), MdStyle::Dim),
                    (label, MdStyle::Plain),
                ]));
                if let Some(description) = option
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                {
                    out.push(ask_line(vec![
                        (format!("     {} ", g.desc), MdStyle::Dim),
                        (description.to_owned(), MdStyle::Muted),
                    ]));
                }
            }
        }
        if let Some(reply) = reply {
            for (key, style) in [("other", MdStyle::Muted), ("note", MdStyle::Muted)] {
                if let Some(value) = reply.get(key).and_then(serde_json::Value::as_str) {
                    out.push(ask_line(vec![
                        (format!("  {key}: "), MdStyle::Dim),
                        (value.to_owned(), style),
                    ]));
                }
            }
        }
    }
    // Declined/cancelled (an error result) is one status line; the text is the tool failure.
    if is_error {
        let status = text.lines().next().unwrap_or("(no answer)").to_owned();
        out.push(ask_line(vec![
            (" ".to_string(), MdStyle::Dim),
            (status, MdStyle::Muted),
        ]));
    }
    out.push(ask_line(vec![(
        format!("{}{}{}", g.bottom_left, g.horizontal, g.bottom_right),
        MdStyle::Dim,
    )]));
    out
}

/// The block's top border with the title embedded (ADR 0024 R1).
fn ask_top_border(g: &AskGlyphs, width: usize, title: &str) -> Vec<(String, MdStyle)> {
    let mut pieces = vec![
        (format!("{}{} ", g.top_left, g.horizontal), MdStyle::Dim),
        (title.to_owned(), MdStyle::Heading),
        (" ".to_string(), MdStyle::Dim),
    ];
    let used =
        unicode_width::UnicodeWidthStr::width(format!("{}{} ", g.top_left, g.horizontal).as_str())
            + unicode_width::UnicodeWidthStr::width(title)
            + 1
            + 1;
    let fill = width.saturating_sub(used);
    pieces.push((g.horizontal.to_string().repeat(fill), MdStyle::Dim));
    pieces.push((g.top_right.to_string(), MdStyle::Dim));
    pieces
}

/// A labeled `├──┤` divider (ADR 0024 R1): `[id] · kind · options:N`.
fn ask_divider(g: &AskGlyphs, width: usize, label: &str) -> Vec<(String, MdStyle)> {
    let prefix = format!("{}{} ", g.tee_left, g.horizontal);
    let mut pieces = vec![
        (prefix.clone(), MdStyle::Dim),
        (label.to_owned(), MdStyle::Muted),
        (" ".to_string(), MdStyle::Dim),
    ];
    let used = unicode_width::UnicodeWidthStr::width(prefix.as_str())
        + unicode_width::UnicodeWidthStr::width(label)
        + 1
        + 1;
    let fill = width.saturating_sub(used);
    pieces.push((g.horizontal.to_string().repeat(fill), MdStyle::Dim));
    pieces.push((g.tee_right.to_string(), MdStyle::Dim));
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

/// One tool header line: the same fit rule as a code line (a line exactly `width` wide shows
/// in full; anything wider is cut and gets `…`), with the characters mapped to the entry's
/// text so selection and copy work on it.
fn tool_line(text: &str, width: usize) -> Vec<BodyLine> {
    let (rendered, kept, cut) = crate::blocks::code_line_layout(text, width);
    let mut chars: Vec<usize> = kept;
    if cut && let Some(&last) = chars.last() {
        chars.push(last);
    }
    let chars: Vec<Option<usize>> = chars.into_iter().map(Some).collect();
    vec![BodyLine {
        chars,
        end: text.len(),
        text: rendered,
        code: false,
        pad: false,
        role: tools::LineRole::Normal,
        hint: false,
        diagram: None,
        reason: false,
        md: None,
        syntax: None,
    }]
}

/// Turns a tool body plan into laid-out lines: a text line wraps like prose at the body width
/// (tabs expanded, its characters keeping their source offsets), a hint is one synthesized line.
fn body_plan_lines(plan: &[tools::BodyLine], text: &str, body_width: usize) -> Vec<BodyLine> {
    let mut out = Vec::new();
    for line in plan {
        match line {
            tools::BodyLine::Text { source, role } => {
                let slice = text.get(source.clone()).unwrap_or("");
                for mut laid in prose_lines(slice, body_width) {
                    laid.role = *role;
                    out.push(laid);
                }
            }
            tools::BodyLine::Hint(hint) => out.push(BodyLine {
                chars: Vec::new(),
                end: 0,
                text: hint.clone(),
                code: false,
                pad: false,
                role: tools::LineRole::Normal,
                hint: true,
                diagram: None,
                reason: false,
                md: None,
                syntax: None,
            }),
        }
    }
    out
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

/// The streaming spinner: a deterministic function of the text's length, so a delta batch
/// (which changes the length and dirties the frame) is the only thing that turns it. The
/// quadrant blocks (ADR 0027) are in all seven checked fonts; the braille frames
/// (`⠋⠙⠸⠴`) and the arc (`◜◝◞◟`) are not, so they are not used.
pub(crate) const SPINNER: [char; 4] = ['▖', '▘', '▝', '▗'];

/// The collapsed reasoning placeholder (ADR 0028 R3). `live` entries get the spinner; the
/// line count stays 1 either way.
fn placeholder(entry: &Entry) -> String {
    let lines = entry.text.lines().count().max(1);
    // No `▸` here: the marker column carries it (ADR 0028 rule 5), so the body
    // column is the same folded and expanded.
    if entry.live {
        format!("thinking ({lines} lines) {}", SPINNER[entry.text.len() % 4])
    } else {
        format!("thinking ({lines} lines)")
    }
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

/// The text of a fenced code block's display lines (fences are markup). Mirrors [`code_lines`].
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
    let mut out: Vec<String> = body
        .iter()
        .map(|line| code_line_text(line, width))
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
        let plain = DisplayModes {
            glyphs: crate::footer::Glyphs::Unicode,
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
            vec![placeholder(entry)]
        }
        (EntryKind::User, _) => {
            let mut lines = vec![String::new()];
            lines.extend(crate::render::wrap(&text, body_width));
            lines.push(String::new());
            lines
        }
        (EntryKind::Assistant, _) => {
            let mut lines = Vec::new();
            let mut diagram_index = 0;
            for block in parse_blocks_for(entry, mode) {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        lines.extend(
                            md_lines_cached(entry, start, end, body_width, &text)
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
        (EntryKind::ToolCall, _) => {
            vec![code_line_layout(&text, width).0]
        }
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
            let mut out = Vec::new();
            for line in &plan {
                match line {
                    tools::BodyLine::Text { source, .. } => {
                        // The same lines the layout draws (tabs expanded included).
                        let slice = text.get(source.clone()).unwrap_or("");
                        out.extend(prose_lines(slice, body_width).into_iter().map(|l| l.text));
                    }
                    tools::BodyLine::Hint(hint) => out.push(hint.clone()),
                }
            }
            out
        }
        _ => crate::render::wrap(&text, body_width),
    }
}

/// Lays one fenced code block out (ADR 0028 R9): the fence lines themselves are markup and
/// do not show; each source line is one display line, tab-expanded (4 spaces), truncated to
/// `width` with `…`. The `chars` cover the kept characters only — what is cut cannot be
/// clicked — while `end` still reaches the source line's end, so a selection that spans the
/// line copies the full text. `raw` is the whole block range (fences included) starting at
/// `offset` in the display text.
fn code_lines(
    raw: &str,
    offset: usize,
    closed: bool,
    width: usize,
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
    let mut out = Vec::new();
    for (index, line) in body.iter().enumerate() {
        // Byte offset of the line's first char within the display text.
        let offset = line.as_ptr() as usize - base + offset;
        let (plain, kept, cut) = code_line_layout(line, width);
        let mut chars: Vec<Option<usize>> =
            kept.into_iter().map(|idx| Some(offset + idx)).collect();
        if cut {
            // The ellipsis is decoration; map it to the last kept char.
            if let Some(&last) = chars.last() {
                chars.push(last);
            }
        }
        let end = offset + line.len();
        // `styled_line` mirrors `code_line_layout` (same tab expansion, same width rule), so
        // the drawn text is the same either way; only the roles differ.
        let syntax = highlight
            .and_then(|lines| lines.get(index))
            .map(|hl| crate::highlight::styled_line(hl, width));
        let text = match &syntax {
            Some(pieces) => pieces.iter().map(|(text, _)| text.as_str()).collect(),
            None => plain,
        };
        out.push(BodyLine {
            chars,
            end,
            text,
            code: true,
            pad: false,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: false,
            md: None,
            syntax,
        });
    }
    if out.is_empty() {
        // An empty code block still shows one blank line.
        out.push(BodyLine {
            chars: Vec::new(),
            end: 0,
            text: String::new(),
            code: true,
            pad: false,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: false,
            md: None,
            syntax: None,
        });
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
pub(crate) fn md_lines(source: &str, base: usize, width: usize) -> Vec<BodyLine> {
    let blocks = crate::markdown::parse(source, base);
    let mut out = Vec::new();
    md_blocks(&blocks, width, base, 0, &mut out);
    // A block range carries its trailing newline (the ranges tile the text), and the prose
    // wrap showed it as one empty line; pulldown drops it, so restore it. The blank line
    // between two paragraphs is what makes the transcript read as the author wrote it.
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

/// The Markdown lines of the paragraph at `[start, end)`: the entry's `(width, text_len)`
/// cache when it holds them, else rendered fresh (the immutable paths may not have run the
/// mutable measure pass). The layout does not depend on the folds, so a fold toggle reuses
/// the cache and stays inside the R7 gate.
fn md_lines_cached(
    entry: &Entry,
    start: usize,
    end: usize,
    width: usize,
    text: &str,
) -> Vec<BodyLine> {
    if let Some((cached_width, cached_len, paragraphs)) = &entry.md_cache
        && *cached_width == width
        && *cached_len == entry.text.len()
        && let Some((_, lines)) = paragraphs.iter().find(|(at, _)| *at == start)
    {
        return lines.clone();
    }
    md_lines(&text[start..end], start, width)
}

/// The Markdown line count of the paragraph at `[start, end)` without cloning the cached
/// lines (the per-frame measure path).
fn md_line_count_cached(
    entry: &Entry,
    start: usize,
    end: usize,
    width: usize,
    text: &str,
) -> usize {
    if let Some((cached_width, cached_len, paragraphs)) = &entry.md_cache
        && *cached_width == width
        && *cached_len == entry.text.len()
        && let Some((_, lines)) = paragraphs.iter().find(|(at, _)| *at == start)
    {
        return lines.len();
    }
    md_lines(&text[start..end], start, width).len()
}

/// Builds the entry's Markdown paragraph-line cache for `width` (the mutable measure pass),
/// so the following `layout`/`line_count` reuse it.
fn fill_md_cache(entry: &mut Entry, blocks: &[Block], width: usize) {
    let text_len = entry.text.len();
    let fresh = entry
        .md_cache
        .as_ref()
        .filter(|(cached_width, cached_len, _)| *cached_width == width && *cached_len == text_len)
        .is_none();
    if !fresh {
        return;
    }
    let text = crate::render::display_text(entry);
    let mut paragraphs = Vec::new();
    for block in blocks {
        if let Block::Paragraph { start, end, .. } = block {
            paragraphs.push((*start, md_lines(&text[*start..*end], *start, width)));
        }
    }
    entry.md_cache = Some((width, text_len, paragraphs));
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
    out: &mut Vec<BodyLine>,
) {
    use crate::markdown::MdBlock;
    for block in blocks {
        match block {
            MdBlock::Paragraph(pieces) => {
                out.extend(md_wrap(md_pieces(pieces, MdStyle::Plain), width, indent));
            }
            MdBlock::Heading { level, pieces } => {
                // The `#`s are markup; the level shows as a synthesized prefix.
                let mut line = vec![MdPiece {
                    text: format!("{} ", "#".repeat(*level as usize)),
                    style: MdStyle::Heading,
                    offset: None,
                }];
                line.extend(md_pieces(pieces, MdStyle::Heading));
                out.extend(md_wrap(line, width, indent));
            }
            MdBlock::List { ordered, items } => {
                for (index, item) in items.iter().enumerate() {
                    let marker = match ordered {
                        Some(first) => format!("{}. ", first + index as u64),
                        None => "• ".to_owned(),
                    };
                    let mut item_lines = Vec::new();
                    md_blocks(item, width, base, indent + 2, &mut item_lines);
                    let mut first = true;
                    for line in item_lines {
                        if first {
                            // The marker takes the first line's indent columns, so the text
                            // starts right after it (`• one`, not `•   one`).
                            let mut pieces = vec![MdPiece {
                                text: marker.clone(),
                                style: MdStyle::Bullet,
                                offset: None,
                            }];
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
                            vec![MdPiece {
                                text: marker,
                                style: MdStyle::Bullet,
                                offset: None,
                            }],
                        ));
                    }
                }
            }
            MdBlock::Quote(inner) => {
                let mut inner_lines = Vec::new();
                md_blocks(
                    inner,
                    width.saturating_sub(2).max(1),
                    base,
                    indent,
                    &mut inner_lines,
                );
                for line in inner_lines {
                    let mut pieces = vec![MdPiece {
                        text: "│ ".to_owned(),
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
                    text: "─".repeat(width.clamp(1, 40)),
                    style: MdStyle::Rule,
                    offset: None,
                }],
            )),
            MdBlock::Table { head, rows } => out.extend(md_table(head, rows, base, width, indent)),
        }
    }
}

/// The Markdown pieces of a parsed block, in the given fallback role.
fn md_pieces(pieces: &[crate::markdown::Piece], plain: MdStyle) -> Vec<MdPiece> {
    pieces
        .iter()
        .map(|piece| MdPiece {
            text: piece.text.clone(),
            style: match piece.inline {
                crate::markdown::Inline::Bold => MdStyle::Bold,
                crate::markdown::Inline::Italic => MdStyle::Italic,
                crate::markdown::Inline::Code => MdStyle::Code,
                crate::markdown::Inline::LinkText => MdStyle::Link,
                crate::markdown::Inline::LinkUrl => MdStyle::LinkUrl,
                crate::markdown::Inline::Plain => plain,
            },
            offset: piece.offset,
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
    let width = width.saturating_sub(indent).max(1);
    let pad = " ".repeat(indent);
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
    let flush = |row: &mut Vec<Cell>, out: &mut Vec<BodyLine>| {
        let mut pieces: Vec<MdPiece> = Vec::new();
        for (ch, style, offset) in row.iter() {
            match pieces.last_mut() {
                Some(piece)
                    if piece.style == *style && piece.offset.is_some() == offset.is_some() =>
                {
                    // Extend a run only when its offsets stay contiguous.
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
        let mut all = vec![MdPiece {
            text: pad.clone(),
            style: MdStyle::Plain,
            offset: None,
        }];
        all.extend(pieces);
        let chars = md_chars(&all);
        // `end` is the gap after the line's last source char, so the selection mapping lands
        // between lines the way the prose wrap's does.
        let end = row
            .iter()
            .filter_map(|(ch, _, offset)| offset.map(|offset| offset + ch.len_utf8()))
            .max()
            .unwrap_or(0);
        out.push(BodyLine::md(chars, end, all));
        row.clear();
    };
    let mut trailing_break = false;
    for (ch, style, offset) in cells {
        if ch == '\n' {
            flush(&mut row, &mut out);
            used = 0;
            trailing_break = true;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width && !row.is_empty() {
            flush(&mut row, &mut out);
            used = 0;
            // A soft wrap drops the space it broke at (the prose wrap's rule): it must not
            // start the next line.
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
        flush(&mut row, &mut out);
    }
    out
}

/// A table: column widths from the cells, wrapping a cell that does not fit; if even the
/// minimum does not fit (too many columns), the raw `|` source comes back as plain text
/// (ADR 0028 R2's fallback).
fn md_table(
    head: &[Vec<crate::markdown::Piece>],
    rows: &[Vec<Vec<crate::markdown::Piece>>],
    base: usize,
    width: usize,
    indent: usize,
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
    // A bordered table costs `columns + 1` borders plus one space each side of every cell.
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
    let border = |left: &str, mid: &str, right: &str| -> String {
        let mut out = String::from(left);
        for (index, w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
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
    let mut out = vec![border_line(border("┌", "┬", "┐"))];
    let row_cells: Vec<Vec<&[crate::markdown::Piece]>> = std::iter::once(head)
        .chain(rows.iter().map(Vec::as_slice))
        .map(|row| {
            (0..columns)
                .map(|column| row.get(column).map(Vec::as_slice).unwrap_or(&[]))
                .collect()
        })
        .collect();
    for (row_index, row) in row_cells.iter().enumerate() {
        let wrapped: Vec<Vec<String>> = row
            .iter()
            .enumerate()
            .map(|(column, cell)| crate::render::wrap(&cell_text(cell), widths[column].max(1)))
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
                    text: "│".to_owned(),
                    style: MdStyle::TableBorder,
                    offset: None,
                },
            ];
            for (column, column_lines) in wrapped.iter().enumerate() {
                let cell = column_lines
                    .get(line_index)
                    .map(String::as_str)
                    .unwrap_or("");
                pieces.push(MdPiece {
                    text: format!(" {} ", pad_to(cell, widths[column])),
                    style: MdStyle::Plain,
                    offset: None,
                });
                pieces.push(MdPiece {
                    text: "│".to_owned(),
                    style: MdStyle::TableBorder,
                    offset: None,
                });
            }
            out.push(BodyLine::md(vec![None; indent + 1], base, pieces));
        }
        if row_index == 0 {
            out.push(border_line(border("├", "┼", "┤")));
        }
    }
    out.push(border_line(border("└", "┴", "┘")));
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

/// Right-pads `text` to `width` display columns.
fn pad_to(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
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
        glyphs: crate::footer::Glyphs::Unicode,
        reasoning: ReasoningMode::Expanded,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
        highlight: true,
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
        assert_eq!(block_gap(Some(&assistant), &user), 0, "a user block's pad");
        assert_eq!(block_gap(Some(&user), &assistant), 0, "the same pad");
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

    #[test]
    fn a_blank_edge_line_is_stripped_and_a_pad_is_kept() {
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
        // A user block's pad rows are its shape: they stay.
        text.kind = EntryKind::User;
        text.text = "hi".to_string();
        let lines = layout(&text, &[], EXPANDED, 20);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].pad && lines[2].pad);
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
        // Fence lines are markup: body = "short" / "abc def gh ijk" → two lines.
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "short");
        assert_eq!(lines[1].text, "abc def g…", "truncated at 9 columns + …");
        assert_eq!(lines[1].chars.len(), lines[1].text.chars().count());
        // The gap still reaches the source line's end (byte offsets are into the entry
        // text, fences included): a selection across it copies all.
        let start = entry.text.find("abc").expect("the line is in the text");
        assert_eq!(lines[1].end, start + "abc def gh ijk".len());
    }

    #[test]
    fn tabs_expand_to_four_spaces_and_map_to_the_tab_offset() {
        let entry = assistant("```\na\tb\n```");
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 20);
        assert_eq!(lines[0].text, "a    b");
        let base = entry.text.find('a').expect("the line is in the text");
        assert_eq!(
            lines[0].chars,
            vec![
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
        // Exactly the width: it fits, so no ellipsis (the `…` costs a column).
        let entry = assistant(&format!("```\n{}\n```", "x".repeat(20)));
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 20);
        assert_eq!(lines[0].text, "x".repeat(20));
        assert!(!lines[0].text.contains('…'));
        // One over: it does not fit; keep width-1 and mark the cut.
        let entry = assistant(&format!("```\n{}\n```", "x".repeat(21)));
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 20);
        assert_eq!(lines[0].text, format!("{}…", "x".repeat(19)));
        // The mapping still reaches the source line's end either way (the offset is into the
        // entry text, which starts with the "```\n" fence line).
        assert_eq!(lines[0].end, "```\n".len() + 21);
    }

    #[test]
    fn cjk_lines_follow_the_same_fit_rule() {
        // 6 columns at width 6: as-is.
        let entry = assistant("```\n日本語\n```");
        let blocks = parse_blocks(&entry.text);
        let lines = layout(&entry, &blocks, EXPANDED, 6);
        assert_eq!(lines[0].text, "日本語");
        // 6 columns at width 5: keep 4 columns (2 wide chars) and the ellipsis.
        let lines = layout(&entry, &blocks, EXPANDED, 5);
        assert_eq!(lines[0].text, "日本…");
        assert_eq!(lines[0].chars.len(), lines[0].text.chars().count());
        // 6 columns at width 7: as-is.
        let lines = layout(&entry, &blocks, EXPANDED, 7);
        assert_eq!(lines[0].text, "日本語");
    }

    #[test]
    fn cjk_widths_count_in_the_truncation() {
        let entry = assistant("```\n日本語テキスト\n```");
        let blocks = parse_blocks(&entry.text);
        // Width 7: 日(2)本(2)語(2) = 6, テ would exceed → cut after 語.
        let lines = layout(&entry, &blocks, EXPANDED, 7);
        assert_eq!(lines[0].text, "日本語…");
    }

    #[test]
    fn a_user_block_gets_padding_rows_and_they_map_to_gaps() {
        let entry = Entry::new(EntryKind::User, "hello");
        let lines = layout(&entry, &[], EXPANDED, 30);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].pad && lines[2].pad);
        assert!(lines[0].chars.is_empty());
        assert_eq!(lines[0].end, 0, "the top pad is the gap before the text");
        assert_eq!(lines[2].end, 5, "the bottom pad reaches the text's end");
        assert_eq!(lines[1].text, "hello");
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
        assert_eq!(line_count(&entry, EXPANDED, 40), 3);
        // The collapsed placeholder carries the spinner while streaming.
        let lines = layout(&entry, &[], modes_with(ReasoningMode::Collapsed), 40);
        // The marker column carries the fold glyph (rule 5), not the placeholder text.
        assert!(lines[0].text.contains("thinking (3 lines)"));
        assert!(lines[0].hint, "the placeholder is a synthesized line");
        assert!(
            lines[0]
                .text
                .chars()
                .last()
                .is_some_and(|c| SPINNER.contains(&c))
        );
        entry.live = false;
        let lines = layout(&entry, &[], modes_with(ReasoningMode::Collapsed), 40);
        assert_eq!(lines[0].text, "thinking (3 lines)");
    }
    /// ADR 0024 R1: the `ask_user` card is a boxed block. The call has no answers
    /// (unanswered), the structured result marks the chosen option and shows a note, and an
    /// error result is a declined/cancelled status.
    #[test]
    fn the_ask_block_is_a_boxed_question() {
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
        assert!(
            unanswered[0].starts_with("╭─ Ask 1 question"),
            "{unanswered:?}"
        );
        assert!(unanswered[0].ends_with('╮'), "{unanswered:?}");
        assert_eq!(unanswered[0].chars().count(), 44, "{unanswered:?}");
        assert!(
            unanswered.iter().any(|t| t.contains("[pick]")),
            "{unanswered:?}"
        );
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
        assert!(unanswered.last().unwrap().ends_with('╯'), "{unanswered:?}");

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

        // The ascii fallback.
        let ascii = texts(&card, false, crate::footer::Glyphs::Ascii);
        assert!(ascii[0].starts_with("+- Ask 1 question"), "{ascii:?}");
        assert!(
            ascii.iter().any(|t| t.contains("( ) Option A")),
            "{ascii:?}"
        );
        assert!(
            ascii.iter().any(|t| t.contains("-> the second")),
            "{ascii:?}"
        );
        assert!(!ascii.iter().any(|t| t.contains('╭')), "{ascii:?}");
    }
}

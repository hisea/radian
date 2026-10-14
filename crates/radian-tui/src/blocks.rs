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
    /// Byte offsets of the chars on this line, in order. A **padding row** (a user block's
    /// breathing room) has none: it maps to a gap and copies nothing.
    pub chars: Vec<usize>,
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
}

impl BodyLine {
    fn prose(chars: Vec<usize>, end: usize, text: String) -> Self {
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
        }
    }

    /// A diagram line: every cell maps to the block's start (the diagram has no source
    /// offsets of its own), so selecting it selects the block — a copy yields the Mermaid
    /// source, the content that actually exists in the text.
    fn diagram(end: usize, offset: usize, spans: Vec<(String, DiagramRole)>) -> Self {
        let text: String = spans.iter().map(|(piece, _)| piece.as_str()).collect();
        let cells = UnicodeWidthStr::width(text.as_str());
        Self {
            chars: vec![offset; cells],
            end,
            text,
            code: true,
            pad: false,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: Some(spans),
            reason: false,
        }
    }

    fn reason(end: usize, offset: usize, text: String) -> Self {
        Self {
            chars: vec![offset],
            end,
            text,
            code: false,
            pad: false,
            role: tools::LineRole::Normal,
            hint: false,
            diagram: None,
            reason: true,
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
    match codex_mermaid::render_spans(source, width) {
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
                        crate::render::wrap_line_count(&text[*start..*end], body_width)
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
    let mode = modes.reasoning;
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    let tool_expanded = expanded(entry, modes.tools.fold == tools::ToolFold::Expanded);
    let reasoning_expanded = expanded(entry, mode == ReasoningMode::Expanded);
    match (entry.kind, mode) {
        (EntryKind::Reasoning, ReasoningMode::Hidden) if !reasoning_expanded => Vec::new(),
        (EntryKind::Reasoning, ReasoningMode::Collapsed) if !reasoning_expanded => {
            vec![BodyLine::prose(Vec::new(), 0, placeholder(entry))]
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
                        lines.extend(prose_lines(&text[*start..*end], body_width));
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
                                    lines.extend(code_lines(source, *start, *closed, width));
                                }
                            }
                        } else {
                            lines.extend(code_lines(source, *start, *closed, width));
                        }
                    }
                }
            }
            lines
        }
        (EntryKind::ToolCall, _) => {
            // One header line, laid out like a code line: never wrapped, truncated with `…`
            // when it does not fit. The entry's text **is** this header (set at push time
            // from the same function), so the characters map back and copy correctly.
            tool_line(&text, width)
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
                }]
            }
        }
        (EntryKind::ToolResult, _) => {
            let card = entry.tool.as_deref();
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

/// One tool header line: the same fit rule as a code line (a line exactly `width` wide shows
/// in full; anything wider is cut and gets `…`), with the characters mapped to the entry's
/// text so selection and copy work on it.
fn tool_line(text: &str, width: usize) -> Vec<BodyLine> {
    let (rendered, kept, cut) = crate::blocks::code_line_layout(text, width);
    let mut chars: Vec<usize> = kept;
    if cut && let Some(&last) = chars.last() {
        chars.push(last);
    }
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
    }]
}

/// Turns a tool body plan into laid-out lines: a text line wraps like prose at the body width
/// (its characters keep their source offsets), a hint is one synthesized line.
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
            let body = chars
                .iter()
                .map(|&i| text[i..].chars().next().expect("a char boundary"))
                .collect();
            BodyLine::prose(chars, end, body)
        })
        .collect()
}

/// The streaming spinner: a deterministic function of the text's length, so a delta batch
/// (which changes the length and dirties the frame) is the only thing that turns it.
const SPINNER: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];

/// The collapsed reasoning placeholder (ADR 0028 R3). `live` entries get the spinner; the
/// line count stays 1 either way.
fn placeholder(entry: &Entry) -> String {
    let lines = entry.text.lines().count().max(1);
    if entry.live {
        format!(
            "▸ thinking ({lines} lines) {}",
            SPINNER[entry.text.len() % 4]
        )
    } else {
        format!("▸ thinking ({lines} lines)")
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
                        lines.extend(crate::render::wrap(&text[start..end], body_width));
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
                        out.extend(crate::render::wrap(&text[source.clone()], body_width));
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
fn code_lines(raw: &str, offset: usize, closed: bool, width: usize) -> Vec<BodyLine> {
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
    for line in body {
        // Byte offset of the line's first char within the display text.
        let offset = line.as_ptr() as usize - base + offset;
        let (text, kept, cut) = code_line_layout(line, width);
        let mut chars: Vec<usize> = kept.into_iter().map(|idx| offset + idx).collect();
        if cut {
            // The ellipsis is decoration; map it to the last kept char.
            if let Some(&last) = chars.last() {
                chars.push(last);
            }
        }
        let end = offset + line.len();
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
    };

    /// The given reasoning mode with the tools expanded.
    fn modes_with(reasoning: ReasoningMode) -> DisplayModes {
        DisplayModes {
            reasoning,
            ..EXPANDED
        }
    }

    fn assistant(text: &str) -> Entry {
        Entry::new(EntryKind::Assistant, text)
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
            vec![base, base + 1, base + 1, base + 1, base + 1, base + 2]
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
        assert!(lines[0].text.contains("▸ thinking (3 lines)"));
        assert!(
            lines[0]
                .text
                .chars()
                .last()
                .is_some_and(|c| SPINNER.contains(&c))
        );
        entry.live = false;
        let lines = layout(&entry, &[], modes_with(ReasoningMode::Collapsed), 40);
        assert_eq!(lines[0].text, "▸ thinking (3 lines)");
    }
}

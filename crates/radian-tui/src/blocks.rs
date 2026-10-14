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

use unicode_width::UnicodeWidthStr;

use crate::app::{Entry, EntryKind};

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
}

impl BodyLine {
    fn prose(chars: Vec<usize>, end: usize, text: String) -> Self {
        Self {
            chars,
            end,
            text,
            code: false,
            pad: false,
        }
    }

    fn pad(end: usize) -> Self {
        Self {
            chars: Vec::new(),
            end,
            text: String::new(),
            code: false,
            pad: true,
        }
    }
}

/// How many display lines an entry's body lays out to (including padding rows; a `Hidden`
/// reasoning entry contributes zero). **Counts only**: no line is materialized, so the frame
/// that measures every entry (the first one, or the first after a resize) does not allocate
/// per entry — the virtualization budget (ADR 0028 R7) depends on this.
pub fn line_count(entry: &Entry, mode: ReasoningMode, width: usize) -> usize {
    if let Some(count) = cheap_count(entry, mode) {
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
        EntryKind::Assistant => parse_blocks_for(entry, mode)
            .iter()
            .map(|block| match block {
                Block::Paragraph { start, end, .. } => {
                    crate::render::wrap_line_count(&text[*start..*end], body_width)
                }
                Block::Code {
                    start, end, closed, ..
                } => code_line_count(&text[*start..*end], *closed),
            })
            .sum(),
        _ => crate::render::wrap_line_count(&text, body_width),
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
pub fn line_count_cached(entry: &mut Entry, mode: ReasoningMode, width: usize) -> usize {
    let text_len = entry.text.len();
    let key = (width, text_len, mode);
    if let Some(lines) = entry
        .layout_cache
        .as_ref()
        .filter(|(cached, _)| *cached == key)
        .map(|(_, lines)| *lines)
    {
        return lines;
    }
    let lines = line_count(entry, mode, width);
    entry.layout_cache = Some((key, lines));
    lines
}

/// Counts that need no layout pass at all.
fn cheap_count(entry: &Entry, mode: ReasoningMode) -> Option<usize> {
    match (entry.kind, mode) {
        (EntryKind::Reasoning, ReasoningMode::Hidden) => Some(0),
        (EntryKind::Reasoning, ReasoningMode::Collapsed) => Some(1),
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
pub fn layout(entry: &Entry, blocks: &[Block], mode: ReasoningMode, width: usize) -> Vec<BodyLine> {
    let prefix = crate::render::marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let text = crate::render::display_text(entry);
    match (entry.kind, mode) {
        (EntryKind::Reasoning, ReasoningMode::Hidden) => Vec::new(),
        (EntryKind::Reasoning, ReasoningMode::Collapsed) => {
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
            for block in blocks {
                match block {
                    Block::Paragraph { start, end, .. } => {
                        lines.extend(prose_lines(&text[*start..*end], body_width));
                    }
                    Block::Code {
                        start, end, closed, ..
                    } => {
                        lines.extend(code_lines(&text[*start..*end], *start, *closed, width));
                    }
                }
            }
            lines
        }
        _ => prose_lines(&text, body_width),
    }
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
        let mut chars = Vec::new();
        let mut text = String::new();
        let mut used = 0usize;
        let mut cut = false;
        let limit = width.saturating_sub(1);
        for (idx, ch) in line.char_indices() {
            let mut expanded = [0; 4];
            let expanded: &str = if ch == '\t' {
                "    "
            } else {
                ch.encode_utf8(&mut expanded)
            };
            let w = UnicodeWidthStr::width(expanded);
            if width > 0 && used + w > limit {
                cut = true;
                break;
            }
            for _ in 0..expanded.chars().count() {
                chars.push(offset + idx);
            }
            text.push_str(expanded);
            used += w;
        }
        if cut {
            text.push('…');
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
        let lines = layout(&entry, &blocks, ReasoningMode::Expanded, 10);
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
        let lines = layout(&entry, &blocks, ReasoningMode::Expanded, 20);
        assert_eq!(lines[0].text, "a    b");
        let base = entry.text.find('a').expect("the line is in the text");
        assert_eq!(
            lines[0].chars,
            vec![base, base + 1, base + 1, base + 1, base + 1, base + 2]
        );
    }

    #[test]
    fn cjk_widths_count_in_the_truncation() {
        let entry = assistant("```\n日本語テキスト\n```");
        let blocks = parse_blocks(&entry.text);
        // Width 7: 日(2)本(2)語(2) = 6, テ would exceed → cut after 語.
        let lines = layout(&entry, &blocks, ReasoningMode::Expanded, 7);
        assert_eq!(lines[0].text, "日本語…");
    }

    #[test]
    fn a_user_block_gets_padding_rows_and_they_map_to_gaps() {
        let entry = Entry::new(EntryKind::User, "hello");
        let lines = layout(&entry, &[], ReasoningMode::Expanded, 30);
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
        assert_eq!(line_count(&entry, ReasoningMode::Collapsed, 40), 1);
        assert_eq!(line_count(&entry, ReasoningMode::Hidden, 40), 0);
        assert_eq!(line_count(&entry, ReasoningMode::Expanded, 40), 3);
        // The collapsed placeholder carries the spinner while streaming.
        let lines = layout(&entry, &[], ReasoningMode::Collapsed, 40);
        assert!(lines[0].text.contains("▸ thinking (3 lines)"));
        assert!(
            lines[0]
                .text
                .chars()
                .last()
                .is_some_and(|c| SPINNER.contains(&c))
        );
        entry.live = false;
        let lines = layout(&entry, &[], ReasoningMode::Collapsed, 40);
        assert_eq!(lines[0].text, "▸ thinking (3 lines)");
    }
}

//! Markdown and code-block layout (ADR 0028 R2/R9): the line-building half of the display
//! model. `blocks/mod.rs` owns the block model, the caches and the line counts; this module
//! turns a Markdown paragraph or a fenced code block into [`BodyLine`]s.

use super::*;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One Mermaid block's render result. `Failed` keeps the reason for the dim line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiagramOutcome {
    Rendered(Vec<Vec<(String, DiagramRole)>>),
    Failed(String),
}

/// Renders `source` at `width` (ADR 0042): a pure function of the two, so the cache key is
/// exactly that pair. No partial diagram: the renderer either returns every line or a reason.
/// `grok_mermaid::render` is the one entry point (the vendored grok-build renderer); a failure
/// keeps the source and names the reason.
pub(crate) fn diagram_for(source: &str, width: usize) -> DiagramOutcome {
    let t = std::time::Instant::now();
    let rendered = grok_mermaid::render(source, Some(width));
    radian_session::trace::took("first-frame/mermaid", t);
    match rendered {
        Ok(art) => DiagramOutcome::Rendered(
            art.lines
                .into_iter()
                .map(|line| {
                    line.into_iter()
                        .map(|(role, text)| {
                            // The vendored renderer's five roles collapse to three (ADR 0042
                            // decision 4); a run with no role (leading spaces) is plain text.
                            let role = match role {
                                Some(grok_mermaid::Role::Node) => DiagramRole::Node,
                                Some(grok_mermaid::Role::Edge) => DiagramRole::Edge,
                                Some(grok_mermaid::Role::Text) | None => DiagramRole::Text,
                            };
                            (text, role)
                        })
                        .collect()
                })
                .collect(),
        ),
        Err(reason) => DiagramOutcome::Failed(reason_line(reason, width)),
    }
}

/// The failure line's reason (ADR 0042 decision 4) — the caller prefixes `mermaid: `. A drawing
/// that is merely too wide says so honestly; everything else is named.
fn reason_line(reason: grok_mermaid::Reason, width: usize) -> String {
    match reason {
        grok_mermaid::Reason::TooWide { needed } => format!("needs {needed} columns, have {width}"),
        grok_mermaid::Reason::Unsupported => "unsupported syntax".to_string(),
        grok_mermaid::Reason::TooLarge => "too large".to_string(),
    }
}

/// Whether a code block is a Mermaid diagram this renderer should draw: the info string is
/// `mermaid` (ASCII case-insensitive — CommonMark keeps info strings case-sensitive, but
/// every fence a model writes is lowercase; accepting any case is more forgiving and never
/// changes a non-mermaid block), the block is closed, and `tui.display.mermaid` is on.
pub(super) fn is_mermaid(lang: &str, closed: bool, mermaid: bool) -> bool {
    mermaid && closed && lang.eq_ignore_ascii_case("mermaid")
}

/// The outcome for one Mermaid block, from the entry's cache when the `(width, text_len)` key
/// matches, else rendered fresh (the immutable paths — selection, search, the reference
/// renderer — may not have run the mutable measure pass).
pub(super) fn diagram_outcome(
    entry: &Entry,
    index: usize,
    source: &str,
    width: usize,
) -> DiagramOutcome {
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
pub(super) fn code_body(raw: &str, closed: bool) -> String {
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
pub(super) fn mermaid_sources(blocks: &[Block], text: &str, mermaid: bool) -> Vec<String> {
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
/// How many display lines a fenced code block lays out to: its body's line count plus the
/// frame lines (the fences are markup). Mirrors [`code_lines`]'s skipping exactly.
pub(super) fn code_line_count(raw: &str, closed: bool) -> usize {
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
/// Wraps prose into body lines (the [`crate::render::wrap_char_offsets`] view).
pub(super) fn prose_lines(text: &str, width: usize) -> Vec<BodyLine> {
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
/// One code source line laid out for `width`: the rendered text (tab-expanded to 4 spaces,
/// ADR 0028 R9), the char indices of `line` that survived (each expanded cell maps to its
/// source char), and whether an ellipsis replaced a tail. A line that **fits** — including
/// one exactly `width` columns wide — renders as-is; only a line that does not fit keeps
/// `width - 1` columns and ends in `…`. Shared by the offset-mapping layout and the text-only
/// search path, so the two can never show different text.
pub(crate) fn code_line_layout(line: &str, width: usize) -> (String, Vec<usize>, bool) {
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
pub(super) fn code_text_lines(raw: &str, closed: bool, width: usize) -> Vec<String> {
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
/// The code frame's top line (ADR 0035): `╭─ <lang> ───…`, or `╭───…`
/// when the block has no language, filled to `width`. Synthesized: it has no source offset,
/// so it never selects or copies (like the fence it replaces).
pub(super) fn code_frame_top(
    lang: &str,
    start: usize,
    width: usize,
    glyphs: &MdGlyphs,
) -> BodyLine {
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
pub(super) fn code_frame_bottom(start: usize, glyphs: &MdGlyphs) -> BodyLine {
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
pub(super) fn code_lines(
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
pub(super) struct MdGlyphs {
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
    pub(super) fn for_glyphs(glyphs: crate::footer::Glyphs) -> Self {
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
            // The emoji tier draws the unicode set (ADR 0027).
            crate::footer::Glyphs::Unicode | crate::footer::Glyphs::Emoji => Self {
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
pub(super) fn md_lines_cached(
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
pub(super) fn md_line_count_cached(
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
pub(super) fn fill_md_count_cache(
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
pub(super) fn syntax_lines_cached(
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
pub(super) fn md_chars(pieces: &[MdPiece]) -> Vec<Option<usize>> {
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

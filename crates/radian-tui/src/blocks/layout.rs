//! Laying the block model out: the count-only measure path ([`line_count`],
//! [`line_count_cached`], [`cheap_count`]) and the laying-out path ([`layout`],
//! [`body_text_lines`]). Both read the same [`tool_plan`], so the line count the frame
//! measures with and the lines it draws cannot disagree.

use super::*;

/// How many display lines an entry's body lays out to (a `Hidden` reasoning entry contributes
/// zero). **Counts only**: no line is materialized, so the frame
/// that measures every entry (the first one, or the first after a resize) does not allocate
/// per entry — the virtualization budget (ADR 0028 R7) depends on this.
pub fn line_count(entry: &Entry, modes: DisplayModes, width: usize) -> usize {
    let mode = modes.reasoning;
    // The `ask_user` block is not a card: its shape follows the card's arguments and result
    // (ADR 0024), and its result entry is blank when it closes the call. Count it
    // through the layout so the total always equals the drawing.
    // The `/context` grid entry (spec `tui-context-grid`): the view builder
    // lays it out from the structured data, so the count always equals the
    // drawing.
    if entry.kind == EntryKind::Context {
        return layout(entry, &[], modes, width).len();
    }
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
        // ), so the count must use the same one the draw does. Only the costly color
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
        // The head: the command's first line (wrapped, its continuation rows `│ `) plus, when
        // the block is fully expanded, the command's other lines (ADR 0035's 2026-10-16
        // "多行命令" note). The meta and the status glyph ride on the first row.
        EntryKind::ToolCall => head_line_count(entry, &text, body_width, modes),
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
            // The thin user row (ADR 0035): `❯ 文本`, no background,
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
            // ADR 0024 Q7: an `ask_user` call is the question record, with no
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
            // ADR 0024 Q7: the result of `ask_user` is the same block, now with the
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
        (EntryKind::Context, _) => super::context_lines(entry, modes, width),
        _ => prose_lines(&text, body_width),
    }
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
    // The `/context` grid entry (spec `tui-context-grid`): the view builder's
    // own strings, shared with the laid-out lines.
    if entry.kind == EntryKind::Context {
        return super::context_texts(entry, modes, width);
    }
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
pub(crate) mod tests {
    use super::*;
    use crate::tools::{ToolDisplay, ToolFold};

    /// Both folds expanded: a geometry test that does not want the fold defaults.
    pub(crate) const EXPANDED: DisplayModes = DisplayModes {
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
        // ADR 0035: the user row is `❯ 文本` with no background and
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
        // ).
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
        // The expanded header is a harness line of its own; the `◻ ` is the marker column's.
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

    /// ADR 0035: a word that does not fit the rest of the row moves whole to the next one (the
    /// brief's long paragraphs); a single word wider than the row still breaks at the column.
    #[test]
    fn a_word_that_does_not_fit_moves_whole_to_the_next_row() {
        let texts = md("aaa bbb ccc", 7, crate::footer::Glyphs::Unicode);
        assert_eq!(texts, ["aaa bbb", "ccc"], "{texts:?}");
        let texts = md("aaa bbbb ccc", 7, crate::footer::Glyphs::Unicode);
        assert_eq!(texts, ["aaa", "bbbb", "ccc"], "{texts:?}");
        let texts = md("aaa bbbbbbbb", 7, crate::footer::Glyphs::Unicode);
        assert_eq!(texts, ["aaa", "bbbbbbb", "b"], "{texts:?}");
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
}

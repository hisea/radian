//! The `/context` grid view (spec `tui-context-grid`, prototype B).
//!
//! The host computes one [`ContextReport`](radian_host::context_estimate::ContextReport)
//! per `/context`; this module lays it out for the transcript. One entry shows the
//! 10 × 20 grid (one cell per 0.5% of the window) beside the legend, with the
//! per-item detail below. Narrow terminals stack the legend under the grid, and
//! terminals too narrow for the grid fall back to the plain-text notices the
//! host renders from the same report, so the two can never disagree on numbers.
//!
//! Everything the transcript shows about a grid entry goes through here: the
//! laid-out [`BodyLine`]s and the text-only lines share the one view builder,
//! and a cell the renderer synthesized (grid art, padding, the `…`) maps to
//! `None`, so it never copies or anchors a selection.

use radian_host::context_estimate::{ContextReport, Details, format_tokens, notice_lines};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::markdown::md_chars;
use super::{BodyLine, CtxTone, MdPiece, MdStyle};
use crate::app::{Entry, EntryKind};
use crate::footer::Glyphs;
use crate::tools::DisplayModes;

/// One grid cell per share of the window, and the grid's own shape.
pub const CELLS: usize = 200;
/// Grid columns (rows follow: exactly 200 cells fill 10 rows).
pub const GRID_COLS: usize = 20;
/// A grid row's display width: 20 cells plus the 19 spaces between them.
pub const GRID_WIDTH: usize = GRID_COLS * 2 - 1;
/// The gap between the grid and the legend in side-by-side mode.
const SIDE_GAP: usize = 3;
/// How many context-file rows the default view lists before `+N more`.
const MAX_FILES: usize = 8;
/// The source glyph: rows are built (and the entry text is stored) with the
/// unicode grid glyphs; [`shift`] swaps the tier's glyphs in at layout time.
const SOURCE_USED: char = '◼';

/// The structured data a grid entry carries (besides its text).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    pub report: ContextReport,
    /// `/context all`: list every context file instead of the first few.
    pub all: bool,
}

/// The grid's share of one fill part: its tone, its tokens, its cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub tone: CtxTone,
    pub tokens: u64,
    pub cells: usize,
}

/// The grid allocation: the fill parts in fill order (categories, free, buffer
/// last) and whether the buffer is drawn at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alloc {
    pub shares: Vec<Share>,
    /// False when the threshold (or the window) is unknown: no buffer cells,
    /// and the legend says so.
    pub buffer_drawn: bool,
}

/// The estimated category's own tone. Unknown names fall back to `System` (the
/// host's category set is closed; a test pins that every reported category
/// maps).
fn tone_of(name: &str) -> CtxTone {
    match name {
        "system prompt" => CtxTone::System,
        "context files" => CtxTone::Files,
        "memory" => CtxTone::Memory,
        "tool definitions" => CtxTone::Tools,
        "skills" => CtxTone::Skills,
        "MCP tools" => CtxTone::Mcp,
        "user messages" => CtxTone::User,
        "assistant text" => CtxTone::Assistant,
        "reasoning" => CtxTone::Reasoning,
        "tool results" => CtxTone::Results,
        _ => CtxTone::System,
    }
}

/// Largest-remainder cells over exactly [`CELLS`] cells of `window`. A part
/// with content gets at least one cell, so nothing vanishes. The buffer goes
/// last. `None` when the window is unknown: without a scale there is no grid,
/// only the plain-text fallback.
pub fn allocate(report: &ContextReport) -> Option<Alloc> {
    let window = report.window.filter(|window| *window > 0)?;
    let mut parts: Vec<(CtxTone, u64)> = report
        .categories
        .iter()
        .map(|category| (tone_of(category.name), category.tokens))
        .collect();
    parts.push((CtxTone::Free, report.free_tokens.unwrap_or(0)));
    // The buffer needs both the window and the threshold (spec: draw no buffer
    // when either is unknown, and say so in the legend).
    let buffer_drawn = report.threshold.is_some();
    if buffer_drawn {
        parts.push((CtxTone::Buffer, report.buffer_tokens.unwrap_or(0)));
    }
    let full = window as u128;
    let mut cells: Vec<usize> = parts
        .iter()
        .map(|(_, tokens)| (*tokens as u128 * CELLS as u128 / full) as usize)
        .collect();
    for (index, (_, tokens)) in parts.iter().enumerate() {
        if *tokens > 0 && cells[index] == 0 {
            cells[index] = 1;
        }
    }
    while cells.iter().sum::<usize>() > CELLS {
        let index = cells
            .iter()
            .enumerate()
            .max_by_key(|(_, count)| **count)
            .map(|(index, _)| index)
            .unwrap_or(0);
        cells[index] -= 1;
    }
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(parts[index].1 as u128 * CELLS as u128 % full));
    let mut round = 0;
    while cells.iter().sum::<usize>() < CELLS {
        cells[order[round % order.len()]] += 1;
        round += 1;
    }
    Some(Alloc {
        shares: parts
            .into_iter()
            .zip(cells)
            .map(|((tone, tokens), cells)| Share {
                tone,
                tokens,
                cells,
            })
            .collect(),
        buffer_drawn,
    })
}

/// The grid glyphs for a glyph set: used, free, buffer. The unicode tier draws
/// the spec's own `◼`/`◻`/`▨`; ascii gets its own set (`#`/`-`/`=`).
fn grid_glyphs(glyphs: Glyphs) -> (char, char, char) {
    match glyphs {
        Glyphs::Ascii => ('#', '-', '='),
        Glyphs::Unicode | Glyphs::Nerd | Glyphs::Emoji => ('◼', '◻', '▨'),
    }
}

/// One laid-out row: its rendered text, its per-char cells (character, style,
/// byte offset into the entry's text **line**, `None` when synthesized), and
/// the entry-text line it copies from (`None` for grid art, blanks and the
/// fallback).
struct Row {
    text: String,
    cells: Vec<(char, MdStyle, Option<usize>)>,
    source: Option<usize>,
}

impl Row {
    fn blank() -> Self {
        Self {
            text: String::new(),
            cells: Vec::new(),
            source: None,
        }
    }
}

/// Shares of `whole` in percent, one decimal.
fn pct(part: u64, whole: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// Builds the entry a `/context` pushes: the source text (legend and detail
/// rows, in order) plus the structured view. The grid art is decoration — its
/// cells map to `None` — while the legend and detail rows keep their source
/// offsets, so a copy carries the numbers.
pub fn context_entry(report: &ContextReport, all: bool) -> Entry {
    let view = ContextView {
        report: report.clone(),
        all,
    };
    let (legend, details) = stacked_rows(&view);
    let text: Vec<&str> = legend
        .iter()
        .chain(details.iter())
        .filter(|row| row.source.is_some())
        .map(|row| row.text.as_str())
        .collect();
    let mut entry = Entry::new(EntryKind::Context, text.join("\n"));
    entry.context = Some(view);
    entry
}

/// The transcript lines of a grid entry at this width and glyph set. The same
/// strings [`context_lines`] lays out, so the search geometry and the drawing
/// cannot disagree.
pub fn context_texts(entry: &Entry, modes: DisplayModes, width: usize) -> Vec<String> {
    context_lines(entry, modes, width)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// Lays a grid entry out at this width and glyph set.
pub fn context_lines(entry: &Entry, modes: DisplayModes, width: usize) -> Vec<BodyLine> {
    let Some(view) = &entry.context else {
        return super::markdown::prose_lines(&entry.text, body_width(width));
    };
    // The grid needs the window: without one there is no scale, only text.
    let Some(alloc) = allocate(&view.report) else {
        return fallback_lines(view, body_width(width));
    };
    let body = body_width(width);
    let (legend, mut details) = stacked_rows(view);
    let tones = flat_tones(&alloc);
    debug_assert_eq!(tones.len(), CELLS);
    let grid: Vec<Row> = tones.chunks(GRID_COLS).map(grid_row).collect();
    let legend_width = legend.iter().map(|row| row.text.width()).max().unwrap_or(0);
    truncate_detail_rows(&mut details, body);
    // Byte starts of the entry's source lines, in source order, with the text
    // length as the sentinel.
    let mut starts = Vec::new();
    let mut at = 0;
    for line in entry.text.split('\n') {
        starts.push(at);
        at += line.len() + 1;
    }
    starts.push(entry.text.len());
    if body >= GRID_WIDTH + SIDE_GAP + legend_width {
        side_by_side(&grid, &legend, &details, &starts, modes.glyphs)
    } else if body >= GRID_WIDTH {
        stacked(&grid, &legend, &details, &starts, modes.glyphs)
    } else {
        fallback_lines(view, body)
    }
}

/// The marker column is two cells wide, like every other entry.
fn body_width(width: usize) -> usize {
    width.saturating_sub(2).max(1)
}

/// Assigns source indices to the non-blank rows, in order: the entry's text is
/// exactly these rows joined.
fn indexed(mut rows: Vec<Row>) -> Vec<Row> {
    let mut next = 0;
    for row in rows.iter_mut() {
        if !row.text.is_empty() {
            row.source = Some(next);
            next += 1;
        }
    }
    rows
}

/// Appends one laid-out row to `rows`.
fn push_row(rows: &mut Vec<Row>, text: String, pieces: Vec<(String, MdStyle, usize)>) {
    rows.push(absolutize(&text, pieces));
}

/// The legend rows of the stacked arrangement, with source offsets relative to
/// their own rows (the layout absolutizes them against the entry's text).
fn stacked_rows(view: &ContextView) -> (Vec<Row>, Vec<Row>) {
    let report = &view.report;
    let mut legend = Vec::new();
    if let Some(model) = &report.model {
        push_row(
            &mut legend,
            model.clone(),
            vec![(model.clone(), MdStyle::Bold, 0)],
        );
    }
    if let Some(provider) = &report.provider {
        push_row(
            &mut legend,
            provider.clone(),
            vec![(provider.clone(), MdStyle::Muted, 0)],
        );
    }
    // `<used> / <window> tokens (<pct>)` reads estimated (ADR 0015 C1); the
    // provider's last reported input is the separate line below it.
    let window = report.window.unwrap_or(0);
    let used = format!(
        "{} / {} tokens ({})",
        format_tokens(report.total_tokens),
        format_tokens(window),
        pct(report.total_tokens, window.max(1))
    );
    let used_text = format!("{used} estimated");
    let used_len = used.len();
    push_row(
        &mut legend,
        used_text,
        vec![
            (used, MdStyle::Plain, 0),
            (" estimated".to_owned(), MdStyle::Dim, used_len),
        ],
    );
    match report.reported {
        Some(input) => {
            let text = format!("{} input tokens", format_tokens(input));
            let text_len = text.len();
            push_row(
                &mut legend,
                format!("{text}  reported, last request"),
                vec![
                    (text, MdStyle::Plain, 0),
                    (
                        "  reported, last request".to_owned(),
                        MdStyle::Dim,
                        text_len,
                    ),
                ],
            );
        }
        None => push_row(
            &mut legend,
            "reported: none yet".to_owned(),
            vec![("reported: none yet".to_owned(), MdStyle::Dim, 0)],
        ),
    }
    legend.push(Row::blank());
    push_row(
        &mut legend,
        "estimated by category".to_owned(),
        vec![("estimated by category".to_owned(), MdStyle::Italic, 0)],
    );
    for category in &report.categories {
        let label = format!(
            "{}: {} ({})",
            category.name,
            format_tokens(category.tokens),
            pct(category.tokens, window.max(1))
        );
        let (text, pieces) = legend_row(&label, category.name, tone_of(category.name));
        push_row(&mut legend, text, pieces);
    }
    let free = report.free_tokens.unwrap_or(0);
    let free_label = format!(
        "free: {} ({})",
        format_tokens(free),
        pct(free, window.max(1))
    );
    let (text, pieces) = legend_row(&free_label, "free", CtxTone::Free);
    push_row(&mut legend, text, pieces);
    if report.threshold.is_some() {
        let buffer = report.buffer_tokens.unwrap_or(0);
        let buffer_label = format!(
            "compaction buffer: {} ({})",
            format_tokens(buffer),
            pct(buffer, window.max(1))
        );
        let (text, pieces) = legend_row(&buffer_label, "compaction buffer", CtxTone::Buffer);
        push_row(&mut legend, text, pieces);
    } else {
        push_row(
            &mut legend,
            "no compaction buffer (threshold unknown)".to_owned(),
            vec![(
                "no compaction buffer (threshold unknown)".to_owned(),
                MdStyle::Dim,
                0,
            )],
        );
    }
    let legend = indexed(legend);
    // Detail sources continue after the legend: the entry's text is the
    // sourced rows of both, joined.
    let mut details = detail_rows(view);
    let mut next = legend.iter().filter(|row| row.source.is_some()).count();
    for row in details.iter_mut() {
        if !row.text.is_empty() {
            row.source = Some(next);
            next += 1;
        }
    }
    (legend, details)
}

/// A legend row's text (source glyph included, so the offsets below are exact)
/// and pieces: the glyph (synthesized, never copied), the name in the text
/// color, the numbers one level weaker.
fn legend_row(label: &str, name: &str, tone: CtxTone) -> (String, Vec<(String, MdStyle, usize)>) {
    let glyph_len = SOURCE_USED.len_utf8() + 1;
    let name_end = glyph_len + name.len() + 2; // `◼ name: `
    let text = format!("{SOURCE_USED} {label}");
    (
        text,
        vec![
            (SOURCE_USED.to_string(), MdStyle::Ctx(tone), usize::MAX),
            (" ".to_owned(), MdStyle::Plain, usize::MAX),
            (format!("{name}: "), MdStyle::Plain, glyph_len),
            (label[name.len() + 2..].to_owned(), MdStyle::Muted, name_end),
        ],
    )
}

fn detail_header(rows: &mut Vec<Row>, title: &str, hint: Option<&str>) {
    let mut text = title.to_owned();
    let mut pieces = vec![(title.to_owned(), MdStyle::Bold, 0)];
    if let Some(hint) = hint {
        pieces.push((hint.to_owned(), MdStyle::Dim, text.len()));
        text.push_str(hint);
    }
    rows.push(absolutize(&text, pieces));
}

/// The detail rows below the grid: context files, MCP, skills, memory, and
/// the message/compaction count.
fn detail_rows(view: &ContextView) -> Vec<Row> {
    let report = &view.report;
    let details = &report.details;
    let mut rows: Vec<Row> = Vec::new();
    if !details.context_files.is_empty() {
        detail_header(&mut rows, "context files", None);
        let shown = if view.all {
            details.context_files.len()
        } else {
            MAX_FILES.min(details.context_files.len())
        };
        for file in details.context_files.iter().take(shown) {
            let tokens = format_tokens(file.tokens);
            let text = format!("{} · {tokens}", file.path);
            let path_end = file.path.len();
            rows.push(absolutize(
                &text,
                vec![
                    (file.path.clone(), MdStyle::Plain, 0),
                    (" · ".to_owned(), MdStyle::Dim, path_end),
                    (tokens, MdStyle::Muted, path_end + " · ".len()),
                ],
            ));
        }
        let rest = details.context_files.len() - shown;
        if rest > 0 {
            let text = format!("+{rest} more · /context all");
            rows.push(absolutize(&text, vec![(text.clone(), MdStyle::Dim, 0)]));
        }
        rows.push(Row::blank());
    }
    detail_header(&mut rows, "MCP", Some(" · /mcp"));
    let line = mcp_line(details);
    rows.push(absolutize(&line, vec![(line.clone(), MdStyle::Muted, 0)]));
    rows.push(Row::blank());
    detail_header(&mut rows, "skills", Some(" · /skills"));
    let line = skills_line(details);
    rows.push(absolutize(&line, vec![(line.clone(), MdStyle::Muted, 0)]));
    rows.push(Row::blank());
    detail_header(&mut rows, "memory", Some(" · /memory"));
    let line = memory_line(details);
    rows.push(absolutize(&line, vec![(line.clone(), MdStyle::Muted, 0)]));
    rows.push(Row::blank());
    let last = format!(
        "{} messages · {} this session",
        report.message_count,
        plural(report.compactions, "compaction", "compactions")
    );
    rows.push(absolutize(&last, vec![(last.clone(), MdStyle::Muted, 0)]));
    rows
}

fn mcp_line(details: &Details) -> String {
    format!(
        "{} · {} · {}",
        plural(details.mcp_servers, "server", "servers"),
        plural(details.mcp_tools_count, "tool", "tools"),
        format_tokens(details.mcp_tools_tokens)
    )
}

fn skills_line(details: &Details) -> String {
    format!(
        "{} · {}",
        plural(details.skills_count, "skill", "skills"),
        format_tokens(details.skills_tokens)
    )
}

fn memory_line(details: &Details) -> String {
    format!(
        "{} records · {}",
        details.memory_records,
        format_tokens(details.memory_tokens)
    )
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Gives pieces offsets relative to their own row. `usize::MAX` marks a
/// synthesized piece (grid art, padding, `…`).
fn absolutize(text: &str, pieces: Vec<(String, MdStyle, usize)>) -> Row {
    let mut cells = Vec::new();
    let mut built = String::new();
    for (piece, style, at) in pieces {
        if at == usize::MAX {
            for ch in piece.chars() {
                cells.push((ch, style, None));
                built.push(ch);
            }
            continue;
        }
        for (index, ch) in piece.char_indices() {
            cells.push((ch, style, Some(at + index)));
            built.push(ch);
        }
    }
    debug_assert_eq!(built, *text);
    Row {
        text: built,
        cells,
        source: None,
    }
}

/// One grid row: the tones' glyphs joined by spaces, all synthesized.
fn grid_row(chunk: &[CtxTone]) -> Row {
    let mut text = String::new();
    let mut cells = Vec::new();
    for (index, tone) in chunk.iter().enumerate() {
        if index > 0 {
            text.push(' ');
            cells.push((' ', MdStyle::Plain, None));
        }
        // The source glyph; the tier's own glyph goes in at layout time.
        text.push(SOURCE_USED);
        cells.push((SOURCE_USED, MdStyle::Ctx(*tone), None));
    }
    Row {
        text,
        cells,
        source: None,
    }
}

fn flat_tones(alloc: &Alloc) -> Vec<CtxTone> {
    let mut tones = Vec::with_capacity(CELLS);
    for share in &alloc.shares {
        tones.extend(std::iter::repeat_n(share.tone, share.cells));
    }
    tones
}

/// Lays one view row out: absolutizes its cells against the entry's source
/// line and swaps the tier's glyphs in.
fn lay_out(row: &Row, starts: &[usize], glyphs: Glyphs) -> BodyLine {
    let (used, free, buffer) = grid_glyphs(glyphs);
    let base = row.source.map(|index| starts[index]).unwrap_or(0);
    let pieces: Vec<MdPiece> = row
        .cells
        .iter()
        .map(|(ch, style, offset)| {
            let glyph = match style {
                MdStyle::Ctx(CtxTone::Free) => Some(free),
                MdStyle::Ctx(CtxTone::Buffer) => Some(buffer),
                MdStyle::Ctx(_) => Some(used),
                _ => None,
            };
            match glyph {
                Some(glyph) => MdPiece {
                    text: glyph.to_string(),
                    style: *style,
                    offset: None,
                },
                None => MdPiece {
                    text: ch.to_string(),
                    style: *style,
                    offset: offset.map(|at| base + at),
                },
            }
        })
        .collect();
    let end = row
        .source
        .map(|index| base + source_line_len(starts, index))
        .unwrap_or(0);
    BodyLine::md(md_chars(&pieces), end, pieces)
}

/// The byte length of one entry-text line. `starts` ends with the text length
/// as a sentinel; every other boundary sits on a `\n`.
fn source_line_len(starts: &[usize], index: usize) -> usize {
    let Some(&start) = starts.get(index) else {
        return 0;
    };
    let Some(&next) = starts.get(index + 1) else {
        return 0;
    };
    if index + 2 == starts.len() {
        next.saturating_sub(start)
    } else {
        next.saturating_sub(start + 1)
    }
}

/// Side-by-side: each grid row with its legend row, then the leftover legend
/// rows (grid column blanked, so the legend column stays put), then details.
fn side_by_side(
    grid: &[Row],
    legend: &[Row],
    details: &[Row],
    starts: &[usize],
    glyphs: Glyphs,
) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let pad: String = " ".repeat(GRID_WIDTH + SIDE_GAP);
    let gap: String = " ".repeat(SIDE_GAP);
    for (index, grid_row) in grid.iter().enumerate() {
        let mut cells: Vec<(char, MdStyle, Option<usize>)> = grid_row.cells.clone();
        cells.extend(gap.chars().map(|ch| (ch, MdStyle::Plain, None)));
        let mut row = Row {
            text: String::new(),
            cells,
            source: None,
        };
        if let Some(legend_row) = legend.get(index) {
            row.cells.extend(legend_row.cells.clone());
            row.source = legend_row.source;
        }
        row.text = grid_text(grid_row, legend.get(index), glyphs);
        out.push(lay_out(&row, starts, glyphs));
    }
    for legend_row in legend.iter().skip(grid.len()) {
        let mut row = Row {
            text: String::new(),
            cells: pad.chars().map(|ch| (ch, MdStyle::Plain, None)).collect(),
            source: None,
        };
        row.cells.extend(legend_row.cells.clone());
        row.source = legend_row.source;
        row.text = format!("{pad}{}", legend_text(legend_row, glyphs));
        out.push(lay_out(&row, starts, glyphs));
    }
    push_details(&mut out, details, starts, glyphs);
    out
}

fn grid_text(grid_row: &Row, legend_row: Option<&Row>, glyphs: Glyphs) -> String {
    let (used, free, buffer) = grid_glyphs(glyphs);
    let mut text: String = grid_row
        .cells
        .iter()
        .map(|(ch, style, _)| match style {
            MdStyle::Ctx(CtxTone::Free) => free,
            MdStyle::Ctx(CtxTone::Buffer) => buffer,
            MdStyle::Ctx(_) => used,
            _ => *ch,
        })
        .collect();
    text.push_str(&" ".repeat(SIDE_GAP));
    if let Some(legend_row) = legend_row {
        text.push_str(&legend_text(legend_row, glyphs));
    }
    text
}

fn legend_text(row: &Row, glyphs: Glyphs) -> String {
    let (used, free, buffer) = grid_glyphs(glyphs);
    row.cells
        .iter()
        .map(|(ch, style, _)| match style {
            MdStyle::Ctx(CtxTone::Free) => free,
            MdStyle::Ctx(CtxTone::Buffer) => buffer,
            MdStyle::Ctx(_) => used,
            _ => *ch,
        })
        .collect()
}

/// The stacked arrangement: grid rows, legend rows, a blank, details.
fn stacked(
    grid: &[Row],
    legend: &[Row],
    details: &[Row],
    starts: &[usize],
    glyphs: Glyphs,
) -> Vec<BodyLine> {
    let mut out = Vec::new();
    for grid_row in grid {
        out.push(lay_out(grid_row, starts, glyphs));
    }
    for legend_row in legend {
        if legend_row.text.is_empty() {
            out.push(blank_line());
            continue;
        }
        out.push(lay_out(legend_row, starts, glyphs));
    }
    push_details(&mut out, details, starts, glyphs);
    out
}

fn push_details(out: &mut Vec<BodyLine>, details: &[Row], starts: &[usize], glyphs: Glyphs) {
    // The detail sections sit below the grid and the legend, separated by one
    // blank row.
    out.push(blank_line());
    for row in details {
        if row.text.is_empty() {
            out.push(blank_line());
            continue;
        }
        out.push(lay_out(row, starts, glyphs));
    }
}

/// Too narrow for the grid: the plain-text notices from the same report,
/// wrapped to the body width. All synthesized: there is no source line for
/// them in the entry's text.
fn fallback_lines(view: &ContextView, body: usize) -> Vec<BodyLine> {
    let mut out = Vec::new();
    for notice in notice_lines(&view.report) {
        for fragment in wrap_plain(&notice, body) {
            let pieces: Vec<MdPiece> = fragment
                .chars()
                .map(|ch| MdPiece {
                    text: ch.to_string(),
                    style: MdStyle::Plain,
                    offset: None,
                })
                .collect();
            out.push(BodyLine::md(md_chars(&pieces), 0, pieces));
        }
    }
    out
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += cell;
    }
    out.push(current);
    out
}

/// Middle-truncates detail rows that do not fit, keeping both ends (the path
/// head and the tokens). Kept cells keep their offsets; the `…` is
/// synthesized.
fn truncate_detail_rows(details: &mut [Row], body: usize) {
    for row in details.iter_mut() {
        if row.text.is_empty() || row.text.width() <= body {
            continue;
        }
        let chars = std::mem::take(&mut row.cells);
        let keep_head = body / 3;
        let mut prefix = 0;
        let mut used = 0;
        for (index, (ch, _, _)) in chars.iter().enumerate() {
            used += ch.width().unwrap_or(0);
            if used <= keep_head {
                prefix = index + 1;
            } else {
                break;
            }
        }
        let mut suffix = chars.len();
        let mut used = 0;
        for (index, (ch, _, _)) in chars.iter().enumerate().rev() {
            used += ch.width().unwrap_or(0);
            if used < body - keep_head {
                suffix = index;
            } else {
                break;
            }
        }
        if suffix <= prefix {
            continue; // too narrow even for the ends; the renderer clips
        }
        let mut kept = Vec::new();
        let mut ellipsis = false;
        for (index, cell) in chars.into_iter().enumerate() {
            if index < prefix || index >= suffix {
                kept.push(cell);
            } else if !ellipsis {
                kept.push(('…', MdStyle::Dim, None));
                ellipsis = true;
            }
        }
        row.cells = kept;
        row.text = row.cells.iter().map(|(ch, _, _)| ch).collect();
    }
}

fn blank_line() -> BodyLine {
    BodyLine::md(md_chars(&[]), 0, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_host::context_estimate::{Category, FileDetail};

    pub(crate) fn sample_report() -> ContextReport {
        let categories = [
            ("system prompt", 1_900),
            ("context files", 3_600),
            ("memory", 800),
            ("tool definitions", 2_900),
            ("skills", 700),
            ("MCP tools", 1_400),
            ("user messages", 4_100),
            ("assistant text", 9_800),
            ("tool results", 36_100),
        ];
        ContextReport {
            provider: Some("opencode".to_owned()),
            model: Some("muse-spark-1.3-contributor".to_owned()),
            categories: categories
                .iter()
                .map(|(name, tokens)| Category {
                    name,
                    tokens: *tokens,
                    share: 0.0,
                })
                .collect(),
            total_tokens: 61_300,
            window: Some(200_000),
            threshold: Some(180_000),
            free_tokens: Some(118_700),
            buffer_tokens: Some(20_000),
            reported: Some(59_270),
            message_count: 58,
            compactions: 1,
            details: Details {
                context_files: vec![FileDetail {
                    path: "AGENTS.md".to_owned(),
                    tokens: 3_600,
                }],
                context_files_tokens: 3_600,
                skills_count: 9,
                skills_tokens: 700,
                builtin_tools_count: 12,
                builtin_tools_tokens: 2_900,
                mcp_servers: 1,
                mcp_tools_count: 16,
                mcp_tools_tokens: 1_400,
                memory_records: 4,
                memory_tokens: 800,
            },
        }
    }

    /// The allocation sums to exactly 200 cells, content gets at least one,
    /// and the buffer goes last.
    #[test]
    fn the_cells_sum_to_200_with_the_buffer_last() {
        let alloc = allocate(&sample_report()).expect("a windowed report allocates");
        assert!(alloc.buffer_drawn);
        assert_eq!(
            alloc.shares.iter().map(|share| share.cells).sum::<usize>(),
            200,
            "the grid is exactly 200 cells (spec)"
        );
        assert_eq!(alloc.shares.last().expect("shares").tone, CtxTone::Buffer);
        for share in &alloc.shares {
            if share.tokens > 0 {
                assert!(share.cells >= 1, "{share:?} has content but no cell");
            }
        }
        let tones = flat_tones(&alloc);
        assert_eq!(tones.len(), 200, "200 cells fill 10 rows of 20");
        assert_eq!(
            tones
                .iter()
                .filter(|tone| **tone == CtxTone::Buffer)
                .count(),
            20
        );
    }

    /// A tiny category keeps one cell instead of vanishing.
    #[test]
    fn a_tiny_category_keeps_one_cell() {
        let mut report = sample_report();
        report.categories[4].tokens = 1; // skills: a single token
        let alloc = allocate(&report).expect("a windowed report allocates");
        let skills = alloc
            .shares
            .iter()
            .find(|share| share.tone == CtxTone::Skills)
            .expect("skills");
        assert_eq!(skills.cells, 1);
        assert_eq!(
            alloc.shares.iter().map(|share| share.cells).sum::<usize>(),
            200,
            "the grid is exactly 200 cells (spec)"
        );
    }

    /// No threshold (or no window) draws no buffer, and the legend says so.
    #[test]
    fn no_threshold_means_no_buffer_and_a_legend_note() {
        let mut report = sample_report();
        report.threshold = None;
        report.buffer_tokens = None;
        let alloc = allocate(&report).expect("a window still allocates");
        assert!(!alloc.buffer_drawn);
        assert!(
            alloc
                .shares
                .iter()
                .all(|share| share.tone != CtxTone::Buffer)
        );
        assert_eq!(
            alloc.shares.iter().map(|share| share.cells).sum::<usize>(),
            200,
            "the grid is exactly 200 cells (spec)"
        );
        let entry = context_entry(&report, false);
        let lines = context_lines(&entry, DisplayModes::default(), 120);
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("no compaction buffer")),
            "the legend says why: {:?}",
            lines.iter().map(|line| &line.text).collect::<Vec<_>>()
        );
    }

    /// No window means no grid at all: the plain-text fallback.
    #[test]
    fn no_window_falls_back_to_plain_text() {
        let mut report = sample_report();
        report.window = None;
        report.free_tokens = None;
        report.buffer_tokens = None;
        assert!(allocate(&report).is_none());
        let entry = context_entry(&report, false);
        let lines = context_lines(&entry, DisplayModes::default(), 120);
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("context (estimated):"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("59") && line.text.contains("reported"))
        );
    }

    /// Every reported category name maps to its own tone (no silent fallback).
    #[test]
    fn every_category_name_maps_to_its_tone() {
        let report = sample_report();
        let expected = [
            ("system prompt", CtxTone::System),
            ("context files", CtxTone::Files),
            ("memory", CtxTone::Memory),
            ("tool definitions", CtxTone::Tools),
            ("skills", CtxTone::Skills),
            ("MCP tools", CtxTone::Mcp),
            ("user messages", CtxTone::User),
            ("assistant text", CtxTone::Assistant),
            ("tool results", CtxTone::Results),
        ];
        for (name, tone) in expected {
            assert!(
                report.categories.iter().any(|c| c.name == name),
                "{name} is reported"
            );
            assert_eq!(tone_of(name), tone, "{name}");
        }
        assert_eq!(tone_of("reasoning"), CtxTone::Reasoning);
    }

    /// Every real source offset points at its own character in the entry's
    /// text, so copies and selections map exactly.
    #[test]
    fn every_source_offset_points_at_its_own_char() {
        for (all, width, glyphs) in [
            (false, 120, Glyphs::Unicode),
            (false, 60, Glyphs::Unicode),
            (true, 80, Glyphs::Ascii),
            (false, 30, Glyphs::Unicode),
        ] {
            let report = sample_report();
            let entry = context_entry(&report, all);
            let modes = DisplayModes {
                glyphs,
                ..DisplayModes::default()
            };
            // The source rows are exactly the entry's text lines.
            let view = ContextView { report, all };
            let (legend, details) = stacked_rows(&view);
            let sourced: Vec<&str> = legend
                .iter()
                .chain(details.iter())
                .filter(|row| row.source.is_some())
                .map(|row| row.text.as_str())
                .collect();
            assert_eq!(sourced, entry.text.split('\n').collect::<Vec<_>>());
            let bytes = entry.text.as_bytes();
            for line in context_lines(&entry, modes, width) {
                for (ch, offset) in line.text.chars().zip(line.chars.iter()) {
                    let Some(offset) = offset else { continue };
                    let rest = &bytes[*offset..];
                    let mut buf = [0u8; 4];
                    let encoded = ch.encode_utf8(&mut buf);
                    assert!(
                        rest.starts_with(encoded.as_bytes()),
                        "offset {offset} of {ch:?} in {:?} misses in {:?}",
                        line.text,
                        entry.text
                    );
                }
            }
        }
    }
}

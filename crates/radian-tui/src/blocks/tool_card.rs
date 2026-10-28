//! The tool card's head and body layout (ADR 0028): the head row
//! (`<name> <title> · <meta> <status>`, folded or open), the `│ ` body rows a body plan lays
//! out, and the syntax-highlighted diff rows. `blocks/mod.rs` owns the block model and the line
//! counts; this module owns the card's own geometry.

use super::markdown::{code_line_layout, md_chars, prose_lines};
use super::*;
use unicode_width::UnicodeWidthChar;

/// The tool head's fold marker (ADR 0035's 2026-10-16 "折叠标记统一" note, option A):
/// the head row's marker is decided **only** by whether a body is drawn under it. The thinking
/// header carries the same pair ([`crate::footer::Glyphs::open_marker`] /
/// [`crate::footer::Glyphs::fold_marker`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadMarker {
    /// A head-only block: a running call, a successful `read`/`grep`/`glob`/`list` (one line
    /// whatever its result hides), or a body the layout stripped (a cancelled call). `• `
    /// (ascii `* `).
    Dot,
    /// A fold hides the body: `collapsed`'s `· N lines` head. `◼ ` (ascii `+ `).
    Folded,
    /// A body is drawn under the head: the `errors` preview's last rows, an error body, a
    /// `Ctrl-O`/search expansion, an expanded thinking channel. `◻ ` (ascii `- `).
    Open,
}

impl HeadMarker {
    /// The two-cell marker-column spelling under a glyph tier (`• `, `◼ `, `◻ `; ascii `* + -`).
    pub fn glyph(self, glyphs: crate::footer::Glyphs) -> &'static str {
        match self {
            HeadMarker::Dot => match glyphs {
                crate::footer::Glyphs::Ascii => "* ",
                _ => "• ",
            },
            HeadMarker::Folded => glyphs.fold_marker(),
            HeadMarker::Open => glyphs.open_marker(),
        }
    }
}

/// The marker a tool **head** row carries (ADR 0035's 2026-10-16 note, option A). `body_follows`
/// is the caller's: the head lives on the call entry and the body on the paired result entry, so
/// only the renderer, walking the transcript, knows whether the result lays out any line
/// ([`crate::render::visible_lines`] passes the pair's cached count).
///
/// The cases, in order:
///
/// * a body is drawn under the head → [`HeadMarker::Open`] (`◻`);
/// * a compact tool (`read`/`grep`/`glob`/`list`, [`crate::tools::ToolRender::compact`]) with no
///   body drawn → [`HeadMarker::Dot`] (`•`), however many lines its result hides: the head is
///   the whole block until an error or `Ctrl-O` draws a body;
/// * every **other** tool that is explicitly folded (`tui.display.tool_output = "collapsed"`)
///   with a hidden body → [`HeadMarker::Folded`] (`◼`);
/// * otherwise it is a one-liner (a running call, an empty or cancelled result) →
///   [`HeadMarker::Dot`] (`•`).
pub fn head_marker(entry: &Entry, modes: DisplayModes, body_follows: bool) -> HeadMarker {
    // A multi-line command fully expanded draws its own `│ ` rows under the head (ADR 0035's
    // 2026-10-16 "多行命令" note), so the head is open even while the paired result is absent.
    if body_follows || call_body_rows(entry, modes) {
        return HeadMarker::Open;
    }
    let card = entry.tool.as_deref();
    // A compact tool's successful call is one head line (ADR 0028): its
    // result is never laid out under `errors`, so the head is a `•` whatever the result's size,
    // never a fold (ADR 0035's 2026-10-16 note, option A).
    if card.is_some_and(|card| tools::renderer_for(&card.name).compact) {
        return HeadMarker::Dot;
    }
    // The head's own `· N lines` (`head_rows`) appears under exactly this condition, so the fold
    // marker is shown exactly when the head names the hidden count.
    let explicitly_folded =
        modes.tools.fold == tools::ToolFold::Collapsed && entry.fold_override != Some(true);
    // `result_lines` is the paired result's body line count (set at pairing time); it is `None`
    // while the call runs, so a running head is a one-liner.
    let hidden = card.and_then(|card| card.result_lines).unwrap_or(0);
    if explicitly_folded && hidden > 0 {
        HeadMarker::Folded
    } else {
        HeadMarker::Dot
    }
}
/// The head row(s) of a tool block (ADR 0028, ADR 0035's 2026-10-16
/// "多行命令" note, option A): `• <tool name> <title> · <meta> <status>`. The
/// entry's text is the `<name> <title>` part (the call entry's text, set at push time from the
/// same function) so those characters copy; the name's own style, the meta, the status glyph,
/// the `… (+N lines)` note and the `│ ` markers are synthesized cells (no source).
///
/// The **fold marker is on the first row only**. A multi-line `shell` command follows its
/// block: the first row carries the command's first line, the note and the meta; while the
/// block is not fully expanded the command's other lines are hidden behind `… (+N lines)` (N
/// is the command's remaining lines, never the output's); when it is fully expanded they
/// follow as `│ ` body rows, before the paired result's output. A command line wider than the
/// row wraps, its continuation rows `│ `-railed too - never a second fold marker. The meta and
/// the status glyph always sit at the end of the **first** row.
pub(super) fn head_rows(
    entry: &Entry,
    text: &str,
    width: usize,
    modes: DisplayModes,
) -> Vec<BodyLine> {
    let card = entry.tool.as_deref();
    let name = card.map(|card| card.name.as_str()).unwrap_or("");
    let meta = head_meta(entry, modes);
    // The name is the head text's own first word (`call_text` puts it there), so its cells keep
    // their source offsets and the whole head copies.
    let named = !name.is_empty() && text.starts_with(name);
    let lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    let last = lines.len() - 1;
    let expanded = call_body_expanded(entry, modes);
    let tail = head_tail(entry, modes, &meta, last, expanded);
    let mut offset = 0usize;
    let mut rows = styled_head_rows(
        head_line_cells(lines[0], offset, named, name.len()),
        tail,
        offset + lines[0].len(),
        width,
    );
    offset += lines[0].len() + 1;
    // The command's other lines: `│ ` body rows when the block is fully expanded, hidden
    // behind the head's `… (+N lines)` note otherwise.
    if expanded {
        for line in &lines[1..] {
            rows.extend(styled_body_rows(
                head_line_cells(line, offset, false, 0),
                offset + line.len(),
                width,
            ));
            offset += line.len() + 1;
        }
    }
    rows
}

/// Whether a tool call's command shows its own remaining lines as `│ ` body rows (ADR 0035's
/// 2026-10-16 "多行命令" note): the block is fully expanded and the command has more than one
/// line. The fold is `tool_plan`'s, so the head and the paired result never disagree - an
/// error's body is never hidden, in any mode.
fn call_body_expanded(entry: &Entry, modes: DisplayModes) -> bool {
    let fold = if entry
        .fold_override
        .unwrap_or(modes.tools.fold == tools::ToolFold::Expanded)
    {
        tools::ToolFold::Expanded
    } else {
        modes.tools.fold
    };
    entry.result == Some(true) || fold == tools::ToolFold::Expanded
}

/// Whether a tool call's head draws `│ ` rows of its own (its fully expanded multi-line
/// command). The renderer ORs it with the paired result's body to pick the head's fold marker:
/// a running call in `expanded` mode already draws its command's lines.
pub fn call_body_rows(entry: &Entry, modes: DisplayModes) -> bool {
    entry.kind == EntryKind::ToolCall
        && call_body_expanded(entry, modes)
        && entry.text.lines().count() > 1
}

/// The head's meta (ADR 0028): the card's own fields plus, under
/// `collapsed`, the count of the body rows the fold hid (the paired result's, which only its
/// own entry lays out: `ToolCard::result_lines`).
fn head_meta(entry: &Entry, modes: DisplayModes) -> String {
    let card = entry.tool.as_deref();
    let mut meta = card_meta(entry).unwrap_or_default();
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
    meta
}

/// The head's trailing cells (ADR 0035's 2026-10-16 "多行命令" note): the `… (+N lines)` note
/// (while the command's other lines are hidden), the ` · <meta>` and the status glyph. Every
/// cell is synthesized - none of it copies or anchors a selection - and the whole tail rides on
/// the head's **first** row, after the command's first line.
fn head_tail(
    entry: &Entry,
    modes: DisplayModes,
    meta: &str,
    last: usize,
    expanded: bool,
) -> Vec<HeadCell> {
    let mut tail: Vec<HeadCell> = Vec::new();
    if !expanded && last > 0 {
        for ch in format!(" … (+{last} lines)").chars() {
            tail.push((ch, MdStyle::Dim, None));
        }
    }
    if !meta.is_empty() {
        for ch in format!(" · {meta}").chars() {
            tail.push((ch, MdStyle::Dim, None));
        }
    }
    // An ACP tool call names its state (`queued`, `running`, `done`, `failed`) instead of
    // drawing the glyph: a `terminal` content block has no result to pair, so the glyph would
    // stay a running frame after the call has finished (ADR 0048 §13).
    let status = entry
        .tool
        .as_deref()
        .and_then(|card| card.status.as_deref());
    if let Some(status) = status {
        let style = match status {
            "done" => MdStyle::StatusOk,
            "failed" => MdStyle::StatusError,
            _ => MdStyle::Dim,
        };
        for ch in format!(" · {status}").chars() {
            tail.push((ch, style, None));
        }
    }
    let glyph = if status.is_some() {
        None
    } else {
        match head_status(entry, entry.tool.as_deref()) {
            HeadStatus::Ok => Some((status_marks(modes.glyphs).0, MdStyle::StatusOk)),
            HeadStatus::Error => Some((status_marks(modes.glyphs).1, MdStyle::StatusError)),
            HeadStatus::Running => Some((running_glyph(modes.glyphs), MdStyle::StatusRunning)),
            HeadStatus::None => None,
        }
    };
    if let Some((glyph, style)) = glyph {
        tail.push((' ', MdStyle::Plain, None));
        tail.push((glyph, style, None));
    }
    tail
}

/// One command line's cells: its chars, the tool name's own style on the head's first line, and
/// each char's source offset (`offset` is the line's byte offset in the entry's text).
fn head_line_cells(line: &str, offset: usize, named: bool, name_len: usize) -> Vec<HeadCell> {
    line.char_indices()
        .map(|(at, ch)| {
            let style = if named && at < name_len {
                MdStyle::ToolName
            } else {
                MdStyle::Plain
            };
            (ch, style, Some(offset + at))
        })
        .collect()
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

/// The status glyphs `✓`/`×` (ADR 0028 with the glyph
/// note: `×` replaces the crossed mark U+2717, which misses Cascadia Code, Fira Code and Source
/// Code Pro). `ascii` draws `+`/`x`; the `nerd` tier keeps the footer's Font Awesome pair. The
/// **running** state draws the live spinner frame instead ([`running_glyph`] is the
/// non-animated placeholder the layout carries).
pub(crate) fn status_marks(glyphs: crate::footer::Glyphs) -> (char, char) {
    match glyphs {
        crate::footer::Glyphs::Unicode => ('✓', '×'),
        crate::footer::Glyphs::Nerd => ('\u{f00c}', '\u{f00d}'),
        crate::footer::Glyphs::Ascii => ('+', 'x'),
        // The emoji set's ballot boxes, from the system emoji font (ADR 0027
        // decision 7).
        crate::footer::Glyphs::Emoji => ('✅', '❌'),
    }
}

/// The placeholder a running head row carries where the renderer then draws the live frame
/// (ADR 0027). Every spinner glyph is one cell, so the head's shape is stable.
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

/// One head row's cell: its character, its render role, and its source offset (`None` when the
/// renderer synthesized it - the `… (+N lines)` note, the meta, the status glyph).
type HeadCell = (char, MdStyle, Option<usize>);

/// A wrapped head row: its cells, and the byte offset its line ends at (the selection mapping's
/// view).
type HeadChunk = (Vec<HeadCell>, usize);

/// The head's first row and its wrapped continuation rows (ADR 0035's 2026-10-16 "多行命令"
/// note): `tail` (the note, the meta and the status) rides on the **first** row, which is
/// therefore `width - tail_width` wide; a wrapped continuation is a `│ ` body row.
fn styled_head_rows(
    cells: Vec<HeadCell>,
    tail: Vec<HeadCell>,
    end: usize,
    width: usize,
) -> Vec<BodyLine> {
    let tail_width: usize = tail.iter().map(|(ch, _, _)| cell_width(*ch)).sum();
    let first_width = width.saturating_sub(tail_width).max(1);
    wrap_cells(cells, first_width, width, end)
        .into_iter()
        .enumerate()
        .map(|(index, (chunk, chunk_end))| {
            if index == 0 {
                styled_row(chunk, tail.clone(), chunk_end, width, Frame::Head)
            } else {
                styled_row(chunk, Vec::new(), chunk_end, width, Frame::Body)
            }
        })
        .collect()
}

/// One command line's `│ ` body rows (ADR 0035's 2026-10-16 "多行命令" note): the line, wrapped
/// at `width`, each row a `Frame::Body` so the renderer rails it.
fn styled_body_rows(cells: Vec<HeadCell>, end: usize, width: usize) -> Vec<BodyLine> {
    wrap_cells(cells, width, width, end)
        .into_iter()
        .map(|(chunk, chunk_end)| styled_row(chunk, Vec::new(), chunk_end, width, Frame::Body))
        .collect()
}

/// The display width of one head cell: a tab expands to four cells (ADR 0028 R9).
fn cell_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

/// How many rows a head line wraps to: the first row is `first_width` cells wide (it also
/// carries the meta and the status, which never truncate), every later row `width`. The mirror
/// of [`wrap_cells`], so the measure pass counts exactly what the layout draws.
fn wrap_count(line: &str, first_width: usize, width: usize) -> usize {
    let mut rows = 1usize;
    let mut used = 0usize;
    let mut any = false;
    let mut limit = first_width.max(1);
    for ch in line.chars() {
        let w = cell_width(ch);
        if used + w > limit && any {
            rows += 1;
            used = 0;
            limit = width.max(1);
        }
        used += w;
        any = true;
    }
    rows
}

/// Splits a head line's cells into rows at `width` display cells, breaking at character
/// boundaries and keeping every character (spaces included, unlike prose). The first row is
/// `first_width` cells wide; every later row `width`. Each row keeps its own end offset, so the
/// selection mapping anchors a click past a wrapped row at the next character.
fn wrap_cells(
    cells: Vec<HeadCell>,
    first_width: usize,
    width: usize,
    end: usize,
) -> Vec<HeadChunk> {
    let mut rows: Vec<HeadChunk> = Vec::new();
    let mut current: Vec<HeadCell> = Vec::new();
    let mut used = 0usize;
    let mut limit = first_width.max(1);
    let mut chunk_end = end;
    for (ch, style, offset) in cells {
        let w = cell_width(ch);
        if used + w > limit && !current.is_empty() {
            rows.push((std::mem::take(&mut current), chunk_end));
            used = 0;
            limit = width.max(1);
            chunk_end = end;
        }
        if let Some(at) = offset {
            chunk_end = at + ch.len_utf8();
        }
        if ch == '\t' {
            for _ in 0..4 {
                current.push((' ', style, offset));
            }
        } else {
            current.push((ch, style, offset));
        }
        used += w;
    }
    rows.push((current, chunk_end));
    rows
}

/// How many rows [`head_rows`] lays a tool call's head out to: the mirror the measure pass
/// counts with, so the virtualized geometry and the drawing cannot disagree.
pub(super) fn head_line_count(
    entry: &Entry,
    text: &str,
    width: usize,
    modes: DisplayModes,
) -> usize {
    let meta = head_meta(entry, modes);
    let last = text.lines().count().saturating_sub(1);
    let expanded = call_body_expanded(entry, modes);
    let tail = head_tail(entry, modes, &meta, last, expanded);
    let tail_width: usize = tail.iter().map(|(ch, _, _)| cell_width(*ch)).sum();
    let first_width = width.saturating_sub(tail_width).max(1);
    let mut count = 0usize;
    let mut first = true;
    for line in text.lines() {
        if first {
            count += wrap_count(line, first_width, width);
            first = false;
        } else if expanded {
            count += wrap_count(line, width, width);
        } else {
            break;
        }
    }
    count.max(1)
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
pub(super) fn body_plan_lines(
    plan: &[tools::BodyLine],
    text: &str,
    body_width: usize,
) -> Vec<BodyLine> {
    let highlights = diff_highlights(plan, text);
    let mut out = Vec::new();
    for (index, line) in plan.iter().enumerate() {
        match line {
            tools::BodyLine::Text { source, role } => {
                let slice = text.get(source.clone()).unwrap_or("");
                for mut laid in prose_lines(slice, body_width) {
                    // `prose_lines` maps offsets within the slice; lift them onto the entry's
                    // own text, so a selection anchors at (and copies) the result's real
                    // characters rather than the slice's first line.
                    for offset in laid.chars.iter_mut().flatten() {
                        *offset += source.start;
                    }
                    laid.end += source.start;
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
pub(super) struct DiffHighlight {
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

/// A diff row's code: its text without the sign. A row whose sign is synthesized (`write`) has
/// no sign in its source, so the whole slice is the code.
fn diff_code<'a>(row: &tools::DiffRow, text: &'a str) -> &'a str {
    let slice = text.get(row.source.clone()).unwrap_or("");
    if row.signed {
        let sign_len = slice.chars().next().map_or(0, char::len_utf8);
        &slice[sign_len..]
    } else {
        slice
    }
}

/// Lays one diff row out (ADR 0028): `│ `-free (the card's frame draws the
/// marker column), the line number right-aligned in four cells plus a space, the sign, a space,
/// then the code. The number and the two spaces are **synthesized**: they are drawn but carry no
/// source offset, so a copy of the row is the row's own text — the sign and the code — exactly as
/// it was before. The code never wraps: the row is truncated to `width` with `…` like a code line
/// (R9), and the pieces of a highlighted code line take the syntax roles (the same table a code
/// block uses).
pub(super) fn diff_line(
    row: &tools::DiffRow,
    highlight: Option<&DiffHighlight>,
    text: &str,
    width: usize,
) -> BodyLine {
    let slice = text.get(row.source.clone()).unwrap_or("");
    // The sign is the row's own first byte for a unified diff; a `write` row's text is the code
    // alone, so its sign is synthesized from the row's kind (never copied: it has no source).
    let (sign, code, code_start) = if row.signed {
        let sign_len = slice.chars().next().map_or(0, char::len_utf8);
        let (sign, code) = slice.split_at(sign_len);
        (sign.to_owned(), code, row.source.start + sign_len)
    } else {
        (row.kind.sign().to_string(), slice, row.source.start)
    };
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
    let sign_source = row.signed.then_some(row.source.start);
    for ch in sign.chars() {
        cells.push((ch, row.kind.sign_role(), sign_source));
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
        cells.push((ch, code_roles[index], Some(code_start + offset)));
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

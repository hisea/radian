//! The transcript path: the block model's entries rendered into styled `ratatui` lines. The
//! marker column and the body/role styles, the entry layout ([`entry_lines`] and [`render_row`])
//! and the virtualized window the frame draws ([`visible_lines_with`], with
//! [`transcript_lines_with`] as the reference path it is tested against), plus the find
//! highlight and the mouse-selection styling.

use std::borrow::Cow;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{Entry, EntryKind};
use crate::blocks::{self, BodyLine};
use crate::theme::Theme;
use crate::tools::DisplayModes;

/// The visible marker for an entry kind (the transcript's `❯ `, `~ `, …). Also the cell
/// offset the selection mapping ([`crate::select`]) subtracts to get a body column.
///
/// This is the **default** (unicode) spelling. Both the user marker and every other marker are
/// two cells wide, so the width the mapping and the body layout subtract is the same under
/// every glyph tier; [`user_marker`] is what picks the `ascii` spelling of the user row.
pub(crate) fn marker_prefix(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::User => USER_MARKER,
        EntryKind::Assistant => "  ",
        // The expanded thinking channel's header marker (ADR 0035); the
        // folded one-liner overrides this with the fold marker in `render_row`. Two cells under
        // every tier, so the body column never moves.
        EntryKind::Reasoning => REASONING_MARKER,
        // ADR 0035's 2026-10-16 "折叠标记统一" note: a tool block's marker column carries the
        // head's fold marker (`• `/`◼ `/`◻ `, ascii `* + -`) on its head row and `│ ` on its
        // body rows (drawn by [`render_row`], not by the generic lead — both are two cells,
        // like every other marker). This is only the width's default spelling.
        EntryKind::ToolCall => "• ",
        EntryKind::ToolResult => "│ ",
        EntryKind::Notice => "* ",
        // The `/context` grid entry (spec `tui-context-grid`): the transcript's
        // notice marker, the same surface `/context` has always used.
        EntryKind::Context => "* ",
        // The compaction line has no marker of its own: it is a rule-like `── … ──` line.
        EntryKind::Compaction => "",
        // The startup block has no marker of its own (the `Tip:` label is content). The warning
        // marker is its own.
        EntryKind::Banner => "  ",
        EntryKind::Startup => "  ",
        // The trust hint has no marker of its own, like the startup lines it sits with.
        EntryKind::TrustHint => "  ",
        EntryKind::Warning => "! ",
        // Rule 2: two cells like every other marker; the `Tip:` label is content now.
        EntryKind::Tip => "  ",
        // ADR 0033: the command line; `!` (context) vs `!!` (not) differ in color, not marker.
        EntryKind::Bash { .. } => "$ ",
    }
}

/// The user row's two-cell marker (ADR 0035): `❯ ` in the `user` role.
/// The default (unicode) spelling, also what [`marker_prefix`] returns for the width.
const USER_MARKER: &str = "❯ ";

/// The expanded thinking channel's header marker (ADR 0035, superseded by
/// the 2026-10-16 "折叠标记统一" note: the thinking header and an expandable tool head share the
/// pair `◼` folded / `◻` expanded, ascii `+`/`-`). This is the **expanded** spelling, `◻ `
/// (ascii `- `); two cells under every tier, so the body column never moves.
const REASONING_MARKER: &str = "◻ ";

/// The thinking channel's header marker in the terminal's glyph set: `◻ ` normally, `- ` under
/// `ascii` ([`crate::footer::Glyphs::open_marker`]).
fn reasoning_marker(glyphs: crate::footer::Glyphs) -> &'static str {
    glyphs.open_marker()
}

/// The user row's marker in the terminal's glyph set (ADR 0035): `❯ `
/// normally, `> ` under `ascii`. Two cells either way, so the body column never moves.
fn user_marker(glyphs: crate::footer::Glyphs) -> &'static str {
    match glyphs {
        crate::footer::Glyphs::Ascii => "> ",
        _ => USER_MARKER,
    }
}

fn marker(kind: EntryKind, theme: &Theme, glyphs: crate::footer::Glyphs) -> (&'static str, Style) {
    let style = match kind {
        EntryKind::User => theme.user,
        EntryKind::Assistant => Style::default(),
        EntryKind::Reasoning => theme.reasoning,
        // The tool block's own markers (`│ ` and the head's fold marker) are `frame` (ADR 0035
        // ).
        EntryKind::ToolCall | EntryKind::ToolResult => theme.frame,
        EntryKind::Notice => theme.notice,
        // The `/context` grid entry (spec `tui-context-grid`): the transcript's
        // notice marker, the same surface `/context` has always used.
        EntryKind::Context => theme.notice,
        EntryKind::Banner => theme.hint,
        EntryKind::Startup => theme.hint,
        // The trust line under the wordmark (ADR 0031 T3): its own role.
        EntryKind::TrustHint => theme.trust_hint,
        EntryKind::Warning => theme.note,
        // The `Tip:` label steps down (dim), the body is muted.
        EntryKind::Tip => theme.truncation,
        // The compaction one-liner is structural (ADR 0035): the `frame`
        // role, the same dim level every other structural line uses.
        EntryKind::Compaction => theme.frame,
        // ADR 0033: `!` uses the command color; `!!` steps down to the hint level.
        EntryKind::Bash {
            exclude_from_context: false,
        } => theme.tool_call,
        EntryKind::Bash {
            exclude_from_context: true,
        } => theme.hint,
    };
    // The user row draws its own marker per glyph tier; the thinking channel's header does
    // too (the ascii `v`); every other entry's marker has one spelling. All are two cells, so
    // nothing else changes with the tier.
    let prefix = match kind {
        EntryKind::User => user_marker(glyphs),
        EntryKind::Reasoning => reasoning_marker(glyphs),
        _ => marker_prefix(kind),
    };
    (prefix, style)
}

fn body_style(entry: &Entry, theme: &Theme) -> Style {
    match entry.kind {
        EntryKind::Reasoning => theme.reasoning,
        EntryKind::ToolCall => theme.tool_call,
        EntryKind::ToolResult if entry.is_error => theme.tool_result_error,
        EntryKind::ToolResult => theme.tool_result,
        EntryKind::Notice => theme.notice,
        // The grid entry's unstyled spans are the main text level (the legend
        // names, the model, the headers); the numbers and hints carry their
        // own per-piece styles.
        EntryKind::Context => theme.text,
        EntryKind::Banner => theme.text,
        EntryKind::Startup => theme.notice_body,
        // The trust line under the wordmark (ADR 0031 T3): its own role.
        EntryKind::TrustHint => theme.trust_hint,
        EntryKind::Warning => theme.note,
        EntryKind::Tip => theme.hint,
        // The expanded summary is body text; its one-liner is styled by `marker`.
        EntryKind::Compaction => theme.notice_body,
        // ADR 0033: the same split as `marker`.
        EntryKind::Bash {
            exclude_from_context: false,
        } => theme.tool_call,
        EntryKind::Bash {
            exclude_from_context: true,
        } => theme.hint,
        // The thin user row (ADR 0035): `❯` **and** the user's words
        // carry the `user` role's foreground. `body_style` is separate from `marker` because a
        // wrapped continuation row has no marker to carry the style.
        EntryKind::User => theme.user,
        EntryKind::Assistant => {
            if entry.interrupted {
                Style::default().add_modifier(Modifier::ITALIC)
            } else {
                Style::default()
            }
        }
    }
}

/// The text a frame shows for `entry`. A user message's `@` attachments are folded to one
/// `[file: P, N lines]` line each; every other entry shows its text unchanged. Display only:
/// the message (and the journal) keep the full text. Also what a selection maps offsets into
/// ([`crate::select`]): the shown text is the copied text.
pub(crate) fn display_text(entry: &Entry) -> Cow<'_, str> {
    match entry.kind {
        EntryKind::User => crate::mention::fold_files(&entry.text),
        _ => Cow::Borrowed(&entry.text),
    }
}

/// The text a selection maps offsets into and copies. It is [`display_text`] plus, for a tool
/// result, the `[image: …]` placeholder lines the layout synthesizes above each run (ADR 0028
/// R10). They are not in the entry's own text, so without this a selection could neither
/// anchor on the placeholder row nor copy it. The spans are the layout's own
/// ([`crate::media::placeholder_spans`]), so the two cannot disagree.
pub(crate) fn selection_text(entry: &Entry) -> Cow<'_, str> {
    let base = display_text(entry);
    if entry.kind != EntryKind::ToolResult || entry.images.is_empty() {
        return base;
    }
    let mut out = base.into_owned();
    for image in &entry.images {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&image.placeholder);
    }
    Cow::Owned(out)
}

/// The display text lines of `entry`'s body (the prefix and the interrupted marker are not
/// included), at `width` — the block model's layout, flattened to strings. The renderer
/// renders exactly these, and [`crate::find`] matches against exactly these, so a folded `@`
/// attachment is shown and searched as its `[file: path, N lines]` line, never as the hidden
/// content. Padding rows (a user block's blank rows) come through as empty lines: they occupy
/// transcript lines and occupy space in the geometry, they just never match a query.
pub fn display_lines(
    entry: &Entry,
    modes: DisplayModes,
    width: usize,
    height: usize,
) -> Vec<String> {
    blocks::body_text_lines(entry, modes, width, height)
}

/// One entry's styled lines (ADR 0028 R1: the block layout, styled). An interrupted
/// assistant entry gets an explicit marker on its last line.
fn entry_lines(
    entry: &Entry,
    modes: DisplayModes,
    width: usize,
    height: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
    body_follows: bool,
) -> Vec<Line<'static>> {
    let (prefix, prefix_style) = marker(entry.kind, theme, modes.glyphs);
    let head_marker = blocks::head_marker(entry, modes, body_follows);
    let blocks = blocks::parse_blocks_for(entry, modes.reasoning);
    let body = blocks::layout_with(entry, &blocks, modes, width, height, spinner);
    let style = body_style(entry, theme);
    // The first and last text rows: the `first_text` flag is the structured-notice label's
    // (ADR 0028) and the interrupted marker sits on the last.
    // Every row carries text now (the tool block's frame rows are what carried none).
    let first_text = (!body.is_empty()).then_some(0);
    let last = body.len().checked_sub(1);
    let mut lines = Vec::with_capacity(body.len());
    for (i, row) in body.into_iter().enumerate() {
        let mut line = render_row(
            entry,
            &row,
            prefix,
            prefix_style,
            style,
            theme,
            Some(i) == first_text,
            modes.glyphs,
            spinner,
            head_marker,
        );
        if Some(i) == last && entry.interrupted {
            line.spans
                .push(Span::styled(" [interrupted]", theme.interrupted));
        }
        lines.push(line);
    }
    lines
}

/// Splits a structured line into its label and the rest (ADR 0028's notice decision): a
/// leading `[tag]` (up to 32 chars), or the short prefix before the first colon (up to 24
/// chars, no colon inside). Everything else — including any colon further in — is the body.
/// Nothing is special-cased per notice: one rule, one function.
pub fn split_label(text: &str) -> Option<(&str, &str)> {
    let start = text.len() - text.trim_start().len();
    let trimmed = &text[start..];
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(close) = rest.find(']')
        && close <= 32
    {
        let end = start + 1 + close + 1;
        return Some((&text[start..end], &text[end..]));
    }
    let colon = trimmed.find(':')?;
    let label = &trimmed[..colon];
    if label.is_empty() || label.chars().count() > 24 || label.contains('[') {
        return None;
    }
    let end = start + colon + 1;
    Some((&text[start..end], &text[end..]))
}

/// One laid-out [`BodyLine`] as styled spans. A tool block's head and body rows carry their own
/// markers ([`render_head_row`]/[`render_body_row`]); every other row takes the entry's marker
/// prefix on its first line; a code line has no lead at all (it is full-width by design,
/// ADR 0028 R9).
#[allow(clippy::too_many_arguments)]
fn render_row(
    entry: &Entry,
    row: &BodyLine,
    prefix: &str,
    prefix_style: Style,
    style: Style,
    theme: &Theme,
    first_text: bool,
    glyphs: crate::footer::Glyphs,
    spinner: crate::spinner::Frame,
    head_marker: blocks::HeadMarker,
) -> Line<'static> {
    match row.frame {
        // The tool block's head (ADR 0028): the fold marker and the
        // head's pieces.
        blocks::Frame::Head => {
            return render_head_row(row, style, theme, glyphs, spinner, head_marker);
        }
        // A `│ `-prefixed body row.
        blocks::Frame::Body => {
            return render_body_row(entry, row, style, theme, first_text, glyphs);
        }
        blocks::Frame::None => {}
    }
    // The collapsed reasoning placeholder is one synthetic line: its marker is the marker
    // column's own glyph (ADR 0028 rule 5), so the body column does not move
    // between folded and expanded. The marker is the tier's folded glyph (ADR 0035's 2026-10-16
    // "折叠标记统一" note: `◼ ` for unicode/nerd, `+ ` under ascii).
    let placeholder = entry.kind == EntryKind::Reasoning && row.hint;
    // An expanded thinking row (ADR 0028): the header carries the expand
    // marker, every other **prose** row the `│ ` rail in the marker column, so the body stays at
    // column 3 whether the channel is folded or expanded. A code row keeps its own frame (the
    // `│ ` rail `code_lines` already writes, or the `╭`/`╰` frame lines of ADR 0035
    // decision 7), exactly like an Assistant reply, so it takes no reasoning lead.
    let reasoning_body = entry.kind == EntryKind::Reasoning && !placeholder;
    let lead = if reasoning_body && !row.code {
        if first_text {
            prefix.to_string()
        } else {
            format!("{} ", glyphs.frame().vertical)
        }
    } else if row.code {
        String::new()
    } else if placeholder {
        glyphs.fold_marker().to_string()
    } else if first_text {
        prefix.to_string()
    } else {
        " ".repeat(prefix.width())
    };
    // A notice's content is the secondary level (the assignment: its value is 次,
    // only its label is 强调); the marker's own role stays `notice`.
    let body = if placeholder {
        theme.reasoning
    } else if matches!(entry.kind, EntryKind::Notice | EntryKind::Startup) {
        theme.notice_body
    } else {
        role_style(row.role, style, theme)
    };
    let mut spans = Vec::with_capacity(4);
    // The thinking body's rail is structural (the `frame` role); the header keeps the
    // channel's own `reasoning` marker style (ADR 0028).
    let lead_style = if reasoning_body && !first_text && !row.code {
        theme.frame
    } else {
        prefix_style
    };
    spans.push(Span::styled(lead, lead_style));
    spans.extend(body_spans(entry, row, body, theme, first_text));
    Line::from(spans)
}

/// The style of a body row's own role (ADR 0028 R4 + ADR 0029's diff roles): the warning
/// channel, the unified-diff line kinds and the structural `frame`.
fn role_style(role: crate::tools::LineRole, style: Style, theme: &Theme) -> Style {
    if role == crate::tools::LineRole::Normal {
        return style;
    }
    match role {
        crate::tools::LineRole::Warn => theme.note,
        crate::tools::LineRole::DiffAdded => theme.diff_added,
        crate::tools::LineRole::DiffRemoved => theme.diff_removed,
        crate::tools::LineRole::DiffContext => theme.diff_context,
        crate::tools::LineRole::DiffHunk => theme.diff_hunk,
        crate::tools::LineRole::Frame => theme.frame,
        // The `ask_user` block (ADR 0024 Q7): the prompt is the emphasis, a
        // description/note is weak, the box is structural.
        crate::tools::LineRole::Accent => theme.notice_label,
        crate::tools::LineRole::Muted => theme.notice_body,
        crate::tools::LineRole::Dim => theme.truncation,
        crate::tools::LineRole::Heading => theme
            .notice_label
            .add_modifier(ratatui::style::Modifier::BOLD),
        crate::tools::LineRole::Normal => style,
    }
}

/// A tool block's head row (ADR 0028): the fold marker (the `frame` role,
/// in the marker column, never copied) and the row's pieces — the tool name (`tool_name`), the
/// title (the entry's own text), the meta (weak) and the status glyph. The marker itself is the
/// head's [`blocks::HeadMarker`] (ADR 0035's 2026-10-16 "折叠标记统一" note, option A).
///
/// A **running** call's status is the live spinner frame in place of the placeholder the
/// layout carries (ADR 0027), drawn with the pulse's own colour.
#[allow(clippy::too_many_arguments)]
fn render_head_row(
    row: &BodyLine,
    style: Style,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
    spinner: crate::spinner::Frame,
    marker: blocks::HeadMarker,
) -> Line<'static> {
    let mut spans = vec![Span::styled(marker.glyph(glyphs).to_string(), theme.frame)];
    match &row.md {
        Some(pieces) => {
            for piece in pieces {
                match piece.style {
                    blocks::MdStyle::StatusRunning => spans.push(Span::styled(
                        spinner.glyph.to_string(),
                        spinner_style(theme, spinner),
                    )),
                    other => spans.push(Span::styled(
                        piece.text.clone(),
                        md_style(other, style, theme),
                    )),
                }
            }
        }
        None => spans.push(Span::styled(row.text.clone(), style)),
    }
    Line::from(spans)
}

/// A tool block's body row (ADR 0028): `│ ` in the marker column, the
/// row's content from the body column (the body's own role).
fn render_body_row(
    entry: &Entry,
    row: &BodyLine,
    style: Style,
    theme: &Theme,
    first_text: bool,
    glyphs: crate::footer::Glyphs,
) -> Line<'static> {
    let mut spans = vec![Span::styled(body_marker(glyphs).to_string(), theme.frame)];
    let body = role_style(row.role, style, theme);
    spans.extend(body_spans(entry, row, body, theme, first_text));
    Line::from(spans)
}

/// The tool block's body marker (ADR 0028): `│ `, `| ` under `ascii`.
fn body_marker(glyphs: crate::footer::Glyphs) -> &'static str {
    match glyphs {
        crate::footer::Glyphs::Ascii => "| ",
        _ => "│ ",
    }
}

/// The pulse's colour at a frame (ADR 0027): the theme's accent while it breathes,
/// the two ends when it cannot interpolate. The same rule [`crate::app::App::spinner_style`]
/// applies to the input's status row.
pub(crate) fn spinner_style(theme: &Theme, frame: crate::spinner::Frame) -> Style {
    let accent = theme.notice_label;
    let dim = theme.truncation;
    match crate::spinner::breath_color(accent.fg, dim.fg, frame.step) {
        Some(color) => accent.fg(color),
        None if frame.step == crate::spinner::PULSE_DIM_STEP => dim,
        None => accent,
    }
}

/// How many lines `entry` lays out to at `width`, from its cache when the entry has not
/// changed since it was last measured.
fn entry_line_count(entry: &mut Entry, modes: DisplayModes, width: usize, height: usize) -> usize {
    blocks::line_count_cached(entry, modes, width, height)
}

/// Lays the whole transcript out into styled lines. The frame does not use this (it lays out
/// only the visible entries, see [`visible_lines`]); it is the reference the virtualized path
/// is tested against.
pub fn transcript_lines(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    transcript_lines_with(
        entries,
        modes,
        width,
        height,
        theme,
        crate::spinner::Frame::default(),
    )
}

/// [`transcript_lines`] with the live spinner frame (ADR 0027).
pub fn transcript_lines_with(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    height: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        // The container owns the blank line between blocks (ADR 0028 rule 1).
        out.extend(std::iter::repeat_n(
            Line::from(""),
            blocks::gap_at(entries, index),
        ));
        // Whether a body is drawn under this tool head (the head's fold marker, ADR 0035's
        // 2026-10-16 note): the reference path measures the paired result without its cache.
        let result_lines = entries
            .get(index + 1)
            .filter(|next| next.kind == EntryKind::ToolResult)
            .map_or(0, |next| blocks::line_count(next, modes, width, height));
        let body_follows = tool_body_follows(entries, index, result_lines);
        out.extend(entry_lines(
            entry,
            modes,
            width,
            height,
            theme,
            spinner,
            body_follows,
        ));
    }
    out
}

/// Whether the tool call at `index` has a body drawn under its head (ADR 0035's 2026-10-16
/// "折叠标记统一" note, option A): the call is a merged block's head, its paired result is the
/// next entry, and that result lays out at least `result_lines` > 0. The head lives on the call
/// entry and the body on the result's, so only the pair can decide the head's fold marker.
fn tool_body_follows(entries: &[Entry], index: usize, result_lines: usize) -> bool {
    entries[index].kind == EntryKind::ToolCall
        && entries[index].card == blocks::Card::Head
        && entries
            .get(index + 1)
            .is_some_and(|next| next.kind == EntryKind::ToolResult)
        && result_lines > 0
}

/// One image the visible window draws (ADR 0028 R10): its entry, its index in the entry's
/// image list, and the window row its fixed run starts at. Only a **fully visible** run is
/// reported: the frame does not split an image across the top or bottom edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRun {
    pub entry: usize,
    pub image: usize,
    pub row: usize,
}

/// [`visible_lines`] plus the image runs the window can draw (ADR 0028 R10). `height` is the
/// transcript area's height (the rows the transcript can draw in), which decides how many
/// rows an image run reserves; `start` and `window` are the visible slice.
#[allow(clippy::too_many_arguments)]
pub fn visible_lines_images(
    entries: &mut [Entry],
    modes: DisplayModes,
    width: usize,
    height: usize,
    start: usize,
    window: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
) -> (usize, Vec<Line<'static>>, Vec<ImageRun>) {
    let __t = std::time::Instant::now();
    let counts: Vec<usize> = entries
        .iter_mut()
        .map(|entry| entry_line_count(entry, modes, width, height))
        .collect();
    if radian_session::trace::enabled() {
        eprintln!(
            "[trace] line-counts {} entries {} ms",
            counts.len(),
            __t.elapsed().as_millis()
        );
    }
    // The container's blank lines are part of the transcript (ADR 0028 rule 1):
    // the total and the window include them, or the scroll position and the mapping would
    // disagree with the drawing. The sum is computed without allocating a per-entry vector
    // (this is the 50k-entry frame path).
    let mut total: usize = counts.iter().sum::<usize>();
    for index in 1..entries.len() {
        total += blocks::gap_at(entries, index);
    }
    let mut out = Vec::with_capacity(window);
    let mut slots: Vec<(usize, usize, usize)> = Vec::new();
    let mut line = 0usize;
    for index in 0..entries.len() {
        if out.len() >= window {
            break;
        }
        let gap = blocks::gap_at(entries, index);
        let count = counts[index];
        // Whether the paired result draws a body (the head's fold marker, ADR 0035's 2026-10-16
        // note): the result's cached count is already in `counts`.
        let body_follows =
            tool_body_follows(entries, index, counts.get(index + 1).copied().unwrap_or(0));
        let entry = &mut entries[index];
        let next = line + gap + count;
        if next > start {
            // The gap lines first, then the entry's own lines.
            for _ in 0..gap {
                if out.len() >= window {
                    break;
                }
                if line >= start {
                    out.push(Line::from(""));
                }
                line += 1;
            }
            if out.len() >= window {
                break;
            }
            let skip = start.saturating_sub(line);
            let blocks = blocks::blocks_for(entry, modes.reasoning);
            let body = blocks::layout_with(entry, &blocks, modes, width, height, spinner);
            let (prefix, prefix_style) = marker(entry.kind, theme, modes.glyphs);
            let head_marker = blocks::head_marker(entry, modes, body_follows);
            let style = body_style(entry, theme);
            let first_text = (!body.is_empty()).then_some(0);
            let last = body.len().checked_sub(1);
            let rows: Vec<(usize, BodyLine)> = body
                .into_iter()
                .enumerate()
                .skip(skip)
                .take(window - out.len())
                .collect();
            let out_start = out.len();
            for (offset, (i, row)) in rows.into_iter().enumerate() {
                if let Some(image) = row.image {
                    slots.push((index, image, out_start + offset));
                }
                let mut line = render_row(
                    entry,
                    &row,
                    prefix,
                    prefix_style,
                    style,
                    theme,
                    Some(i) == first_text,
                    modes.glyphs,
                    spinner,
                    head_marker,
                );
                if Some(i) == last && entry.interrupted {
                    line.spans
                        .push(Span::styled(" [interrupted]", theme.interrupted));
                }
                out.push(line);
            }
        }
        line = next;
    }
    // Only a complete run is drawable (its computed height in rows): a run clipped by the top
    // or the bottom edge stays blank, the placeholder row still says what it was.
    let mut runs = Vec::new();
    let mut at = 0usize;
    while at < slots.len() {
        let (entry, image, row) = slots[at];
        let mut count = 1usize;
        while at + count < slots.len() && slots[at + count] == (entry, image, row + count) {
            count += 1;
        }
        if count == usize::from(crate::media::image_height_rows(height)) {
            runs.push(ImageRun { entry, image, row });
        }
        at += count;
    }
    (total, out, runs)
}

/// The transcript's total line count and the `window` lines starting at line `start`, laying
/// out only the entries that overlap that window (dogfooding D9). `height` is the transcript
/// area's height (ADR 0028 R10: it decides an image run's rows). Every other entry
/// contributes its cached line count, so a long session costs an integer sum per frame
/// instead of re-laying out every entry.
pub fn visible_lines(
    entries: &mut [Entry],
    modes: DisplayModes,
    width: usize,
    height: usize,
    start: usize,
    window: usize,
    theme: &Theme,
) -> (usize, Vec<Line<'static>>) {
    visible_lines_with(
        entries,
        modes,
        width,
        height,
        start,
        window,
        theme,
        crate::spinner::Frame::default(),
    )
}

/// [`visible_lines`] with the live spinner frame (ADR 0027): the frame lays out
/// only the visible entries, so the spinner is what makes it redraw every tick.
#[allow(clippy::too_many_arguments)]
pub fn visible_lines_with(
    entries: &mut [Entry],
    modes: DisplayModes,
    width: usize,
    height: usize,
    start: usize,
    window: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
) -> (usize, Vec<Line<'static>>) {
    let (total, lines, _) =
        visible_lines_images(entries, modes, width, height, start, window, theme, spinner);
    (total, lines)
}

/// Marks every occurrence of `query` (case-insensitive) in a rendered line with
/// `Modifier::REVERSED` (no new theme colours). The query is matched against the line's own
/// spans, which are the displayed text.
pub fn highlight(line: Line<'static>, query: &str) -> Line<'static> {
    if query.is_empty() {
        return line;
    }
    let needle = query.to_lowercase();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let style = span.style;
        let text = span.content.into_owned();
        // Case-insensitive without changing byte offsets: ASCII lowercases 1:1, so slicing the
        // original by the haystack's offsets is safe there. For non-ASCII, fall back to a
        // case-sensitive search on the original text (still byte-safe).
        let (haystack, fold) = if text.is_ascii() && query.is_ascii() {
            (text.to_lowercase(), true)
        } else {
            (text.clone(), false)
        };
        let needle = if fold {
            needle.clone()
        } else {
            query.to_owned()
        };
        let mut start = 0usize;
        while let Some(pos) = haystack[start..].find(&needle) {
            let at = start + pos;
            if at > start {
                spans.push(Span::styled(text[start..at].to_string(), style));
            }
            let end = at + needle.len();
            spans.push(Span::styled(
                text[at..end].to_string(),
                style.add_modifier(Modifier::REVERSED),
            ));
            start = end;
        }
        if start < text.len() {
            spans.push(Span::styled(text[start..].to_string(), style));
        }
    }
    Line::from(spans)
}

/// Styles the selected display-column `ranges` of one rendered line with the theme's
/// `selection` role (the text keeps its foreground; `mono` reverses). Columns are whole-line
/// coordinates — the `> ` prefix's cells included — and a wide char is selected when either
/// of its two cells is. A span is split only where the selection boundary falls inside it;
/// runs never cross spans, so each piece keeps its own base style.
pub(super) fn apply_selection(
    line: Line<'static>,
    ranges: &[(usize, usize)],
    selection: Style,
) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + ranges.len());
    let mut col = 0usize;
    // Each run carries its own base style, so a run's piece keeps the foreground of the span it
    // came from (a selected tail used to fall back to `Style::default()` on the final flush).
    let mut run: Option<(String, bool, Style)> = None;
    let flush = |out: &mut Vec<Span<'static>>, run: &mut Option<(String, bool, Style)>| {
        if let Some((text, run_selected, base)) = run.take() {
            let styled = if run_selected {
                base.patch(selection)
            } else {
                base
            };
            out.push(Span::styled(text, styled));
        }
    };
    for span in line.spans {
        let base = span.style;
        for ch in span.content.into_owned().chars() {
            let w = ch.width().unwrap_or(0);
            let selected = ranges
                .iter()
                .any(|&(start, end)| col < end && col + w.max(1) > start);
            match &mut run {
                Some((text, run_selected, run_base))
                    if *run_selected == selected && *run_base == base =>
                {
                    text.push(ch)
                }
                _ => {
                    flush(&mut out, &mut run);
                    run = Some((ch.to_string(), selected, base));
                }
            }
            col += w;
        }
    }
    flush(&mut out, &mut run);
    Line::from(out)
}

/// The body spans of one row: a structured notice's label split, a Markdown line's pieces, a
/// syntax-highlighted code line, a diagram, a weak reason, or the plain body. Shared by the
/// unframed path and a tool block's `│ ` body rows, so both render identically.
fn body_spans(
    entry: &Entry,
    row: &BodyLine,
    body_style: Style,
    theme: &Theme,
    first_text: bool,
) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(4);
    // A structured notice's **first** line (`profile: …`, `[memory] …`) steps the label up
    // with the accent role and keeps the body on pi's secondary level (ADR 0028,
    // decision). Only the first: a wrapped continuation that happens to contain a colon
    // (`https://…`, `12:30`, a `path:line`) is prose, not a label.
    if first_text
        && matches!(
            entry.kind,
            EntryKind::Notice | EntryKind::Startup | EntryKind::Context
        )
        && let Some((label, rest)) = split_label(&row.text)
    {
        out.push(Span::styled(label.to_owned(), theme.notice_label));
        out.push(Span::styled(rest.to_owned(), theme.notice_body));
    } else if let Some(md) = &row.md {
        // Assistant Markdown (ADR 0028 R2): each piece takes its own role's style. The
        // thinking channel (ADR 0028) is the one row kind that does not
        // use the mapping: every piece is the `reasoning` role, keeping only its weight
        // attributes. The mapping is left exactly as it is; the channel overrides its result.
        let reasoning = entry.kind == EntryKind::Reasoning;
        for piece in md {
            let style = if reasoning {
                reasoning_md_style(piece.style, body_style)
            } else {
                md_style(piece.style, body_style, theme)
            };
            out.push(Span::styled(piece.text.clone(), style));
        }
        // A code row carries its rail (a `Frame` piece) **and** its highlighted code, so the
        // two render in order (ADR 0035).
        if let Some(syntax) = &row.syntax {
            push_syntax(&mut out, syntax, body_style, theme);
        }
    } else if let Some(syntax) = &row.syntax {
        // A syntax-highlighted code line (ADR 0028 R2): each piece takes its Radian role's
        // style; `Plain` keeps the code block's own style. The text is unchanged, so
        // selection, copy and search are unaffected.
        push_syntax(&mut out, syntax, body_style, theme);
    } else if let Some(diagram) = &row.diagram {
        // A rendered diagram (ADR 0028 R8): each piece takes its own role's style.
        for (piece, role) in diagram {
            let style = match role {
                blocks::DiagramRole::Node => theme.mermaid_node,
                blocks::DiagramRole::Edge => theme.mermaid_edge,
                blocks::DiagramRole::Text => theme.mermaid_text,
            };
            out.push(Span::styled(piece.clone(), style));
        }
    } else if row.reason {
        // A weak one-line reason (`mermaid: …`, ADR 0028 R8): the dim/truncation role.
        out.push(Span::styled(row.text.clone(), theme.truncation));
    } else if entry.kind == EntryKind::Compaction && row.hint {
        // The folded compaction one-liner is structural (ADR 0035 /
        // ADR 0028): the `frame` role, from column 1. The expanded body
        // (`hint == false`) keeps the summary's own body style, unchanged.
        out.push(Span::styled(row.text.clone(), theme.frame));
    } else {
        out.push(Span::styled(row.text.clone(), body_style));
    }
    out
}

/// Pushes one code line's highlighted pieces (ADR 0028 R2): each takes its Radian role's
/// style, `Plain` keeps the code block's own style. Shared by a plain syntax row and a code
/// row that also carries its frame rail.
fn push_syntax(
    out: &mut Vec<Span<'static>>,
    syntax: &crate::highlight::HighlightedLine,
    body_style: Style,
    theme: &Theme,
) {
    for (text, role) in syntax {
        out.push(Span::styled(
            text.clone(),
            syntax_style(*role, body_style, theme),
        ));
    }
}

/// The style one per-piece role takes (ADR 0028 R2's Markdown roles, ADR 0029's thin roles):
/// the code/diff frames, the tool head's name and its status glyphs included. One definition
/// for the head and for every Markdown line.
fn md_style(style: blocks::MdStyle, body_style: Style, theme: &Theme) -> Style {
    match style {
        blocks::MdStyle::Heading => theme.md_heading,
        blocks::MdStyle::Heading1 => theme.md_heading.add_modifier(Modifier::UNDERLINED),
        // ADR 0035: level 3 and below are the text role plus bold, and
        // the `#` mark (`md_heading_mark`) is no longer drawn.
        blocks::MdStyle::HeadingMinor => theme.text.add_modifier(Modifier::BOLD),
        blocks::MdStyle::Strikethrough => theme.text.add_modifier(Modifier::CROSSED_OUT),
        blocks::MdStyle::TaskUnchecked | blocks::MdStyle::TaskChecked => theme.md_list_bullet,
        // ADR 0035: bold is the text role plus bold (it never takes
        // the heading role's colour); a code block's frame line, its rail and the `ask_user`
        // record's open frame are the structural `frame` role (ADR 0029).
        blocks::MdStyle::Bold => theme.text.add_modifier(Modifier::BOLD),
        blocks::MdStyle::Frame => theme.frame,
        blocks::MdStyle::Italic => theme.text.add_modifier(Modifier::ITALIC),
        blocks::MdStyle::Code => theme.md_code,
        blocks::MdStyle::Link => theme.md_link,
        blocks::MdStyle::LinkUrl => theme.md_link_url,
        blocks::MdStyle::Quote => theme.md_quote,
        blocks::MdStyle::QuoteBar => theme.md_quote_border,
        blocks::MdStyle::Bullet => theme.md_list_bullet,
        blocks::MdStyle::Rule => theme.md_hr,
        blocks::MdStyle::Accent => theme.notice_label,
        blocks::MdStyle::Muted => theme.notice_body,
        blocks::MdStyle::Dim => theme.truncation,
        blocks::MdStyle::Diff(role) => diff_style(role, body_style, theme),
        blocks::MdStyle::TableBorder => theme.md_hr,
        // The tool head's own pieces (ADR 0028).
        blocks::MdStyle::ToolName => theme.tool_name,
        blocks::MdStyle::StatusOk => theme.status_ok,
        blocks::MdStyle::StatusError => theme.status_error,
        // Handled by [`render_head_row`], which draws the live frame instead of a stored glyph:
        // a non-head path never meets one.
        blocks::MdStyle::StatusRunning => theme.status_ok,
        // The `/context` grid's category glyph (spec `tui-context-grid`): the tone's
        // own theme role. The body style never substitutes: a missing theme role
        // falls back to the built-in palette, never to the surrounding text.
        blocks::MdStyle::Ctx(tone) => match tone {
            blocks::CtxTone::System => theme.ctx_system,
            blocks::CtxTone::Files => theme.ctx_files,
            blocks::CtxTone::Memory => theme.ctx_memory,
            blocks::CtxTone::Tools => theme.ctx_tools,
            blocks::CtxTone::Skills => theme.ctx_skills,
            blocks::CtxTone::Mcp => theme.ctx_mcp,
            blocks::CtxTone::User => theme.ctx_user,
            blocks::CtxTone::Assistant => theme.ctx_assistant,
            blocks::CtxTone::Reasoning => theme.ctx_reasoning,
            blocks::CtxTone::Results => theme.ctx_results,
            blocks::CtxTone::Images => theme.ctx_images,
            blocks::CtxTone::Free => theme.ctx_free,
            blocks::CtxTone::Buffer => theme.ctx_buffer,
        },
        blocks::MdStyle::Plain => body_style,
    }
}

/// The style one **thinking-channel** Markdown piece takes (ADR 0028):
/// the single `reasoning` role plus only the piece's weight attributes. The Assistant mapping
/// in [`body_spans`] is deliberately untouched; this is the thinking channel's own variant.
/// Code and syntax highlighting are off for the channel, so a code piece reaches this as
/// `MdStyle::Code`/`Fence` and is coloured like the rest — no second colour.
fn reasoning_md_style(style: blocks::MdStyle, base: Style) -> Style {
    match style {
        blocks::MdStyle::Bold | blocks::MdStyle::Heading => base.add_modifier(Modifier::BOLD),
        blocks::MdStyle::Heading1 => base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        blocks::MdStyle::Italic => base.add_modifier(Modifier::ITALIC),
        blocks::MdStyle::Strikethrough => base.add_modifier(Modifier::CROSSED_OUT),
        // Every other piece — code, inline code, links, quote bars, bullets, rules, table
        // borders, fences, task boxes, dim/accent — is the one reasoning colour.
        _ => base,
    }
}

/// The style one highlighted piece takes (ADR 0028 R2): its Radian syntax role's colour, with
/// `Plain` keeping the row's own body style. Shared by a code block's lines and by a diff row's
/// code.
fn syntax_style(role: crate::highlight::SyntaxRole, body_style: Style, theme: &Theme) -> Style {
    match role {
        crate::highlight::SyntaxRole::Comment => theme.syntax_comment,
        crate::highlight::SyntaxRole::Keyword => theme.syntax_keyword,
        crate::highlight::SyntaxRole::Function => theme.syntax_function,
        crate::highlight::SyntaxRole::Variable => theme.syntax_variable,
        crate::highlight::SyntaxRole::String => theme.syntax_string,
        crate::highlight::SyntaxRole::Number => theme.syntax_number,
        crate::highlight::SyntaxRole::Type => theme.syntax_type,
        crate::highlight::SyntaxRole::Operator => theme.syntax_operator,
        crate::highlight::SyntaxRole::Punctuation => theme.syntax_punctuation,
        crate::highlight::SyntaxRole::Plain => body_style,
    }
}

/// The style one piece of a diff row takes (ADR 0028): the line number is
/// the weak `diff_line_number` role, the sign carries its own side's colour, a `+`/`-` row's
/// unhighlighted code is the file's own text colour (so the sign stays the coloured part), and a
/// highlighted piece goes through the code block's table.
fn diff_style(role: crate::tools::DiffRole, body_style: Style, theme: &Theme) -> Style {
    match role {
        crate::tools::DiffRole::Number => theme.diff_line_number,
        crate::tools::DiffRole::Added => theme.diff_added,
        crate::tools::DiffRole::Removed => theme.diff_removed,
        crate::tools::DiffRole::Context => theme.diff_context,
        crate::tools::DiffRole::Code => theme.text,
        crate::tools::DiffRole::Syntax(role) => syntax_style(role, body_style, theme),
    }
}

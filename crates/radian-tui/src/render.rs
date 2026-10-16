//! Rendering: the transcript view with its own scrolling, the input box, and the status
//! line. Pure given an `App`, so `TestBackend` can assert on the result.

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use radian_permissions::ApprovalRequest;

use crate::app::{
    AccessChoice, AccessWizard, App, Entry, EntryKind, Modal, TrustWizard, TurnState,
    budget_status_text, mcp_text, usage_text,
};
use crate::blocks::{self, BodyLine};
use crate::login::LoginWizard;
use crate::question::QuestionForm;
use crate::theme::Theme;
use crate::tools::DisplayModes;

/// One wrapped line as [`wrap_char_offsets`] sees it: the chars on the line (as byte offsets
/// into the wrapped text, each on a char boundary) and the **gap** just after the line.
///
/// `end` is where a click past the line's last cell lands: the offset just after the last
/// char, or, for an empty line, the offset of its terminating `\n` (or the text length for
/// the final paragraph). Because `wrap` collapses space runs, `end` is not necessarily where
/// the next line's first char sits; that in-between whitespace is exactly what a selection
/// across the break copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    pub chars: Vec<usize>,
    pub end: usize,
}

/// The same wrap [`wrap`] performs, but reporting *where each rendered char came from*: per
/// line, the byte offsets (into `text`) of the chars on it, in order, plus the line's end gap
/// (see [`WrappedLine`]). One algorithm, two views: `wrap` is derived from this, so a cell →
/// offset mapping through [`crate::select`] can never drift from what is rendered.
pub fn wrap_char_offsets(text: &str, width: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    // The byte offsets of the chars on the line being built, the line's display width, and
    // the end gap so far (just after the last pushed char). The gap starts at 0 and is
    // corrected to the `\n`'s offset for an empty line, and to `text.len()` for an empty
    // final paragraph, when the line is finished.
    let mut chars: Vec<usize> = Vec::new();
    let mut line_width = 0usize;
    let mut end = 0usize;
    // One buffered space (its byte offset), flushed only when a following char stays on the
    // line — so space runs collapse and trail away (the `wrap` semantics, unchanged).
    let mut pending_space: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            let empty = chars.is_empty();
            out.push(WrappedLine {
                chars: std::mem::take(&mut chars),
                end: if empty { idx } else { end },
            });
            line_width = 0;
            end = idx + 1;
            pending_space = None;
            continue;
        }
        if ch == ' ' {
            // A space only joins a line that already has a char on it; leading spaces and
            // repeats collapse away.
            if !chars.is_empty() {
                pending_space = Some(idx);
            }
            continue;
        }
        // A tab occupies four columns (ADR 0028 R9's rule, one rule for every text the
        // transcript wraps); it is not a space, so it never collapses.
        let ch_width = if ch == '\t' {
            4
        } else {
            ch.width().unwrap_or(0)
        };
        let extra = usize::from(pending_space.is_some());
        if line_width + extra + ch_width > width && line_width > 0 {
            out.push(WrappedLine {
                chars: std::mem::take(&mut chars),
                end,
            });
            line_width = 0;
            pending_space = None;
        }
        if let Some(space) = pending_space.take() {
            chars.push(space);
            line_width += 1;
        }
        chars.push(idx);
        line_width += ch_width;
        end = idx + ch.len_utf8();
    }
    out.push(WrappedLine {
        chars,
        end: if end == 0 { text.len() } else { end },
    });
    out
}

/// Hard-wraps `text` to `width` display columns, breaking on spaces where possible and on
/// character boundaries otherwise (CJK has no spaces). Newlines start a new line.
///
/// This is the renderer's hot path (every visible entry, every frame), so it stays the
/// plain string builder. [`wrap_char_offsets`] is the same algorithm reporting *where each
/// rendered char came from*; [`crate::select`] maps cells through it, and the two are held
/// together by the `wrap_and_wrap_char_offsets_agree` test — change either, change both.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0usize;
        let mut pending_space = false;
        for ch in paragraph.chars() {
            if ch == ' ' {
                pending_space = !line.is_empty();
                continue;
            }
            let ch_width = if ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            let extra = usize::from(pending_space);
            if line_width + extra + ch_width > width && line_width > 0 {
                out.push(std::mem::take(&mut line));
                line_width = 0;
                pending_space = false;
            }
            if pending_space {
                line.push(' ');
                line_width += 1;
                pending_space = false;
            }
            line.push(ch);
            line_width += ch_width;
        }
        out.push(line);
    }
    out
}

/// How many lines [`wrap`] would produce, without building any of them. The virtualization
/// budget (ADR 0028 R7) measures every entry on the first frame and after a resize; this is
/// that path, so it must not allocate.
pub fn wrap_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut lines = 0usize;
    for paragraph in text.split('\n') {
        let mut line_width = 0usize;
        let mut pending_space = false;
        let mut any = false;
        for ch in paragraph.chars() {
            if ch == ' ' {
                // A space only joins a line that already has content (`wrap`'s
                // `!line.is_empty()`): leading spaces are dropped.
                pending_space = any;
                continue;
            }
            let ch_width = if ch == '\t' {
                4
            } else if ch.is_ascii() {
                // `UnicodeWidthChar::width` is a table lookup; ASCII is by far the common case
                // (the first frame counts ~40 MiB of tool output). Printable ASCII is 1, the
                // other ASCII (control) is 0, exactly as `width` reports.
                usize::from((0x20..0x7f).contains(&(ch as u8)))
            } else {
                ch.width().unwrap_or(0)
            };
            let extra = usize::from(pending_space);
            if line_width + extra + ch_width > width && line_width > 0 {
                lines += 1;
                line_width = 0;
                pending_space = false;
            }
            if pending_space {
                line_width += 1;
                pending_space = false;
            }
            line_width += ch_width;
            any = true;
        }
        lines += 1;
    }
    lines
}

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
        EntryKind::Reasoning => "~ ",
        // ADR 0028: a tool block's marker column carries `• ` on its
        // head row and `│ ` on its body rows (drawn by [`render_row`], not by the generic
        // lead — both are two cells, like every other marker).
        EntryKind::ToolCall => "• ",
        EntryKind::ToolResult => "│ ",
        EntryKind::Notice => "* ",
        // The compaction line has no marker of its own: it is a rule-like `── … ──` line.
        EntryKind::Compaction => "",
        // The startup block has no marker of its own: its lines carry `▶`/`▼` (or none for
        // the wordmark). The warning marker is its own; `Tip:` is styled separately.
        EntryKind::Banner => "  ",
        EntryKind::Startup => "  ",
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
        // The tool block's own markers (`• `/`│ `) are `frame` (ADR 0035).
        EntryKind::ToolCall | EntryKind::ToolResult => theme.frame,
        EntryKind::Notice => theme.notice,
        EntryKind::Banner => theme.hint,
        EntryKind::Startup => theme.hint,
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
    // The user row draws its own marker per glyph tier; every other entry's marker has one
    // spelling. Both are two cells, so nothing else changes with the tier.
    let prefix = match kind {
        EntryKind::User => user_marker(glyphs),
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
        EntryKind::Banner => theme.text,
        EntryKind::Startup => theme.notice_body,
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

/// The style a band's half-cell pad cell takes (ADR 0035): the band's own color as
/// the foreground and no background of its own — the band's background is painted on the block's
/// text rows, which this row sits half a cell against. Under `mono` both are absent, so the half
/// cell is the terminal default — still a visible block, just uncolored.
fn band_pad_style(band: Style) -> Style {
    let mut style = Style::default().add_modifier(band.add_modifier);
    if let Some(color) = band.bg.or(band.fg) {
        style = style.fg(color);
    }
    style
}

/// The `bar` input's half-cell padding row (ADR 0035): the rail's own cell first —
/// `rail` is `▖` (quadrant lower left) on the row above the band, `▘` (quadrant upper left) on
/// the row below it, both in the accent — then the band's half cell (`glyph`) across the rest.
/// The quadrant is what lets the rail stop at the half cell: the cell keeps the terminal's own
/// background (its right half carries no band on a pad row, which is the cost of the shape), so
/// the rail reads as two cells tall with a straight right edge next to the `▌` text rows.
fn bar_pad_line(
    rail: &str,
    rail_style: Style,
    glyph: &str,
    width: usize,
    band: Style,
) -> Line<'static> {
    let mut spans = vec![Span::styled(rail.to_string(), rail_style)];
    if width > 1 {
        spans.push(Span::styled(glyph.repeat(width - 1), band_pad_style(band)));
    }
    Line::from(spans)
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

/// The display text lines of `entry`'s body (the prefix and the interrupted marker are not
/// included), at `width` — the block model's layout, flattened to strings. The renderer
/// renders exactly these, and [`crate::find`] matches against exactly these, so a folded `@`
/// attachment is shown and searched as its `[file: path, N lines]` line, never as the hidden
/// content. Padding rows (a user block's blank rows) come through as empty lines: they occupy
/// transcript lines and occupy space in the geometry, they just never match a query.
pub fn display_lines(entry: &Entry, modes: DisplayModes, width: usize) -> Vec<String> {
    blocks::body_text_lines(entry, modes, width)
}

/// One entry's styled lines (ADR 0028 R1: the block layout, styled). An interrupted
/// assistant entry gets an explicit marker on its last line.
fn entry_lines(
    entry: &Entry,
    modes: DisplayModes,
    width: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
) -> Vec<Line<'static>> {
    let (prefix, prefix_style) = marker(entry.kind, theme, modes.glyphs);
    let blocks = blocks::parse_blocks_for(entry, modes.reasoning);
    let body = blocks::layout_with(entry, &blocks, modes, width, spinner);
    let style = body_style(entry, theme);
    // The first and last text rows: the `first_text` flag is the structured-notice label's
    // (ADR 0028) and the interrupted marker sits on the last.
    let first_text = body.iter().position(|line| !line.pad.is_pad());
    let last = body.iter().rposition(|line| !line.pad.is_pad());
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
) -> Line<'static> {
    if row.pad.is_pad() {
        // A padding row carries no text (ADR 0035): the transcript's blocks have
        // none since ADR 0028 (the `bar` input draws its own).
        return Line::from(vec![Span::raw(String::new())]);
    }
    match row.frame {
        // The tool block's head (ADR 0028): `• ` and the head's pieces.
        blocks::Frame::Head => return render_head_row(row, style, theme, glyphs, spinner),
        // A `│ `-prefixed body row.
        blocks::Frame::Body => {
            return render_body_row(entry, row, style, theme, first_text, glyphs);
        }
        blocks::Frame::None => {}
    }
    // The collapsed reasoning placeholder is one synthetic line: its marker is the marker
    // column's own glyph (ADR 0028 rule 5), so the body column does not move
    // between folded and expanded. The marker is the tier's fold glyph (ADR 0027
    // item 2: `›` for unicode, `>` for nerd/ascii; `▸` is not in all six checked fonts).
    let placeholder = entry.kind == EntryKind::Reasoning && row.hint;
    let lead = if row.code {
        String::new()
    } else if placeholder {
        format!("{} ", glyphs.fold_marker())
    } else if first_text {
        prefix.to_string()
    } else {
        " ".repeat(prefix.width())
    };
    // A notice's content is the secondary level (the owner's assignment: its value is 次,
    // only its label is 强调); the marker's own role stays `notice`.
    let body = if placeholder {
        theme.reasoning
    } else if matches!(entry.kind, EntryKind::Notice | EntryKind::Startup) {
        theme.notice_body
    } else {
        role_style(row.role, style, theme)
    };
    let mut spans = Vec::with_capacity(4);
    spans.push(Span::styled(lead, prefix_style));
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
        // The `ask_user` block (ADR 0024 R1): the prompt is the emphasis, a
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

/// A tool block's head row (ADR 0028): the `• ` marker (the `frame`
/// role, in the marker column, never copied) and the row's pieces — the tool name
/// (`tool_name`), the title (the entry's own text), the meta (weak) and the status glyph.
///
/// A **running** call's status is the live spinner frame in place of the placeholder the
/// layout carries (ADR 0027/13), drawn with the pulse's own colour.
#[allow(clippy::too_many_arguments)]
fn render_head_row(
    row: &BodyLine,
    style: Style,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
    spinner: crate::spinner::Frame,
) -> Line<'static> {
    let mut spans = vec![Span::styled(tool_marker(glyphs).to_string(), theme.frame)];
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

/// The tool block's head marker (ADR 0035): `• `, `- ` under `ascii`.
/// Two cells, like every marker.
pub(crate) fn tool_marker(glyphs: crate::footer::Glyphs) -> &'static str {
    match glyphs {
        crate::footer::Glyphs::Ascii => "- ",
        _ => "• ",
    }
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
fn entry_line_count(entry: &mut Entry, modes: DisplayModes, width: usize) -> usize {
    blocks::line_count_cached(entry, modes, width)
}

/// Lays the whole transcript out into styled lines. The frame does not use this (it lays out
/// only the visible entries, see [`visible_lines`]); it is the reference the virtualized path
/// is tested against.
pub fn transcript_lines(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    transcript_lines_with(
        entries,
        modes,
        width,
        theme,
        crate::spinner::Frame::default(),
    )
}

/// [`transcript_lines`] with the live spinner frame (ADR 0027).
pub fn transcript_lines_with(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        // The container owns the blank line between blocks (ADR 0028 rule 1).
        out.extend(std::iter::repeat_n(
            Line::from(""),
            blocks::block_gap(entries.get(index.wrapping_sub(1)), entry),
        ));
        out.extend(entry_lines(entry, modes, width, theme, spinner));
    }
    out
}

/// The transcript's total line count and the `height` lines starting at line `start`, laying
/// out only the entries that overlap that window (dogfooding D9). Every other entry
/// contributes its cached line count, so a long session costs an integer sum per frame
/// instead of re-laying out every entry.
pub fn visible_lines(
    entries: &mut [Entry],
    modes: DisplayModes,
    width: usize,
    start: usize,
    height: usize,
    theme: &Theme,
) -> (usize, Vec<Line<'static>>) {
    visible_lines_with(
        entries,
        modes,
        width,
        start,
        height,
        theme,
        crate::spinner::Frame::default(),
    )
}

/// [`visible_lines`] with the live spinner frame (ADR 0027): the frame lays out
/// only the visible entries, so the spinner is what makes it redraw every tick.
pub fn visible_lines_with(
    entries: &mut [Entry],
    modes: DisplayModes,
    width: usize,
    start: usize,
    height: usize,
    theme: &Theme,
    spinner: crate::spinner::Frame,
) -> (usize, Vec<Line<'static>>) {
    let __t = std::time::Instant::now();
    let counts: Vec<usize> = entries
        .iter_mut()
        .map(|entry| entry_line_count(entry, modes, width))
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
        total += blocks::block_gap(entries.get(index - 1), &entries[index]);
    }
    let mut out = Vec::with_capacity(height);
    let mut line = 0usize;
    for index in 0..entries.len() {
        if out.len() >= height {
            break;
        }
        let gap = blocks::block_gap(entries.get(index.wrapping_sub(1)), &entries[index]);
        let count = counts[index];
        let entry = &mut entries[index];
        let next = line + gap + count;
        if next > start {
            // The gap lines first, then the entry's own lines.
            for _ in 0..gap {
                if out.len() >= height {
                    break;
                }
                if line >= start {
                    out.push(Line::from(""));
                }
                line += 1;
            }
            if out.len() >= height {
                break;
            }
            let skip = start.saturating_sub(line);
            let blocks = blocks::blocks_for(entry, modes.reasoning);
            let body = blocks::layout_with(entry, &blocks, modes, width, spinner);
            let (prefix, prefix_style) = marker(entry.kind, theme, modes.glyphs);
            let style = body_style(entry, theme);
            let first_text = body.iter().position(|line| !line.pad.is_pad());
            let last = body.iter().rposition(|line| !line.pad.is_pad());
            out.extend(
                body.into_iter()
                    .enumerate()
                    .skip(skip)
                    .take(height - out.len())
                    .map(|(i, row)| {
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
                        );
                        if Some(i) == last && entry.interrupted {
                            line.spans
                                .push(Span::styled(" [interrupted]", theme.interrupted));
                        }
                        line
                    }),
            );
        }
        line = next;
    }
    (total, out)
}

/// The tallest the input area grows before it scrolls internally instead.
const MAX_INPUT_ROWS: usize = 10;

/// The rows the find box needs: its top border, the query, its bottom border (`Borders::ALL`).
const FIND_BOX_HEIGHT: u16 = 3;

/// How the input area is drawn (config `tui.input_style`, owner UI review 2026-10-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputStyle {
    /// The banded input (ADR 0035 V3, omp's `rail`): one background band per row with a left
    /// accent rail, no border and no rules, and a fixed status row above the band. Still
    /// selectable (`tui.input_style = "bar"`, ADR 0035).
    Bar,
    /// A rule above and below, `❯ ` on the first line, continuation lines indented two cells
    /// (ADR 0035: the default).
    #[default]
    Lines,
    /// The bordered box with a ` message ` title (the pre-review shape).
    Box,
}

impl InputStyle {
    /// The config spelling. `None` for anything else, so the caller falls back to the default.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "lines" => Some(Self::Lines),
            "box" => Some(Self::Box),
            "bar" => Some(Self::Bar),
            _ => None,
        }
    }
}

/// The input prompt, in the style of pi: the first line starts with `❯ ` and the continuation
/// lines indent by the same two cells, so the text stays aligned. The owner's 2026-10-14
/// decision: **both** input styles use `❯`. Its display width (2 cells) is what the layout
/// reserves — never its byte length (`❯` is three bytes, which used to shift the box cursor).
const INPUT_PROMPT: &str = "❯ ";

/// The bordered style's prompt: the owner's `❯`, colored with the theme's
/// accent role (`theme.model`, pi's `accent`). Two cells, like `> `.
const BOX_PROMPT: &str = "❯ ";

/// The placeholder shown while the input is empty.
const INPUT_HINT: &str = "(type a message, Enter to send, Ctrl-J for a new line)";

/// The `/login` wizard's panel (ADR 0035): the shared [`crate::panel::Panel`] with
/// the step's rows, the provider/protocol choices through [`crate::select_list`], and the
/// step's key hints in the bottom border. Pure, so the layout is testable without a terminal.
/// **The key is never among the rows** (D3): a filled field renders one `*` per character.
pub fn login_panel(
    wizard: &LoginWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    use crate::login::LoginStep;
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let label = |text: &str| Line::from(Span::styled(text.to_owned(), theme.text));
    let value = |text: String| Line::from(Span::styled(text, theme.text));
    let mut panel = crate::panel::Panel::new("/login");
    match &wizard.step {
        LoginStep::ChooseProvider { cursor } => {
            panel = panel.row(label("choose a provider"));
            let items: Vec<crate::select_list::Item> = LoginWizard::choices()
                .into_iter()
                .map(|(id, description)| crate::select_list::Item::new(id).description(description))
                .collect();
            let list =
                crate::select_list::view(theme, &items, "", *cursor, items.len(), inner, ascii);
            for line in list.lines {
                panel = panel.row(line);
            }
            panel = panel
                .hint("↑/↓", "choose")
                .hint("⏎", "next")
                .hint("esc", "cancel");
        }
        LoginStep::CustomName => {
            panel = panel
                .row(label("provider name (also its auth.json entry):"))
                .row(value(wizard.name.clone()));
            panel = panel.hint("⏎", "next").hint("esc", "cancel");
        }
        LoginStep::CustomProtocol { cursor } => {
            panel = panel.row(label("wire protocol"));
            let items: Vec<crate::select_list::Item> = crate::login::PROTOCOLS
                .iter()
                .map(|kind| crate::select_list::Item::new(*kind))
                .collect();
            let list =
                crate::select_list::view(theme, &items, "", *cursor, items.len(), inner, ascii);
            for line in list.lines {
                panel = panel.row(line);
            }
            panel = panel
                .hint("↑/↓", "choose")
                .hint("⏎", "next")
                .hint("esc", "cancel");
        }
        LoginStep::CustomBaseUrl => {
            panel = panel
                .row(label("base URL:"))
                .row(value(wizard.base_url.clone()));
            panel = panel.hint("⏎", "next").hint("esc", "cancel");
        }
        LoginStep::KeyInput => {
            panel = panel
                .row(label("API key (hidden; paste works too):"))
                .row(value(LoginWizard::masked(&wizard.key)));
            panel = panel.hint("⏎", "next").hint("esc", "cancel");
        }
        LoginStep::Verifying => {
            panel = panel.row(label(&format!("verifying {} …", wizard.provider_name())));
            panel = panel.hint("esc", "cancel");
        }
        LoginStep::VerifyFailed { reason } => {
            panel = panel
                .row(Line::from(Span::styled(
                    format!("verification failed: {reason}"),
                    theme.turn_cancelled,
                )))
                .row(label("save anyway?"));
            panel = panel.hint("y", "save").hint("n/esc", "cancel");
        }
    }
    if let Some(error) = &wizard.error {
        panel = panel.row(Line::from(Span::styled(
            error.clone(),
            theme.turn_cancelled,
        )));
    }
    panel.lines(width, theme, glyphs)
}

/// The `/login` panel's height, measured from the panel itself so the frame and the layout
/// agree.
fn login_height(
    wizard: &LoginWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    login_panel(wizard, width, theme, glyphs).len() as u16
}

/// The startup project-trust panel (ADR 0031 T3, ADR 0035): the shared panel with
/// the workspace line, the choices through [`crate::select_list`], and the hardcoded key
/// hints. The cursor's default is the fail-safe `UntrustHere`.
pub fn trust_panel(
    wizard: &TrustWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let items: Vec<crate::select_list::Item> = wizard
        .choices
        .iter()
        .map(|choice| crate::select_list::Item::new(wizard.label(*choice)))
        .collect();
    let list =
        crate::select_list::view(theme, &items, "", wizard.cursor, items.len(), inner, ascii);
    let mut panel = crate::panel::Panel::new("project trust").row(Line::from(Span::styled(
        format!("project resources found in {}", wizard.workspace.display()),
        theme.text,
    )));
    for line in list.lines {
        panel = panel.row(line);
    }
    panel = panel
        .hint("↑/↓", "choose")
        .hint("⏎", "select")
        .hint("esc", "do not trust");
    panel.lines(width, theme, glyphs)
}

/// The trust panel's height, measured from the panel itself.
fn trust_height(
    wizard: &TrustWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    trust_panel(wizard, width, theme, glyphs).len() as u16
}

/// The `/access full` confirmation (ADR 0038 slice 2, ADR 0035): the shared panel
/// with a warning line, the three choices through [`crate::select_list`], and the hardcoded
/// key hints. The default cursor is `cancel`, so Enter does not open the sandbox.
pub fn access_panel(
    wizard: &AccessWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let items: Vec<crate::select_list::Item> = AccessChoice::ALL
        .iter()
        .map(|choice| crate::select_list::Item::new(choice.label()))
        .collect();
    let list =
        crate::select_list::view(theme, &items, "", wizard.cursor, items.len(), inner, ascii);
    let mut panel = crate::panel::Panel::new("/access full").row(Line::from(Span::styled(
        "full means no sandbox: commands run with your full access",
        theme.text,
    )));
    for line in list.lines {
        panel = panel.row(line);
    }
    panel = panel
        .hint("↑/↓", "choose")
        .hint("⏎", "select")
        .hint("esc", "cancel");
    panel.lines(width, theme, glyphs)
}

/// The `/access full` panel's height, measured from the panel itself.
fn access_height(
    wizard: &AccessWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    access_panel(wizard, width, theme, glyphs).len() as u16
}

/// The `/logout` confirmation (ADR 0035): the shared panel with the provider and
/// the hardcoded `y`/`n` hints in the bottom border.
pub fn logout_panel(
    provider: &str,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    crate::panel::Panel::new("/logout")
        .row(Line::from(Span::styled(
            format!("remove the stored credential for `{provider}`?"),
            theme.text,
        )))
        .hint("[y]", "remove")
        .hint("[n/esc]", "keep")
        .lines(width, theme, glyphs)
}

/// The `bar` style's left rail (ADR 0035 V3, revision 4): the accent vertical bar on every input
/// row (omp's `ACCENT_RAIL`). Revision 4 made it `▌` (the left **half** block): the rail is half
/// a cell wide, so it can be continued into the band's half-cell pad rows as half-cell-wide
/// quadrants ([`bar_pad_line`]) and read as two cells tall. The `ascii` glyph set uses `|`
/// (ADR 0027's rule: the unicode glyphs are only used when the font set has them).
const BAR_RAIL: &str = "▌";
/// The `bar` rail in the `ascii` glyph set.
const BAR_RAIL_ASCII: &str = "|";
/// The cells before the `bar` style's text: the rail plus one cell of padding (omp's rail
/// `defaultPaddingX = 1`), which is also where the status row's spinner starts (ADR 0035
/// revision 4: the dot sits in the rail's column, the state text at this text column).
const BAR_LEAD: usize = 2;

/// The input area's height: the text rows plus the style's chrome — two rule/border rows for
/// `lines`/`box`, the one fixed status row for `bar` (ADR 0035 V3: `bar` has no rules).
fn input_height(app: &App, width: usize) -> u16 {
    let inner = input_inner_width(app, width);
    let rows = crate::composer::rows(&app.input.text(), inner)
        .len()
        .clamp(1, MAX_INPUT_ROWS);
    match app.input_style {
        InputStyle::Lines | InputStyle::Box => rows as u16 + 2,
        InputStyle::Bar => rows as u16 + 1 + bar_pad_rows(app) as u16,
    }
}

/// The `bar` input's half-cell padding rows (ADR 0035): two (`▄` above the band,
/// `▀` below it) under the `unicode`/`nerd` glyph tiers, none under `ascii` (decision 4). The
/// two rows are always both present or both absent, so the band never changes height mid-frame.
fn bar_pad_rows(app: &App) -> usize {
    if crate::blocks::banded_pad(app.footer_glyphs()) {
        2
    } else {
        0
    }
}

/// The input's visible text rows (`input_area.height` minus the same chrome `input_height`
/// reserves for the style), at least one.
fn input_visible_rows(app: &App, height: u16) -> usize {
    let chrome: u16 = match app.input_style {
        InputStyle::Lines | InputStyle::Box => 2,
        InputStyle::Bar => 1 + bar_pad_rows(app) as u16,
    };
    height.saturating_sub(chrome).max(1) as usize
}

/// The display width of a prompt constant: the columns it occupies, **not** its byte length
/// (`❯` is three bytes and one cell). Every layout use of a prompt goes through this.
fn prompt_width(prompt: &str) -> usize {
    UnicodeWidthStr::width(prompt)
}

/// The text width inside the input area: `❯ ` on the first line for `lines`, a border plus the
/// `❯ ` prompt for `box`, the rail plus its one cell of padding for `bar`.
fn input_inner_width(app: &App, width: usize) -> usize {
    let reserved = match app.input_style {
        InputStyle::Lines => prompt_width(INPUT_PROMPT),
        // Two border cells plus the prompt.
        InputStyle::Box => 2 + prompt_width(BOX_PROMPT),
        InputStyle::Bar => BAR_LEAD,
    };
    width.saturating_sub(reserved).max(1)
}

/// The `bar` style's rail glyph for the terminal's glyph set.
fn bar_rail(app: &App) -> &'static str {
    match app.footer_glyphs() {
        crate::footer::Glyphs::Ascii => BAR_RAIL_ASCII,
        _ => BAR_RAIL,
    }
}

/// The status row a `bar` input carries **above** its band (ADR 0035 V3 / decision 1):
/// `<spinner> Working · 12s · esc to interrupt` while a turn runs, and a blank row — which
/// still occupies its line — when idle, so the band and the transcript never move when a turn
/// starts or ends.
///
/// The spinner and the state word are the ones the `lines`/`box` rule line shows (ADR 0027
/// revision 2, `App::spinner_status`). Since ADR 0035 the dot sits in the **rail's**
/// column (0) rather than at the text column, so it is on the same vertical line as the rail;
/// the state text starts at the text column (`BAR_LEAD`) with one cell between the two.
/// The spinner takes the accent and the rest is one level weaker. The clock is the footer's
/// `elapsed` (`TurnTimer`: the running generation, else the last one), written in whole seconds
/// as the ADR's `12s`; `esc to interrupt` is shown exactly while a turn runs, because that is
/// when Esc cancels it.
pub fn bar_status_line(app: &App) -> Line<'static> {
    let Some(status) = app.spinner_status() else {
        return Line::from("");
    };
    let spin = app.spinner_frame();
    let mut spans = vec![
        Span::styled(spin.glyph, app.spinner_style(spin)),
        Span::styled(" ".repeat(BAR_LEAD - 1), app.theme.hint),
        Span::styled(status.to_string(), app.theme.hint),
    ];
    if let Some(elapsed) = app.session.timer.elapsed(app.now) {
        spans.push(Span::styled(
            format!(" · {}", crate::timing::format_seconds(elapsed)),
            app.theme.hint,
        ));
    }
    if app.turn_running() {
        spans.push(Span::styled(" · esc to interrupt", app.theme.hint));
    }
    Line::from(spans)
}

/// The approval prompt's key line. It is drawn on its **own** row under the body, never part
/// of the wrapped text, so a long command can never push the keys out of the box (owner
/// acceptance, 2026-10-13). The auto key is a constant like the line itself: if the owner
/// changes the key, this string and [`crate::app::Input::ApprovalAuto`]'s `KeyCode` are the
/// only two places (ADR 0026 D9, correction 2026-10-13: the key is `!`; `Y` stays
/// `y` = allow once).
pub const APPROVAL_KEYS: &str =
    "[y] allow once   [a] always this session   [n] deny   [!] auto for this session";

/// The approval prompt's body: everything above the key line. The **resolved** resource is what
/// is shown: `a/../secret` must not read as if it stayed under `a/` (the permission layer
/// resolves it before the prompt, and that resolved form is also the session-grant key).
///
/// The resource's own argument is **not** repeated in `args`: the command or path already has
/// its line, so the rest (`cwd`, `timeout`, …) is what `args` shows, and it is omitted entirely
/// when nothing is left. The user must be able to read the whole command, so it is never
/// truncated; the 400-byte cap applies to the remaining `args` only.
pub fn approval_body(request: &ApprovalRequest) -> String {
    let label = if request.arguments.get("path").is_some() {
        "path"
    } else if request.arguments.get("command").is_some() {
        "command"
    } else {
        "resource"
    };
    let mut text = String::new();
    text.push_str(&format!("tool: {}\n", request.tool));
    text.push_str(&format!("{label}: {}\n", request.resolved_resource));
    let rest: serde_json::Map<String, serde_json::Value> = request
        .arguments
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| key.as_str() != label)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !rest.is_empty() {
        let args = serde_json::to_string_pretty(&rest)
            .unwrap_or_else(|_| serde_json::Value::Object(rest.clone()).to_string());
        let (shown, discarded) = truncate_for_display(&args, 400);
        text.push_str(&format!("args: {shown}"));
        if discarded > 0 {
            text.push_str(&format!("\n      … {discarded} more bytes"));
        }
    }
    if let Some(reason) = &request.reason {
        text.push_str(&format!("\nwhy: {reason}"));
    }
    text
}

/// The approval prompt's full text: body, then the key line. Kept for callers that want one
/// string (tests, a future non-TUI rendering); the box itself draws the two separately.
pub fn approval_text(request: &ApprovalRequest) -> String {
    format!("{}\n{APPROVAL_KEYS}", approval_body(request))
}

/// Truncates a display string on a character boundary, returning the kept part and how many
/// bytes were dropped.
fn truncate_for_display(text: &str, cap: usize) -> (String, usize) {
    if text.len() <= cap {
        return (text.to_owned(), 0);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), text.len() - end)
}

/// The most of the terminal the approval box may take (owner acceptance: ~60%, the transcript
/// keeps at least three rows). `completion` is the rows the `@` list above the box needs.
pub fn approval_cap(screen_height: usize, completion: usize) -> usize {
    let cap = (screen_height * 60 / 100).max(MIN_APPROVAL_HEIGHT);
    cap.min(
        screen_height
            .saturating_sub(completion + 4)
            .max(MIN_APPROVAL_HEIGHT),
    )
}

/// The approval box's floor, border and key line included.
pub const MIN_APPROVAL_HEIGHT: usize = 5;

/// The height the approval box needs: the wrapped body rows (bounded by `cap`) and the
/// panel's two border rows. The body scrolls rather than growing past `cap`.
fn approval_height(request: &ApprovalRequest, width: usize, cap: usize) -> u16 {
    let rows = wrap(&approval_body(request), width.saturating_sub(4).max(1)).len();
    let visible = rows.min(cap.saturating_sub(2).max(1));
    (visible as u16 + 2).clamp(
        MIN_APPROVAL_HEIGHT as u16,
        cap.max(MIN_APPROVAL_HEIGHT) as u16,
    )
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
fn apply_selection(
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

/// One status-bar segment: its text and its style.
struct Segment {
    text: String,
    style: Style,
}

/// The status-bar segments, most important first. The order is the truncation order: a narrow
/// terminal drops the rightmost segments first, so the model, the turn state, the permission
/// tier, the usage and the MCP state survive and only the long context/scroll text can go.
fn status_segments(app: &App) -> Vec<Segment> {
    // A queued `/model` switch (ADR 0026 D5) or `/thinking` level (D11): visible until the
    // driver applies it.
    if let Some(next) = &app.session.next_model {
        return vec![Segment {
            text: format!("next: {next}"),
            style: app.theme.turn_running,
        }];
    }
    if let Some(next) = &app.session.next_thinking {
        return vec![Segment {
            text: format!("next: thinking {next}"),
            style: app.theme.turn_running,
        }];
    }
    // The session facts come from the projection, not from `App` (ADR 0023 U5). One read, so
    // the whole status bar is one instant.
    let session = app.session.projection.read();
    let turn = session.turn();
    let turn_style = match turn {
        TurnState::Idle => app.theme.turn_idle,
        TurnState::Running => app.theme.turn_running,
        TurnState::Cancelled => app.theme.turn_cancelled,
    };
    let position = if app.session.max_scroll == 0 {
        "all".to_string()
    } else {
        format!("{}/{}", app.session.scroll, app.session.max_scroll)
    };
    let mut segments = vec![
        Segment {
            text: format!(" {} ", app.session.model_name),
            style: app.theme.model,
        },
        Segment {
            text: turn.label().to_string(),
            style: turn_style,
        },
    ];
    // Phase 5: the find match position is high priority, so a narrow terminal keeps it.
    if let Some(status) = app.find_state().and_then(crate::find::FindState::status) {
        segments.push(Segment {
            text: status,
            style: app.theme.find,
        });
    }
    // The first idle Ctrl-C armed "press again to quit": the hint sits
    // high, right after the turn state, because it is a mode the user just entered.
    if app.quit_pending.is_some() {
        segments.push(Segment {
            text: "Press Ctrl-C again to quit".to_string(),
            style: app.theme.note,
        });
    }
    if let Some(tier) = session.tier() {
        segments.push(Segment {
            text: tier.to_owned(),
            style: app.theme.tier,
        });
    }
    // ADR 0026 D9: while auto is on the status bar says so, prominently — the session is
    // approving tool calls without asking.
    if app.session.auto {
        segments.push(Segment {
            text: "AUTO".to_string(),
            style: app.theme.auto,
        });
    }
    // ADR 0023 U7: how many requests are waiting. Shown only when there is one, so an idle
    // status bar does not carry a constant `0 pending`.
    if app.pending > 0 {
        segments.push(Segment {
            text: format!("{} pending", app.pending),
            style: app.theme.turn_running,
        });
    }
    // The transient note ("tool … running", "copied N chars (via OSC 52)", a compaction
    // failure) sits ahead of the long tails: a narrow terminal drops the *rightmost*
    // segments, and a note the user just caused must survive that, not be the first to go.
    if let Some(note) = &app.session.note {
        segments.push(Segment {
            text: note.clone(),
            style: app.theme.note,
        });
    }
    segments.push(Segment {
        text: usage_text(session.usage_input(), session.usage_output()),
        style: app.theme.usage,
    });
    segments.push(Segment {
        text: mcp_text(session.mcp_configured(), session.mcp_open()),
        style: app.theme.mcp,
    });
    // ADR 0018: over the input budget `B` the request is still sent, but the status bar
    // warns (the existing warning colour).
    let over_budget = session.budget().is_some_and(agent_core::over_budget);
    segments.push(Segment {
        text: budget_status_text(session.context_window(), session.budget()),
        style: if over_budget {
            app.theme.budget_over
        } else {
            app.theme.budget
        },
    });
    segments.push(Segment {
        text: format!(
            "scroll {position} {}",
            if app.session.follow {
                "follow"
            } else {
                "paused"
            }
        ),
        style: Style::default(),
    });
    segments
}

/// Fits the segments into `width` display columns. Segments that do not fit are dropped from
/// the right (the least important) and a `…` marks the truncation, so the line never wraps
/// and never pushes the model off the screen. The last segment is never dropped, so a very
/// narrow terminal still shows something.
fn fit_segments(segments: Vec<Segment>, width: usize, theme: &Theme) -> Line<'static> {
    const SEP: &str = " │ ";
    let measure = |segments: &[Segment]| -> usize {
        segments
            .iter()
            .map(|segment| UnicodeWidthStr::width(segment.text.as_str()))
            .sum::<usize>()
            + UnicodeWidthStr::width(SEP) * segments.len().saturating_sub(1)
    };
    let mut segments = segments;
    let mut dropped = false;
    while measure(&segments) > width && segments.len() > 1 {
        segments.pop();
        dropped = true;
    }
    // The marker itself needs a column.
    while dropped && measure(&segments) + 1 > width && segments.len() > 1 {
        segments.pop();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(SEP));
        }
        spans.push(Span::styled(segment.text, segment.style));
    }
    if dropped {
        spans.push(Span::styled("…", theme.truncation));
    }
    Line::from(spans)
}

fn status_line(app: &App, width: usize) -> Line<'static> {
    fit_segments(status_segments(app), width, &app.theme)
}

/// Draws one frame. Also feeds the measured transcript size back into `app` so scroll keys
/// operate on wrapped lines.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // ADR 0023 U4: the middle of the frame is drawn by the top modal (the approval box
    // replaces the input box), then the find overlay, then the ordinary input box.
    // `[tui.widgets]` (ADR 0027 C3): one line above and/or below the input box.
    let above_widgets = crate::footer::widget_line(app, &app.widgets.above_input);
    let below_widgets = crate::footer::widget_line(app, &app.widgets.below_input);
    let widget_rows = u16::from(above_widgets.is_some()) + u16::from(below_widgets.is_some());
    // ADR 0035: the completion menu's visible rows are omp's
    // `max(3, min(10, viewport - used - 2))`, where `used` is the input box, the widgets and
    // the status bar.
    let used_rows = input_height(app, area.width as usize) as usize
        + widget_rows as usize
        + status_height(app, area.width as usize) as usize;
    let completion_visible = completion_visible_rows(app, area.height as usize, used_rows);
    let completion = completion_rows(app, area.width as usize, completion_visible);
    let middle = match app.top_modal() {
        Some(Modal::Approval(prompt)) => approval_height(
            &prompt.request,
            area.width as usize,
            approval_cap(area.height as usize, completion_visible),
        ),
        Some(Modal::Question(form)) => {
            // The budget leaves room for the transcript and the footer; the panel windows its
            // option list to it, so the bottom border (the hints) is never clipped.
            let budget = (area.height as usize).saturating_sub(6);
            question_height(
                form,
                &app.session.session_id,
                area.width as usize,
                budget,
                &app.theme,
                app.footer_glyphs(),
            )
        }
        Some(Modal::Login(wizard)) => {
            login_height(wizard, area.width as usize, &app.theme, app.footer_glyphs())
        }
        // The `/logout` confirmation is the shared panel with one row and the hints.
        Some(Modal::ConfirmLogout(provider)) => logout_panel(
            provider,
            area.width as usize,
            &app.theme,
            app.footer_glyphs(),
        )
        .len() as u16,
        // The trust prompt lists up to five options plus a header and the two borders.
        Some(Modal::Trust(wizard)) => {
            trust_height(wizard, area.width as usize, &app.theme, app.footer_glyphs())
        }
        // The `/access full` confirmation: one header row, three options and the two borders.
        Some(Modal::AccessFull(wizard)) => {
            access_height(wizard, area.width as usize, &app.theme, app.footer_glyphs())
        }
        // The find box replaces the input with a **bordered** box: three rows (top border, the
        // query, bottom border). `bar`'s input (its status row plus one text row) can be
        // shorter than that, so the height is raised for the box, never lowered for the others.
        _ if app.find_state().is_some() => {
            input_height(app, area.width as usize).max(FIND_BOX_HEIGHT)
        }
        _ => input_height(app, area.width as usize),
    };
    // The widget lines are part of the middle block.
    let middle = middle.saturating_add(widget_rows);
    // The `@` completion list sits just above the input box (and never over an approval
    // prompt). A zero-height chunk keeps the rest of the layout identical when there is none.
    // The status area's height follows the footer preset (ADR 0027 C2): one row for `classic`
    // (today's line), the configured rows for `pi`.
    let status_height = status_height(app, area.width as usize);
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(completion.len() as u16),
        Constraint::Length(middle),
        Constraint::Length(status_height),
    ])
    .split(area);
    // The middle block: the line above the input, the input/modal itself, the line below.
    let middle_chunks = Layout::vertical([
        Constraint::Length(u16::from(above_widgets.is_some())),
        Constraint::Min(1),
        Constraint::Length(u16::from(below_widgets.is_some())),
    ])
    .split(chunks[2]);
    let input_area = middle_chunks[1];
    if let Some(line) = &above_widgets {
        frame.render_widget(Paragraph::new(line.clone()), middle_chunks[0]);
    }
    if let Some(line) = &below_widgets {
        frame.render_widget(Paragraph::new(line.clone()), middle_chunks[2]);
    }

    // Measure first (cached per entry), so the scroll position is clamped against the real
    // total before the visible window is cut.
    let width = chunks[0].width as usize;
    let height = chunks[0].height as usize;
    let theme = app.theme.clone();
    // The find matcher wraps at the same width; a resize recomputes it (once, not per frame).
    app.set_transcript_width(width);
    let modes = app.display_modes();
    let t_draw = std::time::Instant::now();
    // The live spinner frame (ADR 0027): computed once per frame from the loop's
    // clock, so a live reasoning line and the input rule animate.
    let spinner = app.spinner_frame();
    let (total, _) = visible_lines_with(
        &mut app.session.entries,
        modes,
        width,
        0,
        0,
        &theme,
        spinner,
    );
    radian_session::trace::took("draw/measure", t_draw);
    app.set_viewport(chunks[0].height, total);
    let t_window = std::time::Instant::now();
    let (_, visible) = visible_lines_with(
        &mut app.session.entries,
        modes,
        width,
        app.session.scroll,
        height,
        &theme,
        spinner,
    );
    radian_session::trace::took("draw/window", t_window);
    let visible = match app.find_state() {
        Some(find) => visible
            .into_iter()
            .map(|line| highlight(line, &find.query))
            .collect(),
        None => visible,
    };
    // A live transcript selection: the visible rows' selected display columns, styled with
    // the theme's `selection` role. `highlight_rows` derives from the same wrap the lines
    // above were rendered from, so the band lands exactly on the copied text.
    let visible = match app.session.selection.filter(|sel| !sel.is_empty()) {
        Some(selection) => {
            let ranges = crate::select::highlight_rows(
                &app.session.entries,
                modes,
                width,
                app.session.scroll,
                visible.len(),
                &selection,
            );
            visible
                .into_iter()
                .zip(ranges)
                .map(|(line, ranges)| {
                    if ranges.is_empty() {
                        line
                    } else {
                        apply_selection(line, &ranges, theme.selection)
                    }
                })
                .collect()
        }
        None => visible,
    };
    let t_render = std::time::Instant::now();
    frame.render_widget(Paragraph::new(visible), chunks[0]);
    radian_session::trace::took("draw/render-transcript", t_render);

    // The picker overlay draws over the transcript; the input box and the status bar stay
    // visible below it (ADR 0023 U4).
    if let Some(picker) = app.picker() {
        crate::picker::draw(frame, chunks[0], picker, &app.theme, app.footer_glyphs());
    }

    if !completion.is_empty() {
        frame.render_widget(Paragraph::new(completion), chunks[1]);
    }

    match app.top_modal() {
        Some(Modal::Approval(prompt)) => {
            // ADR 0035: the shared panel draws the frame, the window indicator on
            // the top border and the key hints in the bottom border, so a long command can
            // never push the answers off screen.
            let width = input_area.width as usize;
            let inner_width = width.saturating_sub(4).max(1);
            let body = wrap(&approval_body(&prompt.request), inner_width);
            let total = body.len();
            // The panel's rows are the box height minus its two borders.
            let visible = (input_area.height as usize).saturating_sub(2).max(1);
            // Feed the measurement back, the way the transcript's viewport is fed back, so the
            // scroll keys clamp against real rows.
            let (scroll, total, visible) = {
                let approval = app
                    .approval_modal_mut()
                    .expect("the top modal is an approval");
                approval.total_rows = total;
                approval.visible_rows = visible;
                approval.scroll = approval.scroll.min(total.saturating_sub(visible));
                (approval.scroll, total, visible)
            };
            let first = scroll.min(total.saturating_sub(1));
            let last = (first + visible).min(total);
            let mut panel = crate::panel::Panel::new("approve tool call?");
            if total > visible {
                // The window indicator sits on the top border, where it costs no body row.
                panel = panel.right_title(format!("lines {}–{} of {total}", first + 1, last));
            }
            for row in &body[first..last] {
                panel = panel.row(Line::from(row.clone()));
            }
            // The keys are hardcoded (ADR 0023 U4); the panel puts them in the bottom border.
            panel = panel
                .hint("[y]", "allow once")
                .hint("[a]", "always this session")
                .hint("[n]", "deny")
                .hint("[!]", "auto for this session");
            frame.render_widget(
                Paragraph::new(panel.lines(width, &app.theme, app.footer_glyphs())),
                input_area,
            );
        }
        Some(Modal::Question(form)) => {
            // ADR 0024 R2: the shared panel primitive draws the box, the tab row,
            // the current question and the key hints (the bottom border). The panel is
            // height-aware; a long option list scrolls to the cursor.
            // The same budget the height was measured with, so the window and the frame agree
            // and the bottom border (the hints) stays on screen.
            let budget = (area.height as usize).saturating_sub(6);
            let lines = question_panel(
                form,
                &app.session.session_id,
                input_area.width as usize,
                budget,
                &app.theme,
                app.footer_glyphs(),
            );
            frame.render_widget(Paragraph::new(lines), input_area);
        }
        Some(Modal::Login(wizard)) => {
            let lines = login_panel(
                wizard,
                input_area.width as usize,
                &app.theme,
                app.footer_glyphs(),
            );
            frame.render_widget(Paragraph::new(lines), input_area);
        }
        Some(Modal::Trust(wizard)) => {
            let lines = trust_panel(
                wizard,
                chunks[2].width as usize,
                &app.theme,
                app.footer_glyphs(),
            );
            frame.render_widget(Paragraph::new(lines), chunks[2]);
        }
        Some(Modal::ConfirmLogout(provider)) => {
            let lines = logout_panel(
                provider,
                input_area.width as usize,
                &app.theme,
                app.footer_glyphs(),
            );
            frame.render_widget(Paragraph::new(lines), input_area);
        }
        Some(Modal::AccessFull(wizard)) => {
            let lines = access_panel(
                wizard,
                input_area.width as usize,
                &app.theme,
                app.footer_glyphs(),
            );
            frame.render_widget(Paragraph::new(lines), input_area);
        }
        _ if app.find_state().is_some() => {
            // The find box replaces the input box's line; the input text itself is untouched
            // and comes back when the box closes.
            let find = app.find_state().expect("checked");
            let text = format!("find: {}", find.query);
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" find "))
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, input_area);
        }
        // The `/memory` confirmation and the ordinary state draw the input area, exactly as
        // before (a confirmation only makes it non-editable).
        _ => {
            // pi-style: a rule above and below instead of a box, and
            // `> ` on the first line. `input_height` already reserves the style's chrome (the
            // two rules, the box border or `bar`'s status row), so the layout of everything
            // else is unchanged.
            let inner_width = input_inner_width(app, input_area.width as usize);
            let visible = input_visible_rows(app, input_area.height);
            let text = app.input.text();
            let rows = crate::composer::rows(&text, inner_width);
            let (cursor_row, cursor_col) = app.input.cursor_cell(inner_width);
            // Keep the cursor in view when the text is taller than the area.
            let offset = cursor_row.saturating_sub(visible.saturating_sub(1));
            let offset = offset.min(rows.len().saturating_sub(1));
            let empty = app.input.is_empty();
            let style = if empty {
                app.theme.hint
            } else {
                Style::default()
            };
            // The input rule's running status (ADR 0027): the spinner frame and
            // the state text on the top rule; idle has neither.
            let status = app.spinner_status();
            let spin = app.spinner_frame();
            match app.input_style {
                InputStyle::Lines => {
                    let body: Vec<Line> = if empty {
                        vec![Line::from(vec![
                            Span::styled(INPUT_PROMPT, app.theme.hint),
                            Span::styled(INPUT_HINT, style),
                        ])]
                    } else {
                        rows[offset..]
                            .iter()
                            .enumerate()
                            .map(|(index, row)| {
                                let lead = if index == 0 {
                                    INPUT_PROMPT.to_string()
                                } else {
                                    " ".repeat(prompt_width(INPUT_PROMPT))
                                };
                                Line::from(vec![
                                    Span::styled(lead, app.theme.hint),
                                    Span::styled(row.clone(), style),
                                ])
                            })
                            .collect()
                    };
                    let mut block = Block::default()
                        .borders(Borders::TOP | Borders::BOTTOM)
                        .border_style(app.theme.hint);
                    if let Some(status) = status {
                        // `── ◜  Working ` over the top rule (two spaces between the spinner
                        // and the text, ADR 0027).
                        block = block.title(Line::from(vec![
                            Span::styled("── ", app.theme.hint),
                            Span::styled(spin.glyph, app.spinner_style(spin)),
                            Span::styled(format!("  {status} "), app.theme.turn_running),
                        ]));
                    }
                    frame.render_widget(Paragraph::new(body).block(block), input_area);
                }
                InputStyle::Box => {
                    // judged against ADR 0029: the accent level is
                    // for labels and attention surfaces. `notice_label` is the theme's general
                    // accent role (pi's `accent`), so the rounded border and the `❯` prompt use
                    // it; the body stays on the ordinary text level. (`model` is the model's
                    // own display color, not chrome.)
                    let accent = app.theme.notice_label;
                    let mut body: Vec<Line> = Vec::new();
                    if empty {
                        body.push(Line::from(vec![
                            Span::styled(BOX_PROMPT, accent),
                            Span::styled(INPUT_HINT, style),
                        ]));
                    } else {
                        for (index, row) in rows[offset..].iter().enumerate() {
                            let lead = if index == 0 {
                                BOX_PROMPT.to_string()
                            } else {
                                " ".repeat(prompt_width(BOX_PROMPT))
                            };
                            body.push(Line::from(vec![
                                Span::styled(lead, accent),
                                Span::styled(row.clone(), style),
                            ]));
                        }
                    }
                    let title: Line = match status {
                        // The status replaces the box's ` message ` title while running
                        // (ADR 0027: the same content on the border's top edge).
                        Some(status) => Line::from(vec![
                            Span::styled(" ", accent),
                            Span::styled(spin.glyph, app.spinner_style(spin)),
                            Span::styled(format!("  {status} "), app.theme.turn_running),
                        ]),
                        None => Line::from(" message "),
                    };
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .border_style(accent)
                        .title(title);
                    frame.render_widget(Paragraph::new(body).block(block), input_area);
                }
                InputStyle::Bar => {
                    // ADR 0035 V3 (omp's `rail`): one background band per row with the accent
                    // rail on the left, no border and no rules. The status row is the area's
                    // first row **always**, so the band's own rows start one below it and a
                    // turn starting or ending never moves anything.
                    let band = app.theme.input_bar;
                    // The rail takes the accent (pi's `accent`, the role the `box` border uses)
                    // over the band; the text keeps its own level — main, or the weak
                    // placeholder — and the band only supplies the background behind it
                    // (`band.patch(style)` lets the text's own color win).
                    let rail_style = band.patch(app.theme.notice_label);
                    let text_style = band.patch(style);
                    let area_width = input_area.width as usize;
                    let mut body: Vec<Line> = vec![bar_status_line(app)];
                    // The band's half-cell pads (ADR 0035/4): half a row of the band
                    // above and below the text rows, so the band reads as extending half a cell
                    // beyond its text, with the rail continued into them as `▖`/`▘` so it is as
                    // tall as the band. The `ascii` tier has none.
                    if bar_pad_rows(app) == 2 {
                        body.push(bar_pad_line(
                            "▖",
                            app.theme.notice_label,
                            "▄",
                            area_width,
                            band,
                        ));
                    }
                    let text_rows: Vec<String> = if empty {
                        vec![INPUT_HINT.to_string()]
                    } else {
                        rows[offset..].to_vec()
                    };
                    for row in &text_rows {
                        let mut spans = vec![
                            Span::styled(bar_rail(app).to_string(), rail_style),
                            // The one cell of padding after the rail carries the band, never
                            // the text's own style.
                            Span::styled(" ".to_string(), band),
                            Span::styled(row.clone(), text_style),
                        ];
                        // Fill the rest of the row with the band, so it covers the whole width
                        // (a band that stops at the text reads as a highlight, not a surface).
                        let used = BAR_LEAD + UnicodeWidthStr::width(row.as_str());
                        if used < area_width {
                            spans.push(Span::styled(" ".repeat(area_width - used), band));
                        }
                        body.push(Line::from(spans));
                    }
                    if bar_pad_rows(app) == 2 {
                        body.push(bar_pad_line(
                            "▘",
                            app.theme.notice_label,
                            "▀",
                            area_width,
                            band,
                        ));
                    }
                    frame.render_widget(Paragraph::new(body), input_area);
                }
            }
            if app.input_editable() {
                // The text starts after the prompt (`> `, two cells), after the box border plus
                // the `❯ ` prompt, or after `bar`'s rail plus its one cell of padding; a full
                // row clamps the cursor to its last cell rather than onto the rule or border.
                let lead = match app.input_style {
                    InputStyle::Lines => prompt_width(INPUT_PROMPT) as u16,
                    // Border (1) + the `❯ ` prompt (2 cells).
                    InputStyle::Box => 1 + prompt_width(BOX_PROMPT) as u16,
                    InputStyle::Bar => BAR_LEAD as u16,
                };
                let col = cursor_col.min(inner_width.saturating_sub(1)) as u16;
                let row = (cursor_row - offset).min(visible.saturating_sub(1)) as u16;
                // The text rows start below the style's top chrome: the rule (or box border),
                // or `bar`'s status row **and** its top half-cell padding row (ADR 0035
                // revision 3).
                let top_chrome = match app.input_style {
                    InputStyle::Lines | InputStyle::Box => 1,
                    // The status row, plus the band's top half-cell pad when the tier has one.
                    InputStyle::Bar => {
                        1 + u16::from(crate::blocks::banded_pad(app.footer_glyphs()))
                    }
                };
                let x = input_area.x + lead + col;
                let y = input_area.y + top_chrome + row;
                frame.set_cursor_position((x, y));
            }
        }
    }

    let t_status = std::time::Instant::now();
    let status = status_lines(app, chunks[3].width as usize);
    radian_session::trace::took("draw/status-lines", t_status);
    frame.render_widget(Paragraph::new(status), chunks[3]);
    radian_session::trace::mark("draw/end");
}

/// The status area's height: one row for the `classic` preset, the footer's rows for `pi`.
fn status_height(app: &App, width: usize) -> u16 {
    status_lines(app, width).len().clamp(1, 4) as u16
}

/// The status area's lines: the `classic` one-liner, or the `pi` footer's rows plus the
/// **chrome** line when a mode is active (a queued switch, the find status, the armed quit
/// hint, the AUTO marker, a transient note). Those are not configurable items: they are states
/// the UI must show regardless of the footer layout (ADR 0026 D9, review).
fn status_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    match app.footer.preset() {
        crate::footer::Preset::Classic => vec![status_line(app, width)],
        crate::footer::Preset::Deck | crate::footer::Preset::Full | crate::footer::Preset::Pi => {
            let mut lines = app.footer.lines(app, width);
            if let Some(chrome) = chrome_line(app, width) {
                lines.push(chrome);
            }
            lines
        }
    }
}

/// The chrome segments that survive any footer layout, as one line (empty when none is on).
fn chrome_line(app: &App, width: usize) -> Option<Line<'static>> {
    let mut segments = Vec::new();
    if let Some(next) = &app.session.next_model {
        segments.push(Segment {
            text: format!("next: {next}"),
            style: app.theme.turn_running,
        });
    }
    if let Some(next) = &app.session.next_thinking {
        segments.push(Segment {
            text: format!("next: thinking {next}"),
            style: app.theme.turn_running,
        });
    }
    if let Some(status) = app.find_state().and_then(crate::find::FindState::status) {
        segments.push(Segment {
            text: status,
            style: app.theme.find,
        });
    }
    if app.quit_pending.is_some() {
        segments.push(Segment {
            text: "Press Ctrl-C again to quit".to_string(),
            style: app.theme.note,
        });
    }
    if app.session.auto {
        segments.push(Segment {
            text: "AUTO".to_string(),
            style: app.theme.auto,
        });
    }
    if let Some(note) = &app.session.note {
        segments.push(Segment {
            text: note.clone(),
            style: app.theme.note,
        });
    }
    (!segments.is_empty()).then(|| fit_segments(segments, width, &app.theme))
}

/// The most rows a question form's box may take, border included (ADR 0024 Q7: a long option
/// list scrolls instead of growing the box without bound).
const MAX_QUESTION_ROWS: u16 = 18;

/// The question form's key line, drawn on its own row under the content so a long option list
/// can never push it out of the box (same fix as the approval box's `APPROVAL_KEYS`).
pub const QUESTION_KEYS: &str = "Tab next · Space select · Enter next/submit · Esc decline";

/// The height the question form's box needs: its content plus the key line, bounded.
fn question_height(
    form: &QuestionForm,
    own_session: &str,
    width: usize,
    max_rows: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    let rows = question_panel(form, own_session, width, max_rows, theme, glyphs).len();
    (rows as u16).clamp(5, MAX_QUESTION_ROWS)
}

/// The `ask_user` panel (ADR 0024 R2): the shared [`crate::panel::Panel`] with the
/// tab row, the current question's options (or its text field), the `Submit` list, and the
/// key hints in the bottom border. Pure, so the layout is testable without a terminal.
pub fn question_panel(
    form: &QuestionForm,
    own_session: &str,
    width: usize,
    max_rows: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    // `alt-h`: one line, so the transcript is readable.
    if form.hidden {
        let answered = form
            .questions
            .iter()
            .zip(&form.answers)
            .filter(|(question, answer)| QuestionForm::is_answered(question, answer))
            .count();
        return vec![Line::from(vec![
            Span::styled("Ask", theme.notice_label),
            Span::styled(
                format!(" · {answered}/{} answered · ", form.len()),
                theme.notice_body,
            ),
            Span::styled("alt-h", theme.hint),
            Span::styled(" to show", theme.notice_body),
        ])];
    }
    let inner = width.saturating_sub(4).max(1);
    // ADR 0024 rule 4: `Ask` alone when the asker is this process's own session
    // (the ordinary single-session TUI); the origin only distinguishes several sessions. A
    // TUI process has one session today (ADR 0030 slice 0), so origin == own session is the
    // clean test.
    let title = if form.origin.is_empty() || form.origin == own_session {
        "Ask".to_string()
    } else {
        format!("Ask · {}", form.origin)
    };
    let mut panel = crate::panel::Panel::new(title)
        .row(tab_row(form, inner, theme))
        .divider();
    match form.current_question() {
        Some(question) => {
            let answer = form
                .current_answer()
                .expect("a current question has an answer");
            for line in wrap_char_offsets(&question.prompt, inner) {
                let text: String = line
                    .chars
                    .into_iter()
                    .filter_map(|i| {
                        question
                            .prompt
                            .get(i..)
                            .and_then(|rest| rest.chars().next())
                    })
                    .collect();
                panel = panel.row(Line::from(Span::styled(text, theme.notice_label)));
            }
            panel = panel.divider();
            if question.kind == radian_coding::QuestionKind::Text {
                panel = panel.row(composer_line(&answer.text, theme));
            } else {
                // The option rows (a description is its own row), then a window that keeps the
                // cursor visible and the bottom border (the hints) on screen.
                let mut rows: Vec<(Line<'static>, bool)> = Vec::new();
                for (row, option) in question.options.iter().enumerate() {
                    rows.push((
                        option_line(question, answer, row, theme, glyphs),
                        row == answer.cursor,
                    ));
                    if let Some(description) = &option.description {
                        let prefix = match glyphs {
                            crate::footer::Glyphs::Ascii => "->",
                            _ => "↳",
                        };
                        rows.push((
                            Line::from(Span::styled(
                                clip_columns(&format!("   {prefix} {description}"), inner),
                                theme.notice_body,
                            )),
                            false,
                        ));
                    }
                }
                if question.allow_other {
                    rows.push((
                        other_line(question, answer, theme, glyphs),
                        answer.cursor == question.options.len(),
                    ));
                }
                // The fixed rows: the top border, the tab row, two dividers, the prompt, the
                // note (when open), the decline row, and the bottom border.
                let note_rows = if form.note_focused() { 3 } else { 0 };
                let fixed = 1
                    + 1
                    + 1
                    + question.prompt.lines().count().max(1)
                    + 1
                    + note_rows
                    + usize::from(form.confirm_decline)
                    + 1
                    + 1;
                let budget = max_rows.saturating_sub(fixed).max(1);
                let cursor = rows.iter().position(|(_, cursor)| *cursor).unwrap_or(0);
                let first = cursor
                    .saturating_sub(budget.saturating_sub(1))
                    .min(rows.len().saturating_sub(budget));
                if first > 0 {
                    panel = panel.row(Line::from(Span::styled(
                        format!("  … {first} above"),
                        theme.hint,
                    )));
                }
                let last = (first + budget).min(rows.len());
                for (line, _) in rows.drain(first..last) {
                    panel = panel.row(line);
                }
                if rows.len() > last {
                    panel = panel.row(Line::from(Span::styled(
                        format!("  … {} more", rows.len() - last),
                        theme.hint,
                    )));
                }
            }
            if form.note_focused() {
                panel = panel.divider();
                panel = panel.row(Line::from(Span::styled("note", theme.hint)));
                panel = panel.row(composer_line(&answer.note, theme));
            }
        }
        // The `Submit` tab: one line per question, the header and the answer summary.
        None => {
            for (index, question) in form.questions.iter().enumerate() {
                let answer = &form.answers[index];
                let answered = QuestionForm::is_answered(question, answer);
                let summary = if answered {
                    answer_text(question, answer)
                } else {
                    "(not answered)".to_string()
                };
                let style = if !answered && question.required {
                    theme.tool_result_error
                } else {
                    theme.notice_body
                };
                let mark = if answered { "✓" } else { "·" };
                panel = panel.row(Line::from(Span::styled(
                    clip_columns(&format!("{mark} {} — {summary}", question.header()), inner),
                    style,
                )));
            }
        }
    }
    // The first Esc asks again, so it never looks like it did nothing (ADR 0024 Q7).
    if form.confirm_decline {
        panel = panel.row(Line::from(Span::styled(
            clip_columns(
                "press Esc again to decline this question (Enter keeps it)",
                inner,
            ),
            theme.note,
        )));
    }
    for hint in form.hints() {
        panel = panel.hint(hint.keys, hint.description);
    }
    panel.lines(width, theme, glyphs)
}

/// The tab row (ADR 0024 R2): one tab per question plus `Submit`, the current one
/// reversed. Each label is capped at [`crate::question::MAX_TAB_WIDTH`] and the row is clipped
/// (revision 2: no horizontal scrolling).
fn tab_row(form: &QuestionForm, width: usize, theme: &Theme) -> Line<'static> {
    let current = form.current.unwrap_or(form.questions.len());
    let mut spans = Vec::new();
    let mut used = 0;
    for (index, (label, _)) in form.tab_labels().iter().enumerate() {
        let text = format!(" {label} ");
        let text_width = UnicodeWidthStr::width(text.as_str());
        if used + text_width > width {
            break;
        }
        used += text_width;
        let style = if index == current {
            theme.notice_label.add_modifier(Modifier::REVERSED)
        } else {
            theme.notice_body
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// One option row: the cursor, the mark and the label.
fn option_line(
    question: &radian_coding::Question,
    answer: &crate::question::FormAnswer,
    row: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Line<'static> {
    let option = &question.options[row];
    let cursor = if row == answer.cursor { "❯ " } else { "  " };
    let chosen = answer.selected.contains(&option.id);
    // ADR 0024 implementation note: one marker source (`AskGlyphs`), the same
    // checkbox pair for single and multi.
    let g = blocks::AskGlyphs::for_glyphs(glyphs);
    let mark = match (question.kind, chosen) {
        (radian_coding::QuestionKind::Multi, true) => g.multi_on,
        (radian_coding::QuestionKind::Multi, false) => g.multi_off,
        (_, true) => g.single_on,
        (_, false) => g.single_off,
    };
    let recommended = if option.recommended {
        " (Recommended)"
    } else {
        ""
    };
    // The cursor and the marker are drawn in their own spans and are never bold: the font's
    // bold face can lack the glyph and fall back to a different size (the owner's report). The
    // marker keeps its label's role (the chosen accent / the weak body), just not the weight.
    let label_style = if chosen {
        theme.notice_label
    } else {
        theme.notice_body
    };
    let marker_style = label_style.remove_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(cursor, theme.notice_label.remove_modifier(Modifier::BOLD)),
        Span::styled(mark, marker_style),
        Span::styled(format!(" {}{recommended}", option.label), label_style),
    ])
}

/// The `Other` row: `Other (type your own)`, with the typed text when chosen.
fn other_line(
    question: &radian_coding::Question,
    answer: &crate::question::FormAnswer,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Line<'static> {
    let cursor = if answer.cursor == question.options.len() {
        "❯ "
    } else {
        "  "
    };
    // ADR 0024 implementation note: `Other` uses the **same** checkbox pair as the
    // options (it used to always draw a circle, even in a multi question), from `AskGlyphs`.
    let g = blocks::AskGlyphs::for_glyphs(glyphs);
    let mark = if answer.other {
        g.single_on
    } else {
        g.single_off
    };
    let text = if answer.other && !answer.text.is_empty() {
        format!("Other: {}", answer.text.text())
    } else {
        "Other (type your own)".to_string()
    };
    // The marker and the cursor are never bold, like `option_line`.
    Line::from(vec![
        Span::styled(cursor, theme.notice_label.remove_modifier(Modifier::BOLD)),
        Span::styled(mark, theme.notice_body),
        Span::styled(format!(" {text}"), theme.notice_body),
    ])
}

/// A free-text field (`text`, `Other`, a note): `> ` and the text.
fn composer_line(composer: &crate::composer::Composer, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("> ", theme.notice_label),
        Span::styled(composer.text(), theme.text),
    ])
}

/// One answer as readable text, using option labels.
fn answer_text(question: &radian_coding::Question, answer: &crate::question::FormAnswer) -> String {
    let mut parts: Vec<String> = answer
        .selected
        .iter()
        .map(|id| {
            question
                .options
                .iter()
                .find(|option| &option.id == id)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect();
    if !answer.text.is_empty() {
        parts.push(answer.text.text());
    }
    if parts.is_empty() {
        "(not answered)".to_string()
    } else {
        parts.join(", ")
    }
}

/// Cuts `text` to `width` display columns, never splitting a wide character (ADR 0024 Q7: a
/// narrow terminal clips instead of wrapping).
/// Cuts `text` to `width` display cells (the question form's tab labels use it).
pub(crate) fn clip_display(text: &str, width: usize) -> String {
    clip_columns(text, width)
}

fn clip_columns(text: &str, width: usize) -> String {
    if width == 0 || UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell + 1 > width {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push('…');
    out
}

/// The most rows the completion menu shows (omp's `#autocompleteMaxVisible`).
pub const COMPLETION_MAX_LINES: usize = 10;

/// The completion menu's visible rows: omp's `max(3, min(10, viewport - used - 2))`. Zero when
/// no menu is up.
pub fn completion_visible_rows(app: &App, viewport: usize, used: usize) -> usize {
    if app.has_modal() || app.completion().is_none() {
        return 0;
    }
    viewport
        .saturating_sub(used + 2)
        .clamp(3, COMPLETION_MAX_LINES)
}

/// The completion menu's rows (ADR 0035): the shared list view, so the window
/// follows the highlight and the overflow shows a scrollbar instead of a `+N more` line.
pub fn completion_rows(app: &App, width: usize, visible: usize) -> Vec<Line<'static>> {
    if app.has_modal() {
        return Vec::new();
    }
    let Some(candidates) = app.completion() else {
        return Vec::new();
    };
    let items: Vec<crate::select_list::Item> = candidates
        .iter()
        .map(|candidate| {
            crate::select_list::Item::new(candidate.insert.clone())
                .description(candidate.description.clone())
        })
        .collect();
    crate::select_list::view(
        &app.theme,
        &items,
        &app.completion_filter(),
        app.completion_cursor,
        visible,
        width,
        false,
    )
    .lines
}

/// The body spans of one row: a structured notice's label split, a Markdown line's pieces, a
/// syntax-highlighted code line, a diagram, a weak reason, or the plain body. Shared by the
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
    // with the accent role and keeps the body on pi's secondary level (ADR 0028, owner
    // decision). Only the first: a wrapped continuation that happens to contain a colon
    // (`https://…`, `12:30`, a `path:line`) is prose, not a label.
    if first_text
        && matches!(entry.kind, EntryKind::Notice | EntryKind::Startup)
        && let Some((label, rest)) = split_label(&row.text)
    {
        out.push(Span::styled(label.to_owned(), theme.notice_label));
        out.push(Span::styled(rest.to_owned(), theme.notice_body));
    } else if let Some(md) = &row.md {
        // Assistant Markdown (ADR 0028 R2): each piece takes its own role's style.
        for piece in md {
            let style = md_style(piece.style, body_style, theme);
            out.push(Span::styled(piece.text.clone(), style));
        }
    } else if let Some(syntax) = &row.syntax {
        // A syntax-highlighted code line (ADR 0028 R2): each piece takes its Radian role's
        // style; `Plain` keeps the code block's own style. The text is unchanged, so
        // selection, copy and search are unaffected.
        for (text, role) in syntax {
            let style = syntax_style(*role, body_style, theme);
            out.push(Span::styled(text.clone(), style));
        }
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

/// The style one per-piece role takes (ADR 0028 R2's Markdown roles, ADR 0029's thin roles):
/// the tool head's name and status glyphs included. One definition for the head and for every
/// Markdown line.
fn md_style(style: blocks::MdStyle, body_style: Style, theme: &Theme) -> Style {
    match style {
        blocks::MdStyle::Heading => theme.md_heading,
        blocks::MdStyle::Heading1 => theme.md_heading.add_modifier(Modifier::UNDERLINED),
        blocks::MdStyle::HeadingMark => theme.md_heading_mark,
        blocks::MdStyle::Strikethrough => theme.text.add_modifier(Modifier::CROSSED_OUT),
        blocks::MdStyle::TaskUnchecked | blocks::MdStyle::TaskChecked => theme.md_list_bullet,
        blocks::MdStyle::Fence => theme.truncation,
        blocks::MdStyle::Bold => theme.md_heading.patch(Modifier::BOLD),
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
        // The `ask_user` record's open frame and the tool head's tool name (ADR 0028
        // decision 1/2).
        blocks::MdStyle::Frame => theme.frame,
        blocks::MdStyle::ToolName => theme.tool_name,
        blocks::MdStyle::StatusOk => theme.status_ok,
        blocks::MdStyle::StatusError => theme.status_error,
        // Handled by [`render_head_row`], which draws the live frame instead of a stored glyph:
        // a non-head path never meets one.
        blocks::MdStyle::StatusRunning => theme.status_ok,
        blocks::MdStyle::Plain => body_style,
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

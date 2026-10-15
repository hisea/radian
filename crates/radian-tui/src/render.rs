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
    App, Entry, EntryKind, Modal, TurnState, budget_status_text, mcp_text, usage_text,
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

/// The visible marker for an entry kind (the transcript's `> `, `~ `, …). Also the cell
/// offset the selection mapping ([`crate::select`]) subtracts to get a body column.
pub(crate) fn marker_prefix(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::User => "> ",
        EntryKind::Assistant => "  ",
        EntryKind::Reasoning => "~ ",
        // Rule 2: every marker is two cells; rule 4: a result's marker sits one cell further
        // right, so a result reads as subordinate to its call.
        EntryKind::ToolCall => "->",
        EntryKind::ToolResult => " <-",
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

fn marker(kind: EntryKind, theme: &Theme) -> (&'static str, Style) {
    let style = match kind {
        EntryKind::User => theme.user,
        EntryKind::Assistant => Style::default(),
        EntryKind::Reasoning => theme.reasoning,
        EntryKind::ToolCall => theme.tool_call,
        EntryKind::ToolResult => theme.tool_result,
        EntryKind::Notice => theme.notice,
        EntryKind::Banner => theme.hint,
        EntryKind::Startup => theme.hint,
        EntryKind::Warning => theme.note,
        // The `Tip:` label steps down (dim), the body is muted.
        EntryKind::Tip => theme.truncation,
        // The compaction one-liner is structural: the hunk/accent role.
        EntryKind::Compaction => theme.diff_hunk,
        // ADR 0033: `!` uses the command color; `!!` steps down to the hint level.
        EntryKind::Bash {
            exclude_from_context: false,
        } => theme.tool_call,
        EntryKind::Bash {
            exclude_from_context: true,
        } => theme.hint,
    };
    (marker_prefix(kind), style)
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
        EntryKind::User | EntryKind::Assistant => {
            if entry.interrupted {
                Style::default().add_modifier(Modifier::ITALIC)
            } else {
                Style::default()
            }
        }
    }
}

/// The background band an entry's block paints across the transcript (ADR 0028 R4 + owner
/// acceptance): a user message uses the `user` role's background, a tool card the state's
/// (`tool_pending_bg` / `tool_success_bg` / `tool_error_bg`). `None` means no band — the
/// built-in themes without a user background, and every other kind.
fn card_band(entry: &Entry, theme: &Theme) -> Option<Style> {
    let role = match entry.kind {
        EntryKind::User => theme.user,
        EntryKind::ToolCall => match entry.result {
            None => theme.tool_pending_bg,
            Some(false) => theme.tool_success_bg,
            Some(true) => theme.tool_error_bg,
        },
        EntryKind::ToolResult if entry.is_error => theme.tool_error_bg,
        EntryKind::ToolResult => theme.tool_success_bg,
        _ => return None,
    };
    // Background and modifiers only: the text under the band keeps its own foreground.
    let mut band = Style::default().add_modifier(role.add_modifier);
    if let Some(bg) = role.bg {
        band = band.bg(bg);
    }
    (band.bg.is_some() || !band.add_modifier.is_empty()).then_some(band)
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
) -> Vec<Line<'static>> {
    let (prefix, prefix_style) = marker(entry.kind, theme);
    let band = card_band(entry, theme);
    let blocks = blocks::parse_blocks_for(entry, modes.reasoning);
    let body = blocks::layout(entry, &blocks, modes, width);
    let style = body_style(entry, theme);
    let text_rows: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.pad)
        .map(|(i, _)| i)
        .collect();
    let first_text = text_rows.first().copied();
    let last = text_rows.last().copied();
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
            band,
            width,
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

/// One laid-out [`BodyLine`] as styled spans. `first_text` puts the entry's marker prefix on
/// the lead; a padding row renders as an empty line; a code line has no lead at all (it is
/// full-width by design, ADR 0028 R9). A block with a band (ADR 0028 R4) is padded out to the
/// full transcript width, so the band covers the row's trailing cells too.
#[allow(clippy::too_many_arguments)]
fn render_row(
    entry: &Entry,
    row: &BodyLine,
    prefix: &str,
    prefix_style: Style,
    style: Style,
    theme: &Theme,
    first_text: bool,
    band: Option<Style>,
    width: usize,
) -> Line<'static> {
    if row.pad {
        return match band {
            // The blank rows of a banded block carry the band too.
            Some(band) => Line::from(Span::styled(" ".repeat(width), band)),
            None => Line::from(vec![Span::raw(String::new())]),
        };
    }
    // The collapsed reasoning placeholder is one synthetic line: its marker is the marker
    // column's own glyph (ADR 0028 rule 5), so the body column does not move
    // between folded and expanded. `>` stands in for `▸` (ADR 0027: `▸` is not in
    // all seven checked fonts).
    let placeholder = entry.kind == EntryKind::Reasoning && row.hint;
    let lead = if row.code {
        String::new()
    } else if placeholder {
        "> ".to_string()
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
    } else if row.role != crate::tools::LineRole::Normal {
        // A card body line's own role (ADR 0028 R4 + ADR 0029's diff roles): the warning
        // channel and the unified-diff line kinds.
        match row.role {
            crate::tools::LineRole::Warn => theme.note,
            crate::tools::LineRole::DiffAdded => theme.diff_added,
            crate::tools::LineRole::DiffRemoved => theme.diff_removed,
            crate::tools::LineRole::DiffContext => theme.diff_context,
            crate::tools::LineRole::DiffHunk => theme.diff_hunk,
            crate::tools::LineRole::Normal => style,
        }
    } else {
        style
    };
    let lead_width = UnicodeWidthStr::width(lead.as_str());
    let mut spans = Vec::with_capacity(4);
    let lead_style = match band {
        Some(band) => prefix_style.patch(band),
        None => prefix_style,
    };
    let body_style = match band {
        Some(band) => body.patch(band),
        None => body,
    };
    spans.push(Span::styled(lead, lead_style));
    // A structured notice's **first** line (`profile: …`, `[memory] …`) steps the label up
    // with the accent role and keeps the body on pi's secondary level (ADR 0028, owner
    // decision). Only the first: a wrapped continuation that happens to contain a colon
    // (`https://…`, `12:30`, a `path:line`) is prose, not a label.
    if first_text
        && matches!(entry.kind, EntryKind::Notice | EntryKind::Startup)
        && let Some((label, rest)) = split_label(&row.text)
    {
        let label_style = match band {
            Some(band) => theme.notice_label.patch(band),
            None => theme.notice_label,
        };
        let rest_style = match band {
            Some(band) => theme.notice_body.patch(band),
            None => theme.notice_body,
        };
        spans.push(Span::styled(label.to_owned(), label_style));
        spans.push(Span::styled(rest.to_owned(), rest_style));
    } else if let Some(md) = &row.md {
        // Assistant Markdown (ADR 0028 R2): each piece takes its own role's style.
        for piece in md {
            let style = match piece.style {
                blocks::MdStyle::Heading => theme.md_heading,
                blocks::MdStyle::Bold => theme.md_heading.patch(Modifier::BOLD),
                blocks::MdStyle::Italic => theme.text.add_modifier(Modifier::ITALIC),
                blocks::MdStyle::Code => theme.md_code,
                blocks::MdStyle::Link => theme.md_link,
                blocks::MdStyle::LinkUrl => theme.md_link_url,
                blocks::MdStyle::Quote => theme.md_quote,
                blocks::MdStyle::QuoteBar => theme.md_quote_border,
                blocks::MdStyle::Bullet => theme.md_list_bullet,
                blocks::MdStyle::Rule => theme.md_hr,
                blocks::MdStyle::TableBorder => theme.md_hr,
                blocks::MdStyle::Plain => body_style,
            };
            let style = match band {
                Some(band) => style.patch(band),
                None => style,
            };
            spans.push(Span::styled(piece.text.clone(), style));
        }
    } else if let Some(syntax) = &row.syntax {
        // A syntax-highlighted code line (ADR 0028 R2): each piece takes its Radian role's
        // style; `Plain` keeps the code block's own style. The text is unchanged, so
        // selection, copy and search are unaffected.
        for (text, role) in syntax {
            let style = match role {
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
            };
            let style = match band {
                Some(band) => style.patch(band),
                None => style,
            };
            spans.push(Span::styled(text.clone(), style));
        }
    } else if let Some(diagram) = &row.diagram {
        // A rendered diagram (ADR 0028 R8): each piece takes its own role's style.
        for (piece, role) in diagram {
            let style = match role {
                blocks::DiagramRole::Node => theme.mermaid_node,
                blocks::DiagramRole::Edge => theme.mermaid_edge,
                blocks::DiagramRole::Text => theme.mermaid_text,
            };
            let style = match band {
                Some(band) => style.patch(band),
                None => style,
            };
            spans.push(Span::styled(piece.clone(), style));
        }
    } else if row.reason {
        // A weak one-line reason (`mermaid: …`, ADR 0028 R8): the dim/truncation role.
        spans.push(Span::styled(row.text.clone(), theme.truncation));
    } else {
        spans.push(Span::styled(row.text.clone(), body_style));
    }
    if let Some(band) = band {
        // Pad to the full width so the band reaches the row's end (owner acceptance:
        // including the row's trailing whitespace).
        let used = lead_width + UnicodeWidthStr::width(row.text.as_str());
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), band));
        }
    }
    Line::from(spans)
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
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        // The container owns the blank line between blocks (ADR 0028 rule 1).
        out.extend(std::iter::repeat_n(
            Line::from(""),
            blocks::block_gap(entries.get(index.wrapping_sub(1)), entry),
        ));
        out.extend(entry_lines(entry, modes, width, theme));
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
            let body = blocks::layout(entry, &blocks, modes, width);
            let prefix = marker_prefix(entry.kind);
            let prefix_style = marker(entry.kind, theme).1;
            let style = body_style(entry, theme);
            let band = card_band(entry, theme);
            let text_rows: Vec<usize> = body
                .iter()
                .enumerate()
                .filter(|(_, line)| !line.pad)
                .map(|(i, _)| i)
                .collect();
            let first_text = text_rows.first().copied();
            let last = text_rows.last().copied();
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
                            band,
                            width,
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

/// How the input area is drawn (config `tui.input_style`, owner UI review 2026-10-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputStyle {
    /// A rule above and below, `> ` on the first line, continuation lines indented two cells.
    /// The default.
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

/// The input area's height: one rule above, the text rows, one rule below.
/// The `/login` box's height (rows in `login_lines`, plus the two border rows).
const LOGIN_HEIGHT: u16 = 8;

/// The wizard's lines, styled with the app theme's levels (ADR 0028): the prompt is main
/// text, the selected choice is the emphasis (accent), the rest is secondary, and the
/// key hints are weak. **The key is never among them** (D3): a filled field renders one `*`
/// per character, an empty one its hint.
fn login_lines(wizard: &LoginWizard, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    use crate::login::LoginStep;
    let _ = width;
    let prompt = |text: &str| Line::from(Span::styled(text.to_owned(), theme.text));
    let value = |text: String| Line::from(Span::styled(text, theme.text));
    let hint = |text: &'static str| Span::styled(text.to_owned(), theme.hint);
    let mut lines: Vec<Line> = Vec::new();
    match &wizard.step {
        LoginStep::ChooseProvider { cursor } => {
            lines.push(Line::from(vec![
                Span::styled("choose a provider ".to_owned(), theme.text),
                hint("(↑/↓, Enter)"),
                Span::styled(":".to_owned(), theme.text),
            ]));
            for (index, (id, description)) in LoginWizard::choices().into_iter().enumerate() {
                let marker = if index == *cursor { "> " } else { "   " };
                let style = if index == *cursor {
                    theme.notice_label
                } else {
                    theme.notice_body
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {marker} "), style),
                    Span::styled(format!("{id:<20}"), style),
                    Span::styled(description, theme.hint),
                ]));
            }
        }
        LoginStep::CustomName => {
            lines.push(prompt("provider name (also its auth.json entry):"));
            lines.push(value(format!(" {}", wizard.name)));
        }
        LoginStep::CustomProtocol { cursor } => {
            lines.push(Line::from(vec![
                Span::styled("wire protocol ".to_owned(), theme.text),
                hint("(↑/↓, Enter)"),
                Span::styled(":".to_owned(), theme.text),
            ]));
            for (index, kind) in crate::login::PROTOCOLS.iter().enumerate() {
                let marker = if index == *cursor { "> " } else { "   " };
                let style = if index == *cursor {
                    theme.notice_label
                } else {
                    theme.notice_body
                };
                lines.push(Line::from(Span::styled(format!(" {marker} {kind}"), style)));
            }
        }
        LoginStep::CustomBaseUrl => {
            lines.push(prompt("base URL:"));
            lines.push(value(format!(" {}", wizard.base_url)));
        }
        LoginStep::KeyInput => {
            lines.push(prompt("API key (hidden; paste works too):"));
            let masked = crate::login::LoginWizard::masked(&wizard.key);
            lines.push(value(if masked.is_empty() {
                String::new()
            } else {
                masked
            }));
        }
        LoginStep::Verifying => {
            lines.push(Line::from(Span::styled(
                format!("verifying {} … ", wizard.provider_name()),
                theme.text,
            )));
            lines
                .last_mut()
                .expect("just pushed")
                .spans
                .push(hint("(Esc cancels)"));
        }
        LoginStep::VerifyFailed { reason } => {
            lines.push(Line::from(Span::styled(
                format!("verification failed: {reason}"),
                theme.turn_cancelled,
            )));
            lines.push(Line::from(vec![
                Span::styled("save anyway? ".to_owned(), theme.text),
                Span::styled("[y] save   ".to_owned(), theme.hint),
                Span::styled("[n/Esc] cancel".to_owned(), theme.hint),
            ]));
        }
    }
    if let Some(error) = &wizard.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            theme.turn_cancelled,
        )));
    }
    lines
}

/// The error style for wizard problems (a note-colored line; no new theme role).
fn input_height(app: &App, width: usize) -> u16 {
    let inner = input_inner_width(app, width);
    let rows = crate::composer::rows(&app.input.text(), inner).len();
    (rows.clamp(1, MAX_INPUT_ROWS) as u16) + 2
}

/// The display width of a prompt constant: the columns it occupies, **not** its byte length
/// (`❯` is three bytes and one cell). Every layout use of a prompt goes through this.
fn prompt_width(prompt: &str) -> usize {
    UnicodeWidthStr::width(prompt)
}

/// The text width inside the input area: `❯ ` on the first line for `lines`, a border plus the
/// `❯ ` prompt for `box`.
fn input_inner_width(app: &App, width: usize) -> usize {
    let reserved = match app.input_style {
        InputStyle::Lines => prompt_width(INPUT_PROMPT),
        // Two border cells plus the prompt.
        InputStyle::Box => 2 + prompt_width(BOX_PROMPT),
    };
    width.saturating_sub(reserved).max(1)
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

/// The height the approval box needs: the wrapped body rows (bounded by `cap`), the key line
/// and the border. The body scrolls rather than growing past `cap`.
fn approval_height(request: &ApprovalRequest, width: usize, cap: usize) -> u16 {
    let rows = wrap(&approval_body(request), width.saturating_sub(2).max(1)).len();
    let visible = rows.min(cap.saturating_sub(3).max(1));
    (visible as u16 + 3).clamp(
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
    let mut run: Option<(String, bool)> = None;
    let flush = |out: &mut Vec<Span<'static>>, run: &mut Option<(String, bool)>, base: Style| {
        if let Some((text, run_selected)) = run.take() {
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
        flush(&mut out, &mut run, base);
        for ch in span.content.into_owned().chars() {
            let w = ch.width().unwrap_or(0);
            let selected = ranges
                .iter()
                .any(|&(start, end)| col < end && col + w.max(1) > start);
            match &mut run {
                Some((text, run_selected)) if *run_selected == selected => text.push(ch),
                _ => {
                    flush(&mut out, &mut run, base);
                    run = Some((ch.to_string(), selected));
                }
            }
            col += w;
        }
    }
    flush(&mut out, &mut run, Style::default());
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
    if let Some(next) = &app.next_model {
        return vec![Segment {
            text: format!("next: {next}"),
            style: app.theme.turn_running,
        }];
    }
    if let Some(next) = &app.next_thinking {
        return vec![Segment {
            text: format!("next: thinking {next}"),
            style: app.theme.turn_running,
        }];
    }
    // The session facts come from the projection, not from `App` (ADR 0023 U5). One read, so
    // the whole status bar is one instant.
    let session = app.projection.read();
    let turn = session.turn();
    let turn_style = match turn {
        TurnState::Idle => app.theme.turn_idle,
        TurnState::Running => app.theme.turn_running,
        TurnState::Cancelled => app.theme.turn_cancelled,
    };
    let position = if app.max_scroll == 0 {
        "all".to_string()
    } else {
        format!("{}/{}", app.scroll, app.max_scroll)
    };
    let mut segments = vec![
        Segment {
            text: format!(" {} ", app.model_name),
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
    if app.auto {
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
    if let Some(note) = &app.note {
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
            if app.follow { "follow" } else { "paused" }
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
    let completion = completion_rows(app, area.width as usize);
    // `[tui.widgets]` (ADR 0027 C3): one line above and/or below the input box.
    let above_widgets = crate::footer::widget_line(app, &app.widgets.above_input);
    let below_widgets = crate::footer::widget_line(app, &app.widgets.below_input);
    let widget_rows = u16::from(above_widgets.is_some()) + u16::from(below_widgets.is_some());
    let middle = match app.top_modal() {
        Some(Modal::Approval(prompt)) => approval_height(
            &prompt.request,
            area.width as usize,
            approval_cap(area.height as usize, completion.len()),
        ),
        Some(Modal::Question(form)) => question_height(form, area.width as usize),
        Some(Modal::Login(_)) => LOGIN_HEIGHT,
        // The `/logout` confirmation is a two-line box like the `/memory` one.
        Some(Modal::ConfirmLogout(_)) => 4,
        // The trust prompt lists up to five options plus a header and a key line.
        Some(Modal::Trust(wizard)) => wizard.choices.len() as u16 + 3,
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
    let (total, _) = visible_lines(&mut app.entries, modes, width, 0, 0, &theme);
    radian_session::trace::took("draw/measure", t_draw);
    app.set_viewport(chunks[0].height, total);
    let t_window = std::time::Instant::now();
    let (_, visible) = visible_lines(&mut app.entries, modes, width, app.scroll, height, &theme);
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
    let visible = match app.selection.filter(|sel| !sel.is_empty()) {
        Some(selection) => {
            let ranges = crate::select::highlight_rows(
                &app.entries,
                modes,
                width,
                app.scroll,
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
        crate::picker::draw(frame, chunks[0], picker);
    }

    if !completion.is_empty() {
        // The highlighted row: the menu's cursor, styled with the theme's
        // `selection` (reversed in `mono`), so it is clear which candidate Enter/Tab accepts.
        let lines: Vec<Line> = completion
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let style = if index == app.completion_cursor {
                    theme.selection
                } else {
                    theme.completion
                };
                Line::from(Span::styled(row, style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), chunks[1]);
    }

    match app.top_modal() {
        Some(Modal::Approval(prompt)) => {
            // The body scrolls inside the box; the key line is the box's own last row, so a
            // long command can never push the answers off screen.
            let inner_width = (input_area.width as usize).saturating_sub(2).max(1);
            let body = wrap(&approval_body(&prompt.request), inner_width);
            let total = body.len();
            let visible = (middle as usize).saturating_sub(3).max(1);
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
            let lines: Vec<Line> = body[first..last]
                .iter()
                .map(|row| Line::from(row.clone()))
                .collect();
            let mut block = Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.approval_border)
                .title(" approve tool call? ");
            if total > visible {
                // The window indicator goes in the top-right corner: the bottom row belongs to
                // the keys.
                block = block.title_top(
                    Line::from(format!(" lines {}–{} of {total} ", first + 1, last))
                        .right_aligned(),
                );
            }
            frame.render_widget(&block, input_area);
            let inner = block.inner(input_area);
            let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
            frame.render_widget(Paragraph::new(lines), rows[0]);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    APPROVAL_KEYS,
                    app.theme.approval_border,
                ))),
                rows[1],
            );
        }
        Some(Modal::Question(form)) => {
            // ADR 0024 Q7: the title names the source and the position; the box scrolls its
            // option list rather than growing.
            let position = match form.current {
                Some(index) => format!("{}/{}", index + 1, form.len()),
                None => "overview".to_string(),
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.approval_border)
                .title(format!(" question {position} from {} ", form.origin));
            let inner = input_area.height.saturating_sub(2) as usize;
            let width = input_area.width.saturating_sub(2) as usize;
            // The key line is the box's own last row, so a long option list can never push it
            // out (same fix as the approval box).
            let lines = question_lines(form, width, inner.saturating_sub(1));
            frame.render_widget(&block, input_area);
            let inner_area = block.inner(input_area);
            let rows =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner_area);
            frame.render_widget(Paragraph::new(lines), rows[0]);
            frame.render_widget(
                Paragraph::new(Line::from(clip_columns(QUESTION_KEYS, width))),
                rows[1],
            );
        }
        Some(Modal::Login(wizard)) => {
            let lines = login_lines(wizard, &app.theme, input_area.width as usize);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.approval_border)
                .title(" /login ");
            frame.render_widget(&block, input_area);
            let inner = block.inner(input_area);
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Some(Modal::Trust(wizard)) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.approval_border)
                .title(" project trust ");
            frame.render_widget(&block, chunks[2]);
            let inner = block.inner(chunks[2]);
            let mut lines = vec![Line::from(format!(
                "project resources found in {}",
                wizard.workspace.display()
            ))];
            for (index, choice) in wizard.choices.iter().enumerate() {
                let marker = if index == wizard.cursor { "> " } else { "  " };
                let style = if index == wizard.cursor {
                    app.theme.approval_border
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}{}", wizard.label(*choice)),
                    style,
                )));
            }
            lines.push(Line::from(Span::styled(
                "Up/Down choose   Enter select   Esc = do not trust",
                app.theme.hint,
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Some(Modal::ConfirmLogout(provider)) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.approval_border)
                .title(" /logout ");
            frame.render_widget(&block, input_area);
            let inner = block.inner(input_area);
            let lines = vec![
                Line::from(format!("remove the stored credential for `{provider}`?")),
                Line::from(Span::styled(
                    "[y] remove   [n/Esc] keep",
                    app.theme.approval_border,
                )),
            ];
            frame.render_widget(Paragraph::new(lines), inner);
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
            // `> ` on the first line. `input_height` already reserves the two rule rows, so the
            // layout of everything else is unchanged.
            let inner_width = input_inner_width(app, input_area.width as usize);
            let visible = input_area.height.saturating_sub(2).max(1) as usize;
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
            // Both styles reserve the same two rows (rules / borders), so the rest of the
            // layout does not depend on the choice.
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
                    let block = Block::default()
                        .borders(Borders::TOP | Borders::BOTTOM)
                        .border_style(app.theme.hint);
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
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .border_style(accent)
                        .title(" message ");
                    frame.render_widget(Paragraph::new(body).block(block), input_area);
                }
            }
            if app.input_editable() {
                // The text starts after the prompt (`> `, two cells) or after the box border
                // (one cell); a full row clamps the cursor to its last cell rather than onto the
                // rule or the border.
                let lead = match app.input_style {
                    InputStyle::Lines => prompt_width(INPUT_PROMPT) as u16,
                    // Border (1) + the `❯ ` prompt (2 cells).
                    InputStyle::Box => 1 + prompt_width(BOX_PROMPT) as u16,
                };
                let col = cursor_col.min(inner_width.saturating_sub(1)) as u16;
                let row = (cursor_row - offset).min(visible.saturating_sub(1)) as u16;
                let x = input_area.x + lead + col;
                let y = input_area.y + 1 + row;
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
    if let Some(next) = &app.next_model {
        segments.push(Segment {
            text: format!("next: {next}"),
            style: app.theme.turn_running,
        });
    }
    if let Some(next) = &app.next_thinking {
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
    if app.auto {
        segments.push(Segment {
            text: "AUTO".to_string(),
            style: app.theme.auto,
        });
    }
    if let Some(note) = &app.note {
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
fn question_height(form: &QuestionForm, width: usize) -> u16 {
    let rows = question_lines(form, width, usize::MAX).len() + 1;
    (rows as u16 + 2).clamp(5, MAX_QUESTION_ROWS)
}

/// The question form's content lines, scrolled so the cursor stays visible (ADR 0024 Q7).
///
/// Pure: the layout is testable without a terminal. `max_rows` is the visible budget (the box's
/// inner height **minus the key line**, which `draw` renders separately so it is always
/// visible); `usize::MAX` means "no clipping", which is how the height is measured.
pub fn question_lines(form: &QuestionForm, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    match form.current_question() {
        // The overview: one line per question, with the answer or `(not answered)`.
        None => {
            for (index, question) in form.questions.iter().enumerate() {
                let answer = &form.answers[index];
                let shown = if QuestionForm::is_answered(question, answer) {
                    answer_text(question, answer)
                } else {
                    "(not answered)".to_string()
                };
                out.push(Line::from(clip_columns(
                    &format!("{}. {} → {}", index + 1, question.prompt, shown),
                    width,
                )));
            }
        }
        Some(question) => {
            out.push(Line::from(clip_columns(&question.prompt, width)));
            let answer = form
                .current_answer()
                .expect("a current question has an answer");
            let rows = QuestionForm::row_count(question);
            // The window over the option rows: the prompt, the two clip markers and the decline
            // note each take a row when present.
            let visible = max_rows.saturating_sub(4).max(1);
            let first = if rows <= visible {
                0
            } else {
                (answer.cursor + 1)
                    .saturating_sub(visible)
                    .min(rows.saturating_sub(visible))
            };
            let last = (first + visible).min(rows);
            // A marker above, so a clipped list never looks complete.
            if first > 0 {
                out.push(Line::from(format!("  … {first} above")));
            }
            for row in first..last {
                out.push(Line::from(clip_columns(
                    &row_text(form, question, row),
                    width,
                )));
            }
            let hidden = rows.saturating_sub(last);
            if hidden > 0 {
                out.push(Line::from(format!("  … {hidden} more")));
            }
        }
    }
    if form.confirm_decline {
        out.push(Line::from(clip_columns(
            "press Esc again to decline this question (Enter keeps it)",
            width,
        )));
    }
    if out.len() > max_rows && max_rows != usize::MAX {
        out.truncate(max_rows);
    }
    out
}

/// One option row of the current question: the cursor, the mark and the label. The last row is
/// the `Other` row when the question allows one.
fn row_text(form: &QuestionForm, question: &radian_coding::Question, row: usize) -> String {
    let answer = form
        .current_answer()
        .expect("a current question has an answer");
    let cursor = if row == answer.cursor { ">" } else { " " };
    match question.options.get(row) {
        Some(option) => {
            let mark = if answer.selected.contains(&option.id) {
                match question.kind {
                    radian_coding::QuestionKind::Multi => "[x]",
                    _ => "(•)",
                }
            } else {
                match question.kind {
                    radian_coding::QuestionKind::Multi => "[ ]",
                    _ => "( )",
                }
            };
            let description = option
                .description
                .as_deref()
                .map(|text| format!(" — {text}"))
                .unwrap_or_default();
            format!("{cursor} {mark} {}{description}", option.label)
        }
        // The `Other` row: its text is the composer's, which may be empty.
        None => {
            let mark = if answer.other { "(•)" } else { "( )" };
            let text = answer.text.text();
            format!("{cursor} {mark} Other: {text}")
        }
    }
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

/// The most candidate lines shown before a `+N more` line.
pub const COMPLETION_MAX_LINES: usize = 10;

/// The `@` completion list's lines: the candidates (at most [`COMPLETION_MAX_LINES`]), then a
/// `+N more` line when there are more. Each row is cut to `width` display columns, so a narrow
/// terminal clips instead of wrapping. Empty when no list is up, or while an approval prompt
/// is shown: the list never covers the prompt.
pub fn completion_rows(app: &App, width: usize) -> Vec<String> {
    if app.has_modal() {
        return Vec::new();
    }
    let Some(candidates) = app.completion() else {
        return Vec::new();
    };
    let mut rows: Vec<String> = candidates
        .iter()
        .take(COMPLETION_MAX_LINES)
        .map(|candidate| truncate_columns(candidate, width))
        .collect();
    if candidates.len() > COMPLETION_MAX_LINES {
        rows.push(truncate_columns(
            &format!("+{} more", candidates.len() - COMPLETION_MAX_LINES),
            width,
        ));
    }
    rows
}

/// Cuts `text` to at most `width` display columns, never splitting a wide character.
fn truncate_columns(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > width {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out
}

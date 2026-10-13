//! Rendering: the transcript view with its own scrolling, the input box, and the status
//! line. Pure given an `App`, so `TestBackend` can assert on the result.

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use radian_permissions::ApprovalRequest;

use crate::app::{
    App, Entry, EntryKind, TurnState, WrapKey, budget_status_text, mcp_text, usage_text,
};

/// Reasoning is dim and italic; it must be visually distinct from assistant text (ADR 0014).
pub fn reasoning_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC | Modifier::DIM)
}

/// The marker on an interrupted assistant prefix (ADR 0003).
pub fn interrupted_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Hard-wraps `text` to `width` display columns, breaking on spaces where possible and on
/// character boundaries otherwise (CJK has no spaces). Newlines start a new line.
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
            let ch_width = ch.width().unwrap_or(0);
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

fn marker(kind: EntryKind) -> (&'static str, Style) {
    match kind {
        EntryKind::User => (
            "> ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        EntryKind::Assistant => ("  ", Style::default()),
        EntryKind::Reasoning => ("~ ", reasoning_style()),
        EntryKind::ToolCall => ("-> ", Style::default().fg(Color::Blue)),
        EntryKind::ToolResult => ("<- ", Style::default().fg(Color::Green)),
        EntryKind::Notice => ("* ", Style::default().fg(Color::DarkGray)),
    }
}

fn body_style(entry: &Entry) -> Style {
    match entry.kind {
        EntryKind::Reasoning => reasoning_style(),
        EntryKind::ToolCall => Style::default().fg(Color::Blue),
        EntryKind::ToolResult if entry.is_error => Style::default().fg(Color::Red),
        EntryKind::ToolResult => Style::default().fg(Color::Green),
        EntryKind::Notice => Style::default().fg(Color::DarkGray),
        EntryKind::User | EntryKind::Assistant => {
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
/// the message (and the journal) keep the full text.
fn display_text(entry: &Entry) -> Cow<'_, str> {
    match entry.kind {
        EntryKind::User => crate::mention::fold_files(&entry.text),
        _ => Cow::Borrowed(&entry.text),
    }
}

/// The wrapped display lines of `entry`'s body (the prefix and the interrupted marker are not
/// included), at `width`. The renderer wraps exactly these, and [`crate::find`] matches
/// against exactly these, so a folded `@` attachment is shown and searched as its
/// `[file: path, N lines]` line, never as the hidden content.
pub fn display_lines(entry: &Entry, width: usize) -> Vec<String> {
    let (prefix, _) = marker(entry.kind);
    let body_width = width.saturating_sub(prefix.width()).max(1);
    wrap(&display_text(entry), body_width)
}

/// One entry's styled lines. An interrupted assistant entry gets an explicit marker on its
/// last line.
fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let (prefix, prefix_style) = marker(entry.kind);
    let wrapped = display_lines(entry, width);
    let style = body_style(entry);
    let last = wrapped.len().saturating_sub(1);
    let mut lines = Vec::with_capacity(wrapped.len());
    for (i, row) in wrapped.into_iter().enumerate() {
        let lead = if i == 0 {
            prefix.to_string()
        } else {
            " ".repeat(prefix.width())
        };
        let mut spans = vec![Span::styled(lead, prefix_style), Span::styled(row, style)];
        if entry.interrupted && i == last {
            spans.push(Span::styled(" [interrupted]", interrupted_style()));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// How many lines `entry` wraps to at `width`, from its cache when the entry has not changed
/// since it was last measured.
fn entry_line_count(entry: &mut Entry, width: usize) -> usize {
    let text_len = entry.text.len();
    if let Some(lines) = entry.wrap_cache.get(width, text_len, entry.interrupted) {
        return lines;
    }
    let lines = display_lines(entry, width).len();
    entry.wrap_cache.set(WrapKey {
        width,
        text_len,
        interrupted: entry.interrupted,
        lines,
    });
    lines
}

/// Wraps the whole transcript into styled lines. The frame does not use this (it wraps only
/// the visible entries, see [`visible_lines`]); it is the reference the virtualized path is
/// tested against.
pub fn transcript_lines(entries: &[Entry], width: usize) -> Vec<Line<'static>> {
    entries
        .iter()
        .flat_map(|entry| entry_lines(entry, width))
        .collect()
}

/// The transcript's total line count and the `height` lines starting at line `start`,
/// wrapping only the entries that overlap that window (dogfooding D9). Every other entry
/// contributes its cached line count, so a long session costs an integer sum per frame
/// instead of re-wrapping every entry.
pub fn visible_lines(
    entries: &mut [Entry],
    width: usize,
    start: usize,
    height: usize,
) -> (usize, Vec<Line<'static>>) {
    let counts: Vec<usize> = entries
        .iter_mut()
        .map(|entry| entry_line_count(entry, width))
        .collect();
    let total: usize = counts.iter().sum();
    let mut out = Vec::with_capacity(height);
    let mut line = 0usize;
    for (entry, count) in entries.iter().zip(&counts) {
        if out.len() >= height {
            break;
        }
        let next = line + count;
        if next > start {
            let skip = start.saturating_sub(line);
            out.extend(
                entry_lines(entry, width)
                    .into_iter()
                    .skip(skip)
                    .take(height - out.len()),
            );
        }
        line = next;
    }
    (total, out)
}

/// The tallest the input box grows before it scrolls internally instead.
const MAX_INPUT_ROWS: usize = 10;

fn input_height(app: &App, width: usize) -> u16 {
    let inner = width.saturating_sub(2).max(1);
    let rows = crate::composer::rows(&app.input.text(), inner).len();
    (rows.clamp(1, MAX_INPUT_ROWS) as u16) + 2
}

/// The approval prompt's text. The **resolved** resource is what is shown: `a/../secret`
/// must not read as if it stayed under `a/` (the permission layer resolves it before the
/// prompt, and that resolved form is also the session-grant key).
pub fn approval_text(request: &ApprovalRequest) -> String {
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
    let args = serde_json::to_string_pretty(&request.arguments)
        .unwrap_or_else(|_| request.arguments.to_string());
    let (shown, discarded) = truncate_for_display(&args, 400);
    text.push_str(&format!("args: {shown}"));
    if discarded > 0 {
        text.push_str(&format!("\n      … {discarded} more bytes"));
    }
    if let Some(reason) = &request.reason {
        text.push_str(&format!("\nwhy: {reason}"));
    }
    text.push_str("\n[y] allow once   [a] always this session   [n] deny");
    text
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

/// The height the approval box needs, counting wrapped rows.
fn approval_height(request: &ApprovalRequest, width: usize) -> u16 {
    let rows = wrap(&approval_text(request), width.saturating_sub(2).max(1)).len();
    (rows as u16 + 2).clamp(5, 14)
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

/// One status-bar segment: its text and its style.
struct Segment {
    text: String,
    style: Style,
}

/// The status-bar segments, most important first. The order is the truncation order: a narrow
/// terminal drops the rightmost segments first, so the model, the turn state, the permission
/// tier, the usage and the MCP state survive and only the long context/scroll text can go.
fn status_segments(app: &App) -> Vec<Segment> {
    let turn_style = match app.turn {
        TurnState::Idle => Style::default().fg(Color::Green),
        TurnState::Running => Style::default().fg(Color::Yellow),
        TurnState::Cancelled => Style::default().fg(Color::Red),
    };
    let position = if app.max_scroll == 0 {
        "all".to_string()
    } else {
        format!("{}/{}", app.scroll, app.max_scroll)
    };
    let mut segments = vec![
        Segment {
            text: format!(" {} ", app.model_name),
            style: Style::default().fg(Color::Cyan),
        },
        Segment {
            text: app.turn.label().to_string(),
            style: turn_style,
        },
    ];
    // Phase 5: the find match position is high priority, so a narrow terminal keeps it.
    if let Some(status) = app.find_state().and_then(crate::find::FindState::status) {
        segments.push(Segment {
            text: status,
            style: Style::default().fg(Color::Yellow),
        });
    }
    if let Some(tier) = &app.tier {
        segments.push(Segment {
            text: tier.clone(),
            style: Style::default().fg(Color::Blue),
        });
    }
    segments.push(Segment {
        text: usage_text(app.usage_input, app.usage_output),
        style: Style::default().fg(Color::Green),
    });
    segments.push(Segment {
        text: mcp_text(app.mcp_configured, app.mcp_open),
        style: Style::default().fg(Color::Cyan),
    });
    // ADR 0018: over the input budget `B` the request is still sent, but the status bar
    // warns (the existing warning colour).
    let over_budget = app
        .budget_report
        .as_ref()
        .is_some_and(agent_core::over_budget);
    segments.push(Segment {
        text: budget_status_text(app.context_window, app.budget_report.as_ref()),
        style: Style::default().fg(if over_budget {
            Color::Yellow
        } else {
            Color::Magenta
        }),
    });
    segments.push(Segment {
        text: format!(
            "scroll {position} {}",
            if app.follow { "follow" } else { "paused" }
        ),
        style: Style::default(),
    });
    if let Some(note) = &app.note {
        segments.push(Segment {
            text: note.clone(),
            style: Style::default().fg(Color::Yellow),
        });
    }
    segments
}

/// Fits the segments into `width` display columns. Segments that do not fit are dropped from
/// the right (the least important) and a `…` marks the truncation, so the line never wraps
/// and never pushes the model off the screen. The last segment is never dropped, so a very
/// narrow terminal still shows something.
fn fit_segments(segments: Vec<Segment>, width: usize) -> Line<'static> {
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
        spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn status_line(app: &App, width: usize) -> Line<'static> {
    fit_segments(status_segments(app), width)
}

/// Draws one frame. Also feeds the measured transcript size back into `app` so scroll keys
/// operate on wrapped lines.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let middle = match &app.pending_approval {
        Some(prompt) => approval_height(&prompt.request, area.width as usize),
        None => input_height(app, area.width as usize),
    };
    // The `@` completion list sits just above the input box (and never over an approval
    // prompt). A zero-height chunk keeps the rest of the layout identical when there is none.
    let completion = completion_rows(app, area.width as usize);
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(completion.len() as u16),
        Constraint::Length(middle),
        Constraint::Length(1),
    ])
    .split(area);

    // Measure first (cached per entry), so the scroll position is clamped against the real
    // total before the visible window is cut.
    let width = chunks[0].width as usize;
    let height = chunks[0].height as usize;
    // The find matcher wraps at the same width; a resize recomputes it (once, not per frame).
    app.set_transcript_width(width);
    let (total, _) = visible_lines(&mut app.entries, width, 0, 0);
    app.set_viewport(chunks[0].height, total);
    let (_, visible) = visible_lines(&mut app.entries, width, app.scroll, height);
    let visible = match app.find_state() {
        Some(find) => visible
            .into_iter()
            .map(|line| highlight(line, &find.query))
            .collect(),
        None => visible,
    };
    frame.render_widget(Paragraph::new(visible), chunks[0]);

    if !completion.is_empty() {
        let style = Style::default().fg(Color::DarkGray);
        let lines: Vec<Line> = completion
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style)))
            .collect();
        frame.render_widget(Paragraph::new(lines), chunks[1]);
    }

    match &app.pending_approval {
        Some(prompt) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" approve tool call? ");
            let text = Paragraph::new(approval_text(&prompt.request))
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(text, chunks[2]);
        }
        None if app.find.is_some() => {
            // The find box replaces the input box's line; the input text itself is untouched
            // and comes back when the box closes.
            let find = app.find.as_ref().expect("checked");
            let text = format!("find: {}", find.query);
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" find "))
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, chunks[2]);
        }
        None => {
            let inner_width = chunks[2].width.saturating_sub(2).max(1) as usize;
            let visible = chunks[2].height.saturating_sub(2).max(1) as usize;
            let text = app.input.text();
            let rows = crate::composer::rows(&text, inner_width);
            let (cursor_row, cursor_col) = app.input.cursor_cell(inner_width);
            // Keep the cursor in view when the text is taller than the box.
            let offset = cursor_row.saturating_sub(visible.saturating_sub(1));
            let offset = offset.min(rows.len().saturating_sub(1));
            let empty = app.input.is_empty();
            let shown = if empty {
                "(type a message, Enter to send, Ctrl-J for a new line)".to_string()
            } else {
                rows[offset..].join("\n")
            };
            let style = if empty {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let input = Paragraph::new(Span::styled(shown, style))
                .block(Block::default().borders(Borders::ALL).title(" message "))
                .wrap(Wrap { trim: false });
            frame.render_widget(input, chunks[2]);
            if app.input_editable() {
                // The inner area starts one cell in from the border; a full row clamps the
                // cursor to its last cell rather than onto the border.
                let col = cursor_col.min(inner_width.saturating_sub(1)) as u16;
                let x = chunks[2].x + 1 + col;
                let y = chunks[2].y + 1 + (cursor_row - offset) as u16;
                frame.set_cursor_position((x, y));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(status_line(app, chunks[3].width as usize)),
        chunks[3],
    );
}

/// The most candidate lines shown before a `+N more` line.
pub const COMPLETION_MAX_LINES: usize = 10;

/// The `@` completion list's lines: the candidates (at most [`COMPLETION_MAX_LINES`]), then a
/// `+N more` line when there are more. Each row is cut to `width` display columns, so a narrow
/// terminal clips instead of wrapping. Empty when no list is up, or while an approval prompt
/// is shown: the list never covers the prompt.
pub fn completion_rows(app: &App, width: usize) -> Vec<String> {
    if app.pending_approval.is_some() {
        return Vec::new();
    }
    let Some(candidates) = &app.completion else {
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

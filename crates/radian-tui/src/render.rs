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
    App, Entry, EntryKind, Modal, TurnState, WrapKey, budget_status_text, mcp_text, usage_text,
};
use crate::question::QuestionForm;
use crate::theme::Theme;

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

/// The visible marker for an entry kind (the transcript's `> `, `~ `, …).
fn marker_prefix(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::User => "> ",
        EntryKind::Assistant => "  ",
        EntryKind::Reasoning => "~ ",
        EntryKind::ToolCall => "-> ",
        EntryKind::ToolResult => "<- ",
        EntryKind::Notice => "* ",
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
    let prefix = marker_prefix(entry.kind);
    let body_width = width.saturating_sub(prefix.width()).max(1);
    wrap(&display_text(entry), body_width)
}

/// One entry's styled lines. An interrupted assistant entry gets an explicit marker on its
/// last line.
fn entry_lines(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (prefix, prefix_style) = marker(entry.kind, theme);
    let wrapped = display_lines(entry, width);
    let style = body_style(entry, theme);
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
            spans.push(Span::styled(" [interrupted]", theme.interrupted));
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
pub fn transcript_lines(entries: &[Entry], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    entries
        .iter()
        .flat_map(|entry| entry_lines(entry, width, theme))
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
    theme: &Theme,
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
                entry_lines(entry, width, theme)
                    .into_iter()
                    .skip(skip)
                    .take(height - out.len()),
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

/// The input prompt, in the style of pi: the first line starts with `> ` and the continuation
/// lines indent by the same two cells, so the text stays aligned. Two cells of the width go to
/// it, which is why `input_height` measures the text at `width - 2`.
const INPUT_PROMPT: &str = "> ";

/// The placeholder shown while the input is empty.
const INPUT_HINT: &str = "(type a message, Enter to send, Ctrl-J for a new line)";

/// The input area's height: one rule above, the text rows, one rule below.
fn input_height(app: &App, width: usize) -> u16 {
    let inner = width.saturating_sub(INPUT_PROMPT.len()).max(1);
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
    if let Some(tier) = session.tier() {
        segments.push(Segment {
            text: tier.to_owned(),
            style: app.theme.tier,
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
    if let Some(note) = &app.note {
        segments.push(Segment {
            text: note.clone(),
            style: app.theme.note,
        });
    }
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
    let middle = match app.top_modal() {
        Some(Modal::Approval(prompt)) => approval_height(&prompt.request, area.width as usize),
        Some(Modal::Question(form)) => question_height(form, area.width as usize),
        _ => input_height(app, area.width as usize),
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
    let theme = app.theme.clone();
    // The find matcher wraps at the same width; a resize recomputes it (once, not per frame).
    app.set_transcript_width(width);
    let (total, _) = visible_lines(&mut app.entries, width, 0, 0, &theme);
    app.set_viewport(chunks[0].height, total);
    let (_, visible) = visible_lines(&mut app.entries, width, app.scroll, height, &theme);
    let visible = match app.find_state() {
        Some(find) => visible
            .into_iter()
            .map(|line| highlight(line, &find.query))
            .collect(),
        None => visible,
    };
    frame.render_widget(Paragraph::new(visible), chunks[0]);

    // The picker overlay draws over the transcript; the input box and the status bar stay
    // visible below it (ADR 0023 U4).
    if let Some(picker) = app.picker() {
        crate::picker::draw(frame, chunks[0], picker);
    }

    if !completion.is_empty() {
        let style = theme.completion;
        let lines: Vec<Line> = completion
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style)))
            .collect();
        frame.render_widget(Paragraph::new(lines), chunks[1]);
    }

    match app.top_modal() {
        Some(Modal::Approval(prompt)) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.approval_border)
                .title(" approve tool call? ");
            let text = Paragraph::new(approval_text(&prompt.request))
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(text, chunks[2]);
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
            let inner = chunks[2].height.saturating_sub(2) as usize;
            let width = chunks[2].width.saturating_sub(2) as usize;
            let lines = question_lines(form, width, inner);
            frame.render_widget(Paragraph::new(lines).block(block), chunks[2]);
        }
        _ if app.find_state().is_some() => {
            // The find box replaces the input box's line; the input text itself is untouched
            // and comes back when the box closes.
            let find = app.find_state().expect("checked");
            let text = format!("find: {}", find.query);
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" find "))
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, chunks[2]);
        }
        // The `/memory` confirmation and the ordinary state draw the input area, exactly as
        // before (a confirmation only makes it non-editable).
        _ => {
            // pi-style: a rule above and below instead of a box, and
            // `> ` on the first line. `input_height` already reserves the two rule rows, so the
            // layout of everything else is unchanged.
            let inner_width = chunks[2]
                .width
                .saturating_sub(INPUT_PROMPT.len() as u16)
                .max(1) as usize;
            let visible = chunks[2].height.saturating_sub(2).max(1) as usize;
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
                                let lead = if index == 0 { INPUT_PROMPT } else { "  " };
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
                    frame.render_widget(Paragraph::new(body).block(block), chunks[2]);
                }
                InputStyle::Box => {
                    let shown = if empty {
                        INPUT_HINT.to_string()
                    } else {
                        rows[offset..].join("\n")
                    };
                    let input = Paragraph::new(Span::styled(shown, style))
                        .block(Block::default().borders(Borders::ALL).title(" message "))
                        .wrap(Wrap { trim: false });
                    frame.render_widget(input, chunks[2]);
                }
            }
            if app.input_editable() {
                // The text starts after the prompt (`> `, two cells) or after the box border
                // (one cell); a full row clamps the cursor to its last cell rather than onto the
                // rule or the border.
                let lead = match app.input_style {
                    InputStyle::Lines => INPUT_PROMPT.len() as u16,
                    InputStyle::Box => 1,
                };
                let col = cursor_col.min(inner_width.saturating_sub(1)) as u16;
                let row = (cursor_row - offset).min(visible.saturating_sub(1)) as u16;
                let x = chunks[2].x + lead + col;
                let y = chunks[2].y + 1 + row;
                frame.set_cursor_position((x, y));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(status_line(app, chunks[3].width as usize)),
        chunks[3],
    );
}

/// The most rows a question form's box may take, border included (ADR 0024 Q7: a long option
/// list scrolls instead of growing the box without bound).
const MAX_QUESTION_ROWS: u16 = 18;

/// The height the question form's box needs: its content, bounded.
fn question_height(form: &QuestionForm, width: usize) -> u16 {
    let rows = question_lines(form, width, usize::MAX).len();
    (rows as u16 + 2).clamp(5, MAX_QUESTION_ROWS)
}

/// The question form's content lines, scrolled so the cursor stays visible (ADR 0024 Q7).
///
/// Pure: the layout is testable without a terminal. `max_rows` is the visible budget (the
/// box's inner height); `usize::MAX` means "no clipping", which is how the height is measured.
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
            // The window over the option rows: the prompt and the hint are two rows.
            let visible = max_rows.saturating_sub(3).max(1);
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
    out.push(Line::from(clip_columns(
        "Tab next · Space select · Enter next/submit · Esc decline",
        width,
    )));
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

//! Rendering: the transcript view with its own scrolling, the input box, and the status
//! line. Pure given an `App`, so `TestBackend` can assert on the result.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use radian_permissions::ApprovalRequest;

use crate::app::{App, Entry, EntryKind, TurnState, budget_status_text};

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

/// Wraps the transcript into styled lines. One entry becomes one or more lines; an
/// interrupted assistant entry gets an explicit marker on its last line.
pub fn transcript_lines(entries: &[Entry], width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        let (prefix, prefix_style) = marker(entry.kind);
        let body_width = width.saturating_sub(prefix.width()).max(1);
        let wrapped = wrap(&entry.text, body_width);
        let style = body_style(entry);
        let last = wrapped.len().saturating_sub(1);
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
    }
    lines
}

fn input_height(app: &App, width: usize) -> u16 {
    let text = if app.input.is_empty() {
        String::new()
    } else {
        app.input.clone()
    };
    let rows = wrap(&text, width.saturating_sub(2).max(1)).len().max(1);
    (rows as u16 + 2).clamp(3, 8)
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

fn status_line(app: &App) -> Line<'static> {
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
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.model_name),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("│ "),
        Span::styled(app.turn.label(), turn_style),
        Span::raw(format!(
            " │ scroll {position} {} ",
            if app.follow { "follow" } else { "paused" }
        )),
    ];
    spans.push(Span::styled(
        format!(
            " │ {} ",
            budget_status_text(app.context_window, app.budget_report.as_ref())
        ),
        Style::default().fg(Color::Magenta),
    ));
    if let Some(note) = &app.note {
        spans.push(Span::styled(
            note.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

/// Draws one frame. Also feeds the measured transcript size back into `app` so scroll keys
/// operate on wrapped lines.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let middle = match &app.pending_approval {
        Some(prompt) => approval_height(&prompt.request, area.width as usize),
        None => input_height(app, area.width as usize),
    };
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(middle),
        Constraint::Length(1),
    ])
    .split(area);

    let lines = transcript_lines(&app.entries, chunks[0].width as usize);
    app.set_viewport(chunks[0].height, lines.len() as u16);
    let start = app.scroll as usize;
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(start)
        .take(chunks[0].height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), chunks[0]);

    match &app.pending_approval {
        Some(prompt) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" approve tool call? ");
            let text = Paragraph::new(approval_text(&prompt.request))
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(text, chunks[1]);
        }
        None => {
            let hint = if app.input.is_empty() {
                "(type a message, Enter to send, Ctrl-J for a new line)".to_string()
            } else {
                app.input.clone()
            };
            let style = if app.input.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let input = Paragraph::new(Span::styled(hint, style))
                .block(Block::default().borders(Borders::ALL).title(" message "))
                .wrap(Wrap { trim: false });
            frame.render_widget(input, chunks[1]);
        }
    }

    frame.render_widget(Paragraph::new(status_line(app)), chunks[2]);
}

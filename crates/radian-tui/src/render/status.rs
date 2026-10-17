//! The status bar at the bottom of the frame: the `classic` one-line segments (most important
//! first, dropped from the right when the terminal is narrow) and the `pi` footer rows plus the
//! chrome line every layout keeps.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, TurnState, budget_status_text, mcp_text, usage_text};
use crate::theme::Theme;

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

/// The status area's height: one row for the `classic` preset, the footer's rows for `pi`.
pub(super) fn status_height(app: &App, width: usize) -> u16 {
    status_lines(app, width).len().clamp(1, 4) as u16
}

/// The status area's lines: the `classic` one-liner, or the `pi` footer's rows plus the
/// **chrome** line when a mode is active (a queued switch, the find status, the armed quit
/// hint, the AUTO marker, a transient note). Those are not configurable items: they are states
/// the UI must show regardless of the footer layout (ADR 0026 D9, review).
pub(super) fn status_lines(app: &App, width: usize) -> Vec<Line<'static>> {
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

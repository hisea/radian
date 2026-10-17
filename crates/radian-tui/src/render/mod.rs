//! Rendering: the transcript view with its own scrolling, the input box, and the status
//! line. Pure given an `App`, so `TestBackend` can assert on the result.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    AccessChoice, AccessWizard, App, Modal, TrustWizard, TurnState, budget_status_text, mcp_text,
    usage_text,
};
use crate::blocks;
use crate::login::LoginWizard;
use crate::question::QuestionForm;
use crate::theme::Theme;

mod approval;
mod input;
mod transcript;
mod wrap;

use approval::approval_height;
use input::{
    BOX_PROMPT, BoxGlyphs, FIND_BOX_HEIGHT, INPUT_HINT, INPUT_PROMPT, input_height,
    input_inner_width, input_status_spans, input_visible_rows, prompt_width,
};
use transcript::apply_selection;
use wrap::clip_columns;
pub use approval::{
    APPROVAL_KEYS, MIN_APPROVAL_HEIGHT, approval_body, approval_cap, approval_text,
};
pub use input::{COMPLETION_MAX_LINES, InputStyle, completion_rows, completion_visible_rows};
pub use transcript::{
    display_lines, highlight, split_label, transcript_lines, transcript_lines_with, visible_lines,
    visible_lines_with,
};
pub(crate) use transcript::{display_text, marker_prefix, spinner_style};
pub use wrap::{WrappedLine, wrap, wrap_char_offsets, wrap_line_count};
pub(crate) use wrap::clip_display;

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
        LoginStep::OAuthBrowserWaiting { url } => {
            panel = panel.row(label("signing in with ChatGPT…"));
            match url {
                Some(url) => {
                    panel = panel
                        .row(label("opening your browser; if it did not open, visit:"))
                        .row(value(url.clone()));
                }
                None => panel = panel.row(label("starting the local sign-in listener…")),
            }
            panel = panel.hint("esc", "cancel");
        }
        LoginStep::OAuthDeviceWaiting {
            user_code,
            verification_url,
        } => {
            panel = panel.row(label("signing in with ChatGPT (device code)…"));
            if user_code.is_empty() {
                panel = panel.row(label("requesting a code…"));
            } else {
                panel = panel
                    .row(label(&format!("go to {verification_url} and enter:")))
                    .row(Line::from(Span::styled(user_code.clone(), theme.text)));
            }
            panel = panel.hint("esc", "cancel");
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
    // ADR 0030 O10: the task-agent count `running/total`, shown only when there is a task, so
    // an idle status bar does not carry a constant `0/0`.
    if let Some((running, total)) = app.session.task_counts() {
        segments.push(Segment {
            text: format!("tasks {running}/{total}"),
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
            // `> ` on the first line. `input_height` already reserves the style's two chrome
            // rows (the two rules, or the box's top rule and bottom tail), so the layout of
            // everything else is unchanged.
            let inner_width = input_inner_width(app, input_area.width as usize);
            let visible = input_visible_rows(input_area.height);
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
                    // ADR 0035 / the 2026-10-16 note: the thin half frame
                    // is the same shape as a code block's open frame. `notice_label` is the
                    // accent role the box already used (judged against ADR
                    // 0029 revision 1: the accent level is for labels and attention surfaces),
                    // so the frame and the `❯` prompt keep it; the body stays on the ordinary
                    // text level. (`model` is the model's own display color, not chrome.)
                    let frame_style = app.theme.notice_label;
                    let glyphs = BoxGlyphs::for_glyphs(app.footer_glyphs());
                    let area_width = input_area.width as usize;
                    // The top border: a full-width rule when idle, the running status where a
                    // code block's language label sits (`╭─ <status> ───…`, one space either
                    // side, dashes filling to the last column) while a turn runs.
                    let top: Line = match input_status_spans(app) {
                        None => Line::from(Span::styled(
                            format!(
                                "{}{}",
                                glyphs.open,
                                glyphs.rule.to_string().repeat(area_width.saturating_sub(1))
                            ),
                            frame_style,
                        )),
                        Some(status) => {
                            let used = 3
                                + status
                                    .iter()
                                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                                    .sum::<usize>()
                                + 1;
                            let mut spans = vec![
                                Span::styled(glyphs.open, frame_style),
                                Span::styled(format!("{} ", glyphs.rule), frame_style),
                            ];
                            spans.extend(status);
                            spans.push(Span::styled(" ", frame_style));
                            spans.push(Span::styled(
                                glyphs
                                    .rule
                                    .to_string()
                                    .repeat(area_width.saturating_sub(used)),
                                frame_style,
                            ));
                            Line::from(spans)
                        }
                    };
                    let mut body: Vec<Line> = vec![top];
                    let text_rows: Vec<String> = if empty {
                        vec![INPUT_HINT.to_string()]
                    } else {
                        rows[offset..].to_vec()
                    };
                    for (index, row) in text_rows.iter().enumerate() {
                        let lead = if index == 0 {
                            BOX_PROMPT.to_string()
                        } else {
                            " ".repeat(prompt_width(BOX_PROMPT))
                        };
                        body.push(Line::from(vec![
                            Span::styled(glyphs.rail, frame_style),
                            Span::styled(lead, frame_style),
                            Span::styled(row.clone(), style),
                        ]));
                    }
                    body.push(Line::from(Span::styled(glyphs.bottom, frame_style)));
                    frame.render_widget(Paragraph::new(body), input_area);
                }
            }
            if app.input_editable() {
                // The text starts after the `❯ ` prompt (`lines`), or after the `│ ` body rail
                // plus the `❯ ` prompt (`box`); a full row clamps the cursor to its last cell
                // rather than onto the rule or rail.
                let lead = match app.input_style {
                    InputStyle::Lines => prompt_width(INPUT_PROMPT) as u16,
                    // The body rail (`│ `, two cells) + the `❯ ` prompt (two cells).
                    InputStyle::Box => 2 + prompt_width(BOX_PROMPT) as u16,
                };
                let col = cursor_col.min(inner_width.saturating_sub(1)) as u16;
                let row = (cursor_row - offset).min(visible.saturating_sub(1)) as u16;
                // Both styles draw one chrome row above the text (the top rule, or the box's
                // top rule with the running status in it).
                let top_chrome: u16 = 1;
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

/// The `Other` row: `Other (type your own)` before it is chosen, `Other: <text>` once it is. A
/// focused, chosen row draws a visible inline cursor (`Space` on `Other`
/// starts typing at once), at the composer's cursor so `←`/`→` move it.
fn other_line(
    question: &radian_coding::Question,
    answer: &crate::question::FormAnswer,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Line<'static> {
    let focused = answer.cursor == question.options.len();
    let cursor = if focused { "❯ " } else { "  " };
    // ADR 0024 implementation note: `Other` uses the **same** checkbox pair as the
    // options (it used to always draw a circle, even in a multi question), from `AskGlyphs`.
    let g = blocks::AskGlyphs::for_glyphs(glyphs);
    let mark = if answer.other {
        g.single_on
    } else {
        g.single_off
    };
    // The marker and the cursor are never bold, like `option_line`.
    let mut spans = vec![
        Span::styled(cursor, theme.notice_label.remove_modifier(Modifier::BOLD)),
        Span::styled(mark, theme.notice_body),
    ];
    if answer.other && focused {
        let text = answer.text.text();
        let at = answer.text.cursor();
        let before: String = text.chars().take(at).collect();
        let after: String = text.chars().skip(at).collect();
        spans.push(Span::styled(" Other: ".to_string(), theme.notice_body));
        spans.push(Span::styled(before, theme.notice_body));
        spans.push(Span::styled("▏", theme.hint));
        spans.push(Span::styled(after, theme.notice_body));
    } else {
        let text = if answer.other && !answer.text.is_empty() {
            format!("Other: {}", answer.text.text())
        } else {
            "Other (type your own)".to_string()
        };
        spans.push(Span::styled(format!(" {text}"), theme.notice_body));
    }
    Line::from(spans)
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

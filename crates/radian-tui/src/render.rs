//! Rendering: the frame and the modules under it. [`draw`] composes the transcript, the input
//! area, the approval prompt, the status bar and the modal panels, each of which lives in its
//! own submodule and is re-exported here, so every `render::X` path is unchanged. Pure given an
//! `App`, so `TestBackend` can assert on the result.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Modal};

mod approval;
mod input;
mod panels;
mod status;
mod transcript;
mod wrap;

use approval::approval_height;
pub use approval::{
    APPROVAL_KEYS, MIN_APPROVAL_HEIGHT, approval_body, approval_cap, approval_text,
};
use input::{
    BOX_PROMPT, BoxGlyphs, FIND_BOX_HEIGHT, INPUT_HINT, INPUT_HINT_NO_MODEL, INPUT_PROMPT,
    input_height, input_inner_width, input_status_spans, input_visible_rows, prompt_width,
};
pub use input::{COMPLETION_MAX_LINES, InputStyle, completion_rows, completion_visible_rows};
pub use panels::{
    QUESTION_KEYS, access_panel, login_panel, logout_panel, question_panel, trust_panel,
};
use panels::{access_height, login_height, question_height, trust_height};
use status::{status_height, status_lines};
use transcript::apply_selection;
pub use transcript::{
    display_lines, highlight, split_label, transcript_lines, transcript_lines_with, visible_lines,
    visible_lines_with,
};
pub(crate) use transcript::{display_text, marker_prefix, spinner_style};
pub(crate) use wrap::clip_display;
pub use wrap::{WrappedLine, wrap, wrap_char_offsets, wrap_line_count};

/// Draws one frame. Also feeds the measured transcript size back into `app` so scroll keys
/// operate on wrapped lines.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // ADR 0023 U4: the middle of the frame is drawn by the top modal (the approval box
    // replaces the input box), then the find overlay, then the ordinary input box.
    // ADR 0027: the input-adjacent rows live in the footer now (each
    // `[[tui.footer.rows]]` has a `position`); empty rows take no space.
    let above_lines = app.footer.above_input_lines(app, area.width as usize);
    let below_lines = app.footer.below_input_lines(app, area.width as usize);
    let widget_rows = (above_lines.len() + below_lines.len()) as u16;
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
    // The middle block: the lines above the input, the input/modal itself, the lines below.
    let middle_chunks = Layout::vertical([
        Constraint::Length(above_lines.len() as u16),
        Constraint::Min(1),
        Constraint::Length(below_lines.len() as u16),
    ])
    .split(chunks[2]);
    let input_area = middle_chunks[1];
    if !above_lines.is_empty() {
        let rows = Layout::vertical(vec![Constraint::Length(1); above_lines.len()])
            .split(middle_chunks[0]);
        for (line, area) in above_lines.iter().zip(rows.iter()) {
            frame.render_widget(Paragraph::new(line.clone()), *area);
        }
    }
    if !below_lines.is_empty() {
        let rows = Layout::vertical(vec![Constraint::Length(1); below_lines.len()])
            .split(middle_chunks[2]);
        for (line, area) in below_lines.iter().zip(rows.iter()) {
            frame.render_widget(Paragraph::new(line.clone()), *area);
        }
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
    // The inspector overlay draws over the transcript the same way: one tabbed panel,
    // never above a modal (the layer stack guarantees the tier).
    if let Some(inspector) = app.inspector() {
        crate::inspector::draw(frame, chunks[0], inspector, &app.theme, app.footer_glyphs());
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
                    // ADR 0008 §2: the empty box names `/model` while no model is routed.
                    let hint = if app.session.has_model {
                        INPUT_HINT
                    } else {
                        INPUT_HINT_NO_MODEL
                    };
                    let body: Vec<Line> = if empty {
                        vec![Line::from(vec![
                            Span::styled(INPUT_PROMPT, app.theme.hint),
                            Span::styled(hint, style),
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
                        vec![if app.session.has_model {
                            INPUT_HINT.to_string()
                        } else {
                            INPUT_HINT_NO_MODEL.to_string()
                        }]
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

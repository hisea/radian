//! The arc TUI's frame (ADR 0035): the header with the page tabs and the loop state, the
//! plans page, the task page and the key hints.
//!
//! The plans page is two columns: the plan list (fixed width) and the selected plan in an open
//! frame. The task page is one open frame; the conversation tab puts the attach renderer
//! inside it, the diff tab adds a second open frame for the selected file, and the attempts
//! tab lists each attempt's verdicts.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::*;
use crate::theme::Theme;

/// The header row and the blank row under it.
const HEADER_ROWS: u16 = 2;
/// The frame rule and the key-hint row at the bottom.
const FOOTER_ROWS: u16 = 2;

/// Draws one whole frame.
pub fn draw(frame: &mut Frame, app: &mut ArcApp) {
    let area = frame.area();
    if area.height <= HEADER_ROWS + FOOTER_ROWS || area.width < 20 {
        return;
    }
    let body = Rect {
        x: area.x,
        y: area.y + HEADER_ROWS,
        width: area.width,
        height: area.height - HEADER_ROWS - FOOTER_ROWS,
    };
    draw_line(
        frame,
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        header_line(app, area.width as usize),
    );
    match app.page {
        Page::Plans => draw_plans(frame, body, app),
        Page::Task => draw_task(frame, body, app),
        Page::Merge => {}
    }
    let rule_y = area.y + area.height - FOOTER_ROWS;
    draw_line(
        frame,
        Rect {
            x: area.x,
            y: rule_y,
            width: area.width,
            height: 1,
        },
        Line::from(Span::styled(
            "─".repeat(area.width as usize),
            app.theme.frame,
        )),
    );
    draw_line(
        frame,
        Rect {
            x: area.x,
            y: rule_y + 1,
            width: area.width,
            height: 1,
        },
        footer_line(app),
    );
}

fn draw_line(frame: &mut Frame, area: Rect, line: Line<'static>) {
    frame.render_widget(Paragraph::new(line), area);
}

// ---- header and footer --------------------------------------------------------------

fn header_line(app: &ArcApp, width: usize) -> Line<'static> {
    let theme = &app.theme;
    let mut left = vec![
        Span::styled("  ", theme.hint),
        Span::styled("arc", theme.model),
    ];
    for (index, page) in [Page::Plans, Page::Task, Page::Merge]
        .into_iter()
        .enumerate()
    {
        left.push(Span::styled(
            if index == 0 { "   " } else { "  ·  " },
            theme.hint,
        ));
        let style = if app.page == page {
            theme.notice_label
        } else {
            theme.hint
        };
        left.push(Span::styled(page.name(), style));
    }
    let mut right = vec![
        Span::styled("main ", theme.hint),
        Span::styled(
            app.main_sha.clone().unwrap_or_else(|| "—".to_owned()),
            theme.text,
        ),
        Span::styled("  ·  loop ", theme.hint),
    ];
    if app.loop_running {
        right.push(Span::styled("● ", theme.model));
        right.push(Span::styled("running", theme.text));
    } else {
        right.push(Span::styled("◻ ", theme.hint));
        right.push(Span::styled("stopped", theme.hint));
    }
    let left_width: usize = left.iter().map(span_width).sum();
    let right_width: usize = right.iter().map(span_width).sum();
    let gap = width.saturating_sub(left_width + right_width);
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

fn footer_line(app: &ArcApp) -> Line<'static> {
    let theme = &app.theme;
    let hints: &[(&str, &str)] = match app.page {
        Page::Plans => &[
            ("↑↓", "select"),
            ("enter", "open task"),
            ("g", "graph/list"),
            ("tab", "pages"),
            ("q", "quit"),
        ],
        Page::Task => &[
            ("j/k", "scroll"),
            ("f", "follow"),
            ("[ ]", "attempt"),
            ("n/p", "file"),
            ("h/l", "tab"),
            ("esc", "back"),
            ("q", "quit"),
        ],
        Page::Merge => &[("q", "quit")],
    };
    let mut spans = vec![Span::styled("  ", theme.hint)];
    for (index, (key, note)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", theme.hint));
        }
        spans.push(Span::styled((*key).to_owned(), theme.hint));
        spans.push(Span::styled(" ", theme.hint));
        spans.push(Span::styled((*note).to_owned(), theme.truncation));
    }
    Line::from(spans)
}

// ---- plans page ----------------------------------------------------------------------

fn draw_plans(frame: &mut Frame, area: Rect, app: &mut ArcApp) {
    let rows = app.plan_rows();
    let height = area.height as usize;
    // The `PLANS` heading and the blank line under it stay put; each plan takes three rows.
    let visible_plans = height.saturating_sub(2) / 3;
    app.plan_top = scroll_top(app.plan_top, app.plan_sel, visible_plans, rows.len());
    let left = plan_list_lines(app, &rows, app.plan_top);
    let right_width = area.width.saturating_sub(RIGHT_X) as usize;
    let right = match rows.get(app.plan_sel) {
        Some(row) => {
            let title = app
                .selected_plan()
                .map(|view| view.plan.item.title.clone())
                .unwrap_or_default();
            // The frame's own rows (top, blank, title, blank, bottom) leave this many rows
            // for its tasks; the list scrolls to keep the selected task visible.
            let visible_tasks = height.saturating_sub(5);
            let selected = app.task_sel.min(row.tasks.len().saturating_sub(1));
            app.task_top = scroll_top(app.task_top, selected, visible_tasks, row.tasks.len());
            plan_frame_lines(app, row, &title, right_width, app.task_top, visible_tasks)
        }
        None => {
            app.task_top = 0;
            vec![Line::from(Span::styled(
                app.notice
                    .clone()
                    .unwrap_or_else(|| "no plans yet".to_owned()),
                if app.notice.is_some() {
                    app.theme.status_error
                } else {
                    app.theme.hint
                },
            ))]
        }
    };
    for y in 0..height {
        let left_line = left.get(y).cloned().unwrap_or_default();
        let left_width = left_line.width();
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        spans.extend(left_line.spans);
        spans.push(Span::raw(
            " ".repeat(LEFT_WIDTH as usize - left_width.min(LEFT_WIDTH as usize)),
        ));
        spans.push(Span::styled("│", app.theme.frame));
        spans.push(Span::raw("  "));
        if let Some(right_line) = right.get(y) {
            spans.extend(right_line.spans.clone());
        }
        draw_line(
            frame,
            Rect {
                x: area.x,
                y: area.y + y as u16,
                width: area.width,
                height: 1,
            },
            Line::from(spans),
        );
    }
}

/// The first item to draw so that `selected` stays inside `visible` rows: a terminal list, which
/// scrolls only when the selection leaves the top or the bottom, and keeps its place otherwise.
fn scroll_top(top: usize, selected: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let mut top = top.min(len - visible);
    if selected < top {
        top = selected;
    } else if selected >= top + visible {
        top = (selected + 1 - visible).min(len - visible);
    }
    top
}

/// The left column: `PLANS`, then each plan from `top` in two rows (the id and its summary).
fn plan_list_lines(app: &ArcApp, rows: &[PlanRow], top: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut out = vec![Line::from(Span::styled("PLANS", theme.hint))];
    if rows.is_empty() {
        out.push(Line::default());
        out.push(Line::from(Span::styled("  none yet", theme.hint)));
        return out;
    }
    out.push(Line::default());
    for (index, row) in rows.iter().enumerate().skip(top) {
        let selected = index == app.plan_sel;
        let cursor = if selected {
            Span::styled("❯ ", theme.model)
        } else {
            Span::raw("  ")
        };
        out.push(Line::from(vec![
            cursor,
            Span::styled(format!("{} ", row.mark.glyph()), row.mark.style(theme)),
            Span::styled(fit(&row.id, LEFT_WIDTH as usize - 6), theme.text),
        ]));
        let prefix = match row.status.as_str() {
            "draft" => "draft · ",
            "cancelled" => "cancelled · ",
            _ => "",
        };
        let details = format!(
            "{prefix}auto {} · {}/{} · {}",
            row.auto,
            row.merged,
            row.total,
            format_cost(row.cost)
        );
        out.push(Line::from(Span::styled(
            format!("    {}", fit(&details, LEFT_WIDTH as usize - 5)),
            theme.tier,
        )));
        out.push(Line::default());
    }
    out
}

/// The right column: the selected plan's open frame, its tasks from `top`.
fn plan_frame_lines(
    app: &ArcApp,
    row: &PlanRow,
    title: &str,
    width: usize,
    top: usize,
    visible: usize,
) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let label = format!(
        "{} {} · auto {} · {}/{} merged · {}",
        row.id,
        row.status,
        row.auto,
        row.merged,
        row.total,
        format_cost(row.cost)
    );
    let mut out = vec![frame_top(width, &label, theme)];
    out.push(rail(theme, Line::default()));
    out.push(rail(
        theme,
        Line::from(Span::styled(
            fit(title, width.saturating_sub(3)),
            theme.text,
        )),
    ));
    out.push(rail(theme, Line::default()));
    let (id_width, phase_width) = task_columns(&row.tasks, width.saturating_sub(2));
    // The cursor is on the task the moves act on: the row at `task_sel` in this same display
    // order, and only while the task list has the focus.
    let selected = (app.focus == Focus::Tasks)
        .then(|| row.tasks.get(app.task_sel).map(|task| task.id.as_str()))
        .flatten();
    for task in row.tasks.iter().skip(top).take(visible) {
        let cursor = selected == Some(task.id.as_str());
        let line = task_row_line(task, id_width, phase_width, cursor, theme);
        out.push(rail(theme, line));
    }
    out.push(frame_bottom(theme));
    out
}

/// The id and `state.phase` column widths for one plan's rows, shrunk to fit.
fn task_columns(tasks: &[TaskRow], content: usize) -> (usize, usize) {
    let id_max = tasks.iter().map(|task| task.id.width()).max().unwrap_or(16);
    let phase_max = tasks
        .iter()
        .map(|task| task.phase.width())
        .max()
        .unwrap_or(12);
    let connector_max = tasks
        .iter()
        .map(|task| task.connector.width())
        .max()
        .unwrap_or(0);
    let mut phase = (phase_max + 2).clamp(10, 25);
    let mut id = (id_max + 2).clamp(12, 34);
    let pre = connector_max + 2;
    // connector + mark + space + id + space + phase + space + `#N` + space + cost. The
    // `waits`/`also after` tail takes whatever is left; a narrow page clips it rather than
    // shortening the id or the phase.
    let needed = |id: usize, phase: usize| pre + id + 1 + phase + 1 + 3 + 1 + 6;
    while needed(id, phase) > content && phase > 10 {
        phase -= 1;
    }
    while needed(id, phase) > content && id > 12 {
        id -= 1;
    }
    (id, phase)
}

/// One task row inside the plan frame: connector, mark, id, `state.phase`, `#attempt`, cost,
/// and `waits`/`also after` when they apply.
fn task_row_line(
    task: &TaskRow,
    id_width: usize,
    phase_width: usize,
    cursor: bool,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = Vec::new();
    if cursor {
        spans.push(Span::styled("❯ ", theme.model));
    }
    if !task.connector.is_empty() {
        spans.push(Span::styled(task.connector.clone(), theme.frame));
    }
    spans.push(Span::styled(
        format!("{} ", task.mark.glyph()),
        task.mark.style(theme),
    ));
    spans.push(Span::styled(
        fit(&task.id, id_width.saturating_sub(1)),
        theme.text,
    ));
    let id_used = task.id.width().min(id_width.saturating_sub(1));
    spans.push(Span::raw(" ".repeat(id_width.saturating_sub(id_used))));
    spans.push(Span::styled(
        fit(&task.phase, phase_width.saturating_sub(1)),
        task.mark.style(theme),
    ));
    let phase_used = task.phase.width().min(phase_width.saturating_sub(1));
    spans.push(Span::raw(
        " ".repeat(phase_width.saturating_sub(phase_used)),
    ));
    spans.push(Span::styled(format!("#{:<2}", task.attempt), theme.tier));
    spans.push(Span::styled(
        format!(" {}", format_cost(task.cost)),
        theme.tier,
    ));
    let extra = extra_text(task);
    if !extra.is_empty() {
        spans.push(Span::styled(format!("  {extra}"), theme.hint));
    }
    Line::from(spans)
}

fn extra_text(task: &TaskRow) -> String {
    let mut parts = Vec::new();
    if !task.waits.is_empty() {
        parts.push(format!("waits {}", task.waits.join(", ")));
    }
    if !task.also_after.is_empty() {
        parts.push(format!("also after {}", task.also_after.join(", ")));
    }
    if !task.after.is_empty() {
        parts.push(format!("after {}", task.after.join(", ")));
    }
    parts.join("  ")
}

// ---- task page -----------------------------------------------------------------------

fn draw_task(frame: &mut Frame, area: Rect, app: &mut ArcApp) {
    let Some(page) = &app.task else {
        return;
    };
    let theme = app.theme.clone();
    let width = area.width.saturating_sub(2);
    let label = format!(
        "{} {} · attempt {} · {} · plan {}",
        page.id,
        page.phase,
        page.attempt
            .map(|attempt| attempt.to_string())
            .unwrap_or_else(|| "—".to_owned()),
        format_cost(page.cost),
        page.plan
    );
    let tab = page.tab;
    let mut lines = vec![frame_top(width as usize, &label, &theme)];
    lines.push(rail(
        &theme,
        Line::from(Span::styled(
            fit(&page.title, width.saturating_sub(3) as usize),
            theme.text,
        )),
    ));
    lines.push(rail(
        &theme,
        tab_row(page, width.saturating_sub(2) as usize, &theme),
    ));
    lines.push(rail(&theme, Line::default()));
    match tab {
        TaskTab::Conversation => {
            draw_conversation(frame, area, app, lines);
            return;
        }
        TaskTab::Diff => draw_diff(
            &mut lines,
            page,
            width as usize,
            area.height.saturating_sub(4) as usize,
            &theme,
        ),
        TaskTab::Attempts => draw_attempts(
            &mut lines,
            page,
            width as usize,
            area.height as usize,
            &theme,
        ),
    }
    lines.push(frame_bottom(&theme));
    // The last row is always the frame's tail: a body longer than the page loses its last
    // rows, never the bottom line.
    let height = area.height as usize;
    if lines.len() > height {
        lines.truncate(height);
        if let Some(last) = lines.last_mut() {
            *last = frame_bottom(&theme);
        }
    }
    draw_lines(frame, area, lines);
}

/// The conversation tab: the frame's first rows, then the attach renderer's transcript, then
/// the frame's tail.
fn draw_conversation(frame: &mut Frame, area: Rect, app: &mut ArcApp, lines: Vec<Line<'static>>) {
    let theme = app.theme.clone();
    let height = area.height as usize;
    if height < 6 {
        return;
    }
    // The first rows of the frame, then the transcript occupies the rest above the tail.
    let first = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 4,
    };
    draw_lines(frame, first, lines);
    let transcript = Rect {
        x: area.x + 4,
        y: area.y + 4,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(5),
    };
    let page = app.task.as_mut().expect("the page exists");
    match &mut page.journal {
        Some(view) => view.draw(frame, transcript),
        None => {
            let text = page
                .journal_error
                .clone()
                .unwrap_or_else(|| "no session journal yet".to_owned());
            let style = if page.journal_error.is_some() {
                theme.status_error
            } else {
                theme.hint
            };
            draw_line(
                frame,
                Rect {
                    x: area.x,
                    y: area.y + 4,
                    width: area.width,
                    height: 1,
                },
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("│ ", theme.frame),
                    Span::styled(text, style),
                ]),
            );
        }
    }
    draw_line(
        frame,
        Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        },
        Line::from(vec![Span::raw("  "), Span::styled("╰──", theme.frame)]),
    );
}

/// The tab row: the three tabs on the left, the attempt selector on the right.
fn tab_row(page: &TaskPage, width: usize, theme: &Theme) -> Line<'static> {
    let mut left = Vec::new();
    for (index, tab) in [TaskTab::Conversation, TaskTab::Diff, TaskTab::Attempts]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            left.push(Span::styled("  ·  ", theme.hint));
        }
        let style = if page.tab == tab {
            theme.notice_label
        } else {
            theme.hint
        };
        left.push(Span::styled(tab.name(), style));
    }
    let mut right = Vec::new();
    if !page.attempts.is_empty() {
        right.push(Span::styled("attempt ", theme.hint));
        for (index, attempt) in page.attempts.iter().enumerate() {
            if index > 0 {
                right.push(Span::raw("  "));
            }
            let current = page.attempt == Some(attempt.attempt);
            let style = if current {
                theme.notice_label.add_modifier(Modifier::BOLD)
            } else {
                theme.hint
            };
            right.push(Span::styled(attempt.attempt.to_string(), style));
        }
    }
    let used: usize =
        left.iter().map(span_width).sum::<usize>() + right.iter().map(span_width).sum::<usize>();
    let gap = width.saturating_sub(used);
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

/// The diff tab: the file list inside the frame, then the selected file's own open frame.
fn draw_diff(
    lines: &mut Vec<Line<'static>>,
    page: &TaskPage,
    width: usize,
    remaining: usize,
    theme: &Theme,
) {
    let inner = width.saturating_sub(2);
    if page.diff_loading.is_some() && page.diff.is_none() {
        lines.push(rail(
            theme,
            Line::from(Span::styled("loading…", theme.hint)),
        ));
        return;
    }
    if let Some(error) = &page.diff_error {
        lines.push(rail(
            theme,
            Line::from(Span::styled(fit(error, inner), theme.status_error)),
        ));
        return;
    }
    let Some(diff) = &page.diff else {
        lines.push(rail(
            theme,
            Line::from(Span::styled("loading…", theme.hint)),
        ));
        return;
    };
    if diff.files.is_empty() {
        lines.push(rail(
            theme,
            Line::from(Span::styled("no change yet", theme.hint)),
        ));
        return;
    }
    // The task frame holds the file list; the diff frame takes the rest of the page.
    // After the list the frame needs five rows: the task frame's bottom, a blank, the diff
    // frame's title, at least one body row and its bottom. The list is clipped to leave
    // them; the window follows the selected file.
    let file_rows = diff.files.len().min(remaining.saturating_sub(5).max(1));
    let first = page
        .file_sel
        .saturating_sub(file_rows.saturating_sub(1))
        .min(diff.files.len().saturating_sub(file_rows));
    for (index, file) in diff.files.iter().enumerate().skip(first).take(file_rows) {
        let selected = index == page.file_sel;
        let cursor = if selected {
            Span::styled("❯ ", theme.model)
        } else {
            Span::raw("  ")
        };
        let counts = format!(" +{} -{}", file.added, file.removed);
        let path_width = inner.saturating_sub(2 + counts.width());
        let mut spans = vec![
            cursor,
            Span::styled(fit(&file.path, path_width), theme.text),
        ];
        let path_used = file.path.width().min(path_width);
        spans.push(Span::raw(" ".repeat(path_width.saturating_sub(path_used))));
        spans.push(Span::styled(format!(" +{}", file.added), theme.status_ok));
        spans.push(Span::styled(
            format!(" -{}", file.removed),
            theme.status_error,
        ));
        lines.push(rail(theme, Line::from(spans)));
    }
    // The selected file's diff: a second open frame, `╭─ <path> ───`, `│ ` rows, `╰──`.
    let path = &diff.files[page.file_sel].path;
    lines.push(frame_bottom(theme));
    lines.push(Line::default());
    lines.push(frame_top(width, path, theme));
    let body = diff_lines(&diff.files[page.file_sel], inner, theme);
    let start = page.diff_scroll.min(body.len().saturating_sub(1));
    for line in body.into_iter().skip(start) {
        lines.push(rail(theme, line));
    }
}

fn draw_attempts(
    lines: &mut Vec<Line<'static>>,
    page: &TaskPage,
    width: usize,
    height: usize,
    theme: &Theme,
) {
    if page.attempts.is_empty() {
        lines.push(rail(
            theme,
            Line::from(Span::styled("no attempts yet", theme.hint)),
        ));
        return;
    }
    let _ = width;
    // The rows left after the frame's first rows and its bottom. The selected attempt scrolls
    // into view: the list follows it like a terminal list does.
    let visible = height.saturating_sub(lines.len() + 1);
    let current = page
        .attempt_index()
        .unwrap_or(page.attempts.len().saturating_sub(1));
    let first = current.saturating_sub(visible.saturating_sub(1));
    for attempt in page.attempts.iter().skip(first).take(visible) {
        let current = page.attempt == Some(attempt.attempt);
        let cursor = if current {
            Span::styled("❯ ", theme.model)
        } else {
            Span::raw("  ")
        };
        let mut spans = vec![
            cursor,
            Span::styled(
                format!("attempt {}", attempt.attempt),
                if current {
                    theme.notice_label.add_modifier(Modifier::BOLD)
                } else {
                    theme.text
                },
            ),
        ];
        if attempt.verdicts.is_empty() {
            spans.push(Span::styled("   no verdicts yet", theme.hint));
        } else {
            for verdict in &attempt.verdicts {
                spans.push(Span::raw("   "));
                if verdict.passed {
                    spans.push(Span::styled("✓ ", theme.status_ok));
                    spans.push(Span::styled(verdict.stage.to_owned(), theme.text));
                } else {
                    spans.push(Span::styled("× ", theme.status_error));
                    spans.push(Span::styled(verdict.stage.to_owned(), theme.text));
                    if let Some(reason) = &verdict.reason {
                        spans.push(Span::styled(format!(" ({reason})"), theme.hint));
                    }
                }
            }
        }
        let line = Line::from(spans);
        lines.push(rail(theme, line));
    }
}

fn draw_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.min(lines.len() as u16),
    };
    frame.render_widget(Paragraph::new(lines), area);
}

// ---- diff rows -----------------------------------------------------------------------

/// The selected file's diff as rows: `@@` hunks in `diff_hunk`, a `+`/`-`/` ` sign in its
/// diff role, and the code syntax-highlighted through `highlight.rs` (the sign stripped, each
/// run of context/added rows highlighted as one block so multi-line state carries).
fn diff_lines(
    file: &radian_arc::read::DiffFile,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = file.text.lines().collect();
    let lang = crate::highlight::language_for_path(&file.path);
    let mut highlights: Vec<Option<std::sync::Arc<Vec<crate::highlight::HighlightedLine>>>> =
        vec![None; lines.len()];
    let mut offsets: Vec<Option<usize>> = vec![None; lines.len()];
    if let Some(lang) = lang {
        let mut index = 0;
        while index < lines.len() {
            if !is_code_row(lines[index]) {
                index += 1;
                continue;
            }
            let start = index;
            let mut body = String::new();
            while index < lines.len() && is_code_row(lines[index]) {
                body.push_str(diff_code(lines[index]));
                body.push('\n');
                index += 1;
            }
            if let Some(highlighted) = crate::highlight::highlighted(&body, lang) {
                let shared = std::sync::Arc::new(highlighted);
                for (offset, slot) in highlights[start..index].iter_mut().enumerate() {
                    *slot = Some(std::sync::Arc::clone(&shared));
                    offsets[start + offset] = Some(offset);
                }
            }
        }
    }
    let mut out = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let raw = *raw;
        if super::is_diff_metadata(raw) {
            continue;
        }
        if raw.starts_with("@@") {
            out.push(Line::from(Span::styled(fit(raw, width), theme.diff_hunk)));
            continue;
        }
        let (sign, code) = match raw.chars().next() {
            Some(sign @ ('+' | '-' | ' ')) => (sign, &raw[sign.len_utf8()..]),
            _ => (' ', raw),
        };
        let sign_style = match sign {
            '+' => theme.diff_added,
            '-' => theme.diff_removed,
            _ => theme.diff_context,
        };
        let body_style = match sign {
            '+' => theme.diff_added,
            '-' => theme.diff_removed,
            _ => theme.diff_context,
        };
        let mut spans = vec![Span::styled(sign.to_string(), sign_style)];
        let remaining = width.saturating_sub(1);
        let pieces = highlights[index]
            .as_ref()
            .zip(offsets[index])
            .and_then(|(lines, offset)| lines.get(offset))
            .map(|line| crate::highlight::styled_line(line, remaining));
        match pieces {
            Some(pieces) => {
                for (text, role) in pieces {
                    spans.push(Span::styled(text, syntax_style(role, body_style, theme)));
                }
            }
            None => spans.push(Span::styled(fit(code, remaining), body_style)),
        }
        out.push(Line::from(spans));
    }
    out
}

fn syntax_style(role: crate::highlight::SyntaxRole, body: Style, theme: &Theme) -> Style {
    use crate::highlight::SyntaxRole;
    match role {
        SyntaxRole::Plain => body,
        SyntaxRole::Comment => theme.syntax_comment,
        SyntaxRole::Keyword => theme.syntax_keyword,
        SyntaxRole::Function => theme.syntax_function,
        SyntaxRole::Variable => theme.syntax_variable,
        SyntaxRole::String => theme.syntax_string,
        SyntaxRole::Number => theme.syntax_number,
        SyntaxRole::Type => theme.syntax_type,
        SyntaxRole::Operator => theme.syntax_operator,
        SyntaxRole::Punctuation => theme.syntax_punctuation,
    }
}

/// Whether a diff row's code is highlighted: a context or added row (never a removed one, whose
/// code is the old file's).
fn is_code_row(line: &str) -> bool {
    matches!(line.chars().next(), Some('+' | ' ')) && !line.starts_with("+++")
}

fn diff_code(line: &str) -> &str {
    let mut chars = line.chars();
    match chars.next() {
        Some(sign @ ('+' | '-' | ' ')) => &line[sign.len_utf8()..],
        _ => line,
    }
}

// ---- small helpers -------------------------------------------------------------------

fn frame_top(width: usize, label: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![Span::raw("  "), frame_top_inner(width, label, theme)])
}

fn frame_top_inner(width: usize, label: &str, theme: &Theme) -> Span<'static> {
    let label = fit(label, width.saturating_sub(4));
    let used = 3 + label.width() + 1;
    let dashes = width.saturating_sub(used);
    Span::styled(format!("╭─ {label} {}", "─".repeat(dashes)), theme.frame)
}

fn frame_bottom(theme: &Theme) -> Line<'static> {
    Line::from(vec![Span::raw("  "), Span::styled("╰──", theme.frame)])
}

fn rail(theme: &Theme, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("  "), Span::styled("│ ", theme.frame)];
    spans.extend(line.spans);
    Line::from(spans)
}

fn span_width(span: &Span<'_>) -> usize {
    span.content.width()
}

/// Cuts `text` to `width` cells, ending in `…` when it was longer.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > width - 1 {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push('…');
    out
}

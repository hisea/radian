//! The arc TUI's frame (ADR 0035): the header with the page tabs and the loop state, the
//! plans page, the task page and the key hints.
//!
//! The plans page is two columns: the plan list (fixed width) and the selected plan in an open
//! frame. The task page is one open frame; the brief tab draws the task file's body through the
//! transcript's Markdown renderer, the conversation tab puts the attach renderer inside it, the
//! diff tab adds a second open frame for the selected file, and the attempts tab lists each
//! attempt's verdicts.

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
/// The frame rule and the key-hint row at the bottom, plus the loop's ticker line when
/// `loop.log` has gained a transition since the TUI opened.
const FOOTER_ROWS: u16 = 2;

/// The footer's rows in this frame: the ticker takes one only while it has a line to show, so
/// a repository with no loop keeps the body's old size.
fn footer_rows(app: &ArcApp) -> u16 {
    FOOTER_ROWS + u16::from(app.ticker.is_some())
}

/// Draws one whole frame.
pub fn draw(frame: &mut Frame, app: &mut ArcApp) {
    let area = frame.area();
    let footer_rows = footer_rows(app);
    if area.height <= HEADER_ROWS + footer_rows || area.width < 20 {
        return;
    }
    let body = Rect {
        x: area.x,
        y: area.y + HEADER_ROWS,
        width: area.width,
        height: area.height - HEADER_ROWS - footer_rows,
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
        Page::Merge => draw_merge(frame, body, app),
    }
    if app.log_open {
        draw_loop_log(frame, body, app);
    }
    if let Some(log) = app
        .merge
        .as_ref()
        .filter(|_| app.page == Page::Merge)
        .and_then(|page| page.log.as_ref())
    {
        draw_log(frame, body, app, log);
    }
    if let Some(float) = &app.float {
        draw_float(frame, body, app, float);
    }
    let rule_y = area.y + area.height - footer_rows;
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
    if footer_rows > FOOTER_ROWS {
        draw_line(
            frame,
            Rect {
                x: area.x,
                y: rule_y + 1,
                width: area.width,
                height: 1,
            },
            ticker_line(app, area.width as usize),
        );
    }
    draw_line(
        frame,
        Rect {
            x: area.x,
            y: rule_y + footer_rows - 1,
            width: area.width,
            height: 1,
        },
        footer_line(app, area.width as usize),
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
    // The plans page names its view, and counts the plans the view hides (never on `all`).
    if app.page == Page::Plans {
        left.push(Span::styled("  ·  ", theme.hint));
        left.push(Span::styled(app.filter.name(), theme.text));
        if app.filter != PlanFilter::All && app.hidden > 0 {
            left.push(Span::styled(
                format!(" · {} hidden", app.hidden),
                theme.hint,
            ));
        }
    }
    let mut right = vec![
        Span::styled("main ", theme.hint),
        Span::styled(
            app.main_sha.clone().unwrap_or_else(|| "—".to_owned()),
            theme.text,
        ),
        Span::styled("  ·  loop ", theme.hint),
    ];
    match app.loop_status {
        super::looping::LoopStatus::Stopped => {
            right.push(Span::styled("○ ", theme.hint));
            right.push(Span::styled("stopped", theme.hint));
        }
        super::looping::LoopStatus::Starting => {
            right.push(Span::styled("◌ ", theme.hint));
            right.push(Span::styled("starting", theme.hint));
        }
        super::looping::LoopStatus::Elsewhere(pid) => {
            right.push(Span::styled("● ", theme.model));
            right.push(Span::styled(
                pid.map_or_else(
                    || "running".to_owned(),
                    |pid| format!("running (pid {pid})"),
                ),
                theme.text,
            ));
        }
    }
    let left_width: usize = left.iter().map(span_width).sum();
    let right_width: usize = right.iter().map(span_width).sum();
    let gap = width.saturating_sub(left_width + right_width);
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

/// The footer's one-line ticker: the newest transition line `loop.log` gained since the TUI
/// opened, or nothing (ADR 0048 decision 13).
fn ticker_line(app: &ArcApp, width: usize) -> Line<'static> {
    let Some(text) = &app.ticker else {
        return Line::default();
    };
    Line::from(vec![
        Span::styled("  ", app.theme.hint),
        Span::styled("▸ ", app.theme.model),
        Span::styled(fit(text, width.saturating_sub(4)), app.theme.text),
    ])
}

/// One footer hint: the key, what it does, and whether the page can act on it now (weak).
type Hint = (&'static str, &'static str, bool);

fn footer_line(app: &ArcApp, width: usize) -> Line<'static> {
    let theme = &app.theme;
    // An action in flight shows a spinner; its result line then stays until the next key.
    if let Some(pending) = &app.pending {
        return Line::from(vec![
            Span::styled("  ", theme.hint),
            Span::styled(format!("{} ", spinner_glyph(pending.started)), theme.model),
            Span::styled(format!("{} …", pending.label), theme.text),
        ]);
    }
    if let Some(result) = &app.result {
        let style = if result.failed {
            theme.status_error
        } else {
            theme.status_ok
        };
        return Line::from(vec![
            Span::styled("  ", theme.hint),
            Span::styled(fit(&result.text, width.saturating_sub(2)), style),
        ]);
    }
    if app.log_open {
        return Line::from(vec![
            Span::styled("  ", theme.hint),
            Span::styled("j/k scroll   f end   L close   q quit", theme.hint),
        ]);
    }
    let hints: Vec<Hint> = match app.page {
        Page::Plans => {
            let plan = app.selected_plan().map(|view| &view.plan.item);
            let draft = plan.is_some_and(|plan| plan.status == "draft");
            let confirmed = plan.is_some_and(|plan| plan.status == "confirmed");
            let auto_on = plan.is_some_and(|plan| plan.auto != "off");
            let task = app.selected_task().map(|task| &task.item);
            let proposed = app.focus == Focus::Tasks
                && task
                    .is_some_and(|task| task.state == TaskState::Todo && task.phase == "proposed");
            let can_confirm = (app.focus == Focus::Plans && draft) || proposed;
            let have_item = match app.focus {
                Focus::Plans => plan.is_some(),
                Focus::Tasks => task.is_some(),
            };
            let priority = plan.is_some_and(|plan| !plan.is_cancelled());
            vec![
                ("↑↓", "select", false),
                ("enter", "open", false),
                ("c", "confirm", !can_confirm),
                ("x", "cancel", !have_item),
                ("a", "auto", !confirmed),
                ("p", "pause", !auto_on),
                ("s", "start", app.start_blocker().is_some()),
                ("+/-", "prio", !priority),
                ("g", "flat", false),
                ("f", "view", false),
                ("tab", "pages", false),
                ("q", "quit", false),
            ]
        }
        Page::Task => {
            let applies = app.task.as_ref().is_some_and(|page| {
                page.phase.starts_with("review.") || page.phase.starts_with("done.")
            });
            vec![
                ("j/k", "scroll", false),
                ("f", "follow", false),
                ("[ ]", "attempt", false),
                ("n/p", "file", false),
                ("h/l", "tab", false),
                ("tab", "tabs", false),
                ("m", "merge page", !applies),
                ("L", "log", false),
                ("esc", "back", false),
                ("q", "quit", false),
            ]
        }
        Page::Merge => vec![
            ("M", "merge", false),
            ("r", "reject with note", false),
            ("v", "reverify", false),
            ("↑↓", "command", false),
            ("enter", "log", false),
            ("n/p", "file", false),
            ("L", "log", false),
            ("esc", "back", false),
            ("q", "quit", false),
        ],
    };
    let mut spans = vec![Span::styled("  ", theme.hint)];
    for (index, (key, note, weak)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", theme.hint));
        }
        let key_style = if *weak { theme.truncation } else { theme.hint };
        spans.push(Span::styled((*key).to_owned(), key_style));
        spans.push(Span::styled(" ", key_style));
        spans.push(Span::styled((*note).to_owned(), theme.truncation));
    }
    Line::from(spans)
}

/// The rotating spinner frame for an action that started at `started` (ADR 0027: the frame is
/// a function of time).
fn spinner_glyph(started: std::time::Instant) -> &'static str {
    let index = (started.elapsed().as_millis() / 250) as usize % crate::spinner::ARC.len();
    crate::spinner::ARC[index]
}

// ---- plans page ----------------------------------------------------------------------

fn draw_plans(frame: &mut Frame, area: Rect, app: &mut ArcApp) {
    let rows = app.plan_rows();
    let height = area.height as usize;
    // The `PLANS` heading and the blank line under it stay put; each plan takes two rows and
    // one blank separates two plans. The blank after the last plan is not required, so the
    // area holds `1 + 3n` rows for `n` plans.
    let visible_plans = height.saturating_sub(1) / 3;
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
            // A view with no plan leaves the right column empty; a store error keeps its one
            // line there.
            match &app.notice {
                Some(notice) => vec![Line::from(Span::styled(
                    notice.clone(),
                    app.theme.status_error,
                ))],
                None => Vec::new(),
            }
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
        out.push(Line::from(Span::styled(
            format!("  {}", app.filter.empty_text()),
            theme.hint,
        )));
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
            Span::styled(
                format!("{} ", row.priority),
                priority_style(theme, &row.priority),
            ),
            Span::styled(fit(&row.id, LEFT_WIDTH as usize - 8), theme.text),
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
    let label = vec![
        Span::styled(format!("{} {} · ", row.id, row.status), theme.frame),
        Span::styled(row.priority.clone(), priority_style(theme, &row.priority)),
        Span::styled(
            format!(
                " · auto {} · {}/{} merged · {}",
                row.auto,
                row.merged,
                row.total,
                format_cost(row.cost)
            ),
            theme.frame,
        ),
    ];
    let mut out = vec![frame_top_spans(width, label, theme)];
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
        // The heartbeat is live progress only while a loop holds `runner.lock`: `starting`
        // counts as a loop on its way, so only `stopped` adds the row's `loop stopped`.
        let loop_running = !matches!(app.loop_status, super::looping::LoopStatus::Stopped);
        let line = task_row_line(task, id_width, phase_width, cursor, loop_running, theme);
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
/// the worker's freshness for a `doing` task, and `waits`/`also after` when they apply.
fn task_row_line(
    task: &TaskRow,
    id_width: usize,
    phase_width: usize,
    cursor: bool,
    loop_running: bool,
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
    if let Some(freshness) = task.freshness {
        let (text, style) = freshness_text(freshness, theme);
        spans.push(Span::styled(format!("  {text}"), style));
        if !loop_running {
            // A stored `doing` state is not live progress when nothing schedules.
            spans.push(Span::styled("  loop stopped", theme.hint));
        }
    }
    let extra = extra_text(task);
    if !extra.is_empty() {
        spans.push(Span::styled(format!("  {extra}"), theme.hint));
    }
    Line::from(spans)
}

/// The heartbeat line of a `doing` task: `heartbeat 12s ago`, in the warning style with
/// `stalled?` once the beat is older than the stall window, or `starting` before the first one.
fn freshness_text(freshness: Freshness, theme: &Theme) -> (String, Style) {
    match freshness {
        Freshness::Starting => ("starting".to_owned(), theme.hint),
        Freshness::Beating { age } => (format!("heartbeat {} ago", age_text(age)), theme.hint),
        Freshness::Stalled { age } => (
            format!("heartbeat {} ago stalled?", age_text(age)),
            theme.status_warn,
        ),
    }
}

/// A heartbeat's age: `12s`, `5m54s`, `3h07m`, or `12d` from one day up.
fn age_text(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d", secs / 86_400)
    }
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
    let theme = app.theme.clone();
    let width = area.width.saturating_sub(2);
    let (tab, mut lines) = {
        let Some(page) = &app.task else {
            return;
        };
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
        (page.tab, lines)
    };
    match tab {
        TaskTab::Brief => {
            let height = area.height as usize;
            // The frame's first rows and its bottom leave the body this many rows.
            let body_height = height.saturating_sub(lines.len() + 1);
            let Some(page) = app.task.as_mut() else {
                return;
            };
            draw_brief(&mut lines, page, width as usize, body_height, &theme);
        }
        TaskTab::Conversation => {
            draw_conversation(frame, area, app, lines);
            return;
        }
        TaskTab::Diff => {
            let Some(page) = &app.task else {
                return;
            };
            let diff = DiffState {
                diff: &page.diff,
                diff_loading: page.diff_loading.as_ref(),
                diff_error: &page.diff_error,
                file_sel: page.file_sel,
                diff_scroll: page.diff_scroll,
            };
            draw_diff(
                &mut lines,
                &diff,
                width as usize,
                area.height.saturating_sub(4) as usize,
                &theme,
            );
        }
        TaskTab::Attempts => {
            let height = area.height as usize;
            // The attempts window follows the selection the way the plan list does: it moves
            // only when the selection leaves its top or its bottom row.
            if let Some(page) = app.task.as_mut() {
                let visible = height.saturating_sub(lines.len() + 1);
                let current = page
                    .attempt_index()
                    .unwrap_or(page.attempts.len().saturating_sub(1));
                page.attempt_top =
                    scroll_top(page.attempt_top, current, visible, page.attempts.len());
            }
            let Some(page) = &app.task else {
                return;
            };
            draw_attempts(&mut lines, page, width as usize, height, &theme);
        }
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

/// The brief tab: the task file's body, markdown through the transcript's own renderer, one
/// frame rail per row. The body is loaded from the store with the task (never rebuilt here),
/// so it is the brief and its rework sections, not an attempt's hand-off.
fn draw_brief(
    lines: &mut Vec<Line<'static>>,
    page: &mut TaskPage,
    width: usize,
    height: usize,
    theme: &Theme,
) {
    if height == 0 {
        return;
    }
    // The rail takes the first two columns of the frame's inner width; the transcript's own
    // assistant marker takes two more.
    let inner = width.saturating_sub(2);
    let entry = crate::app::Entry::new(crate::app::EntryKind::Assistant, page.brief.clone());
    let rendered = crate::render::transcript_lines(
        std::slice::from_ref(&entry),
        crate::tools::DisplayModes::default(),
        inner,
        theme,
    );
    // The last page is where the frame clamps: `f` goes there, and a `k` after it moves up
    // from the real end rather than from a sentinel.
    let last = rendered.len().saturating_sub(height);
    if page.brief_follow {
        page.brief_scroll = last;
        page.brief_follow = false;
    } else {
        page.brief_scroll = page.brief_scroll.min(last);
    }
    for line in rendered.into_iter().skip(page.brief_scroll).take(height) {
        lines.push(rail(theme, line));
    }
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

/// The tab row: the four tabs on the left, the attempt selector on the right.
fn tab_row(page: &TaskPage, width: usize, theme: &Theme) -> Line<'static> {
    let mut left = Vec::new();
    for (index, tab) in [
        TaskTab::Brief,
        TaskTab::Conversation,
        TaskTab::Diff,
        TaskTab::Attempts,
    ]
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

/// The diff a page draws: what is loaded, what is in flight, and the selection. Both the task
/// page's diff tab and the merge page draw through the same functions.
struct DiffState<'a> {
    diff: &'a Option<radian_arc::read::TaskDiff>,
    diff_loading: Option<&'a (String, u32)>,
    diff_error: &'a Option<String>,
    file_sel: usize,
    diff_scroll: usize,
}

/// The task page's diff tab: the file list inside the frame, then the selected file's own open
/// frame.
fn draw_diff(
    lines: &mut Vec<Line<'static>>,
    diff: &DiffState,
    width: usize,
    remaining: usize,
    theme: &Theme,
) {
    // The source line of a running attempt's diff: the worker's worktree, not a commit range
    // (ADR 0048 decision 13).
    let live = diff
        .diff
        .as_ref()
        .is_some_and(|diff| diff.source == radian_arc::read::DiffSource::Worktree);
    if live {
        lines.push(rail(
            theme,
            Line::from(Span::styled("live · worktree", theme.hint)),
        ));
    }
    // After the list the frame needs five rows: the frame's bottom, a blank, the diff frame's
    // title, at least one body row and its bottom. The list is clipped to leave them.
    let rows = remaining.saturating_sub(5 + usize::from(live)).max(1);
    if !diff_file_list(lines, diff, width.saturating_sub(2), rows, theme, true) {
        return;
    }
    lines.push(frame_bottom(theme));
    diff_file_frame(lines, diff, width, theme);
}

/// The file list rows (or the one loading/error/empty line). Returns whether a diff follows.
fn diff_file_list(
    lines: &mut Vec<Line<'static>>,
    diff: &DiffState,
    inner: usize,
    rows: usize,
    theme: &Theme,
    framed: bool,
) -> bool {
    let row = |line: Line<'static>| -> Line<'static> {
        if framed {
            rail(theme, line)
        } else {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(line.spans);
            Line::from(spans)
        }
    };
    if diff.diff_loading.is_some() && diff.diff.is_none() {
        lines.push(row(Line::from(Span::styled("loading…", theme.hint))));
        return false;
    }
    if let Some(error) = diff.diff_error {
        lines.push(row(Line::from(Span::styled(
            fit(error, inner),
            theme.status_error,
        ))));
        return false;
    }
    let Some(task_diff) = diff.diff else {
        lines.push(row(Line::from(Span::styled("loading…", theme.hint))));
        return false;
    };
    if task_diff.files.is_empty() {
        lines.push(row(Line::from(Span::styled("no change yet", theme.hint))));
        return false;
    }
    let file_rows = task_diff.files.len().min(rows);
    let first = diff
        .file_sel
        .saturating_sub(file_rows.saturating_sub(1))
        .min(task_diff.files.len().saturating_sub(file_rows));
    for (index, file) in task_diff
        .files
        .iter()
        .enumerate()
        .skip(first)
        .take(file_rows)
    {
        let selected = index == diff.file_sel;
        let cursor = if selected {
            Span::styled("❯ ", theme.model)
        } else {
            Span::raw("  ")
        };
        let counts = format!(" +{} -{}", file.added, file.removed);
        let mark = if file.uncommitted { " uncommitted" } else { "" };
        let path_width = inner.saturating_sub(2 + counts.width() + mark.width());
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
        if file.uncommitted {
            spans.push(Span::styled(mark, theme.hint));
        }
        lines.push(row(Line::from(spans)));
    }
    true
}

/// The selected file's own open frame (`╭─ <path> ───`, `│ ` body rows). The caller adds the
/// frame's bottom, so a clipped body still ends on a frame line.
fn diff_file_frame(lines: &mut Vec<Line<'static>>, diff: &DiffState, width: usize, theme: &Theme) {
    let Some(task_diff) = diff.diff else {
        return;
    };
    if task_diff.files.is_empty() {
        return;
    }
    let index = diff.file_sel.min(task_diff.files.len() - 1);
    let file = &task_diff.files[index];
    lines.push(Line::default());
    lines.push(frame_top(width, &file.path, theme));
    let body = diff_lines(file, width.saturating_sub(2), theme);
    let start = diff.diff_scroll.min(body.len().saturating_sub(1));
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
    // The rows left after the frame's first rows and its bottom. The window is kept in
    // `attempt_top` and moves only when the selection leaves it, like the plan list.
    let visible = height.saturating_sub(lines.len() + 1);
    let first = page.attempt_top;
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

// ---- merge page ----------------------------------------------------------------------

/// The merge page (ADR 0048 §6 and §10, ADR 0035): the task's open frame with one row per
/// stage, then the review's summary and the task's commits in a `summary` frame, then the
/// file list and the selected file's diff through the task page's own diff functions.
fn draw_merge(frame: &mut Frame, area: Rect, app: &mut ArcApp) {
    let Some(page) = &app.merge else {
        return;
    };
    let theme = app.theme.clone();
    let width = area.width as usize;
    let inner = width.saturating_sub(4);
    let (verified, attempt) = match &page.view {
        Some(view) => (
            view.verified.clone().unwrap_or_else(|| "—".to_owned()),
            view.attempt,
        ),
        None => ("—".to_owned(), page.attempt),
    };
    let label = format!("{} verified {verified} · attempt {attempt}", page.id);
    let mut lines = vec![frame_top(width, &label, &theme)];
    match &page.view {
        Some(view) => {
            lines.push(rail(
                &theme,
                Line::from(Span::styled(fit(&view.title, inner), theme.text)),
            ));
            lines.push(rail(&theme, Line::default()));
            if view.moved {
                lines.push(rail(
                    &theme,
                    Line::from(Span::styled(moved_text(view), theme.notice_label)),
                ));
            }
            let mut command_index = 0;
            for stage in &view.stages {
                lines.push(rail(&theme, stage_line(stage, view.moved, &theme)));
                for command in &stage.commands {
                    let selected = command_index == page.command_sel;
                    lines.push(rail(&theme, command_line(command, selected, inner, &theme)));
                    command_index += 1;
                }
            }
        }
        None => {
            let (text, style) = match &page.error {
                Some(error) => (error.clone(), theme.status_error),
                None => ("loading…".to_owned(), theme.hint),
            };
            lines.push(rail(
                &theme,
                Line::from(Span::styled(fit(&text, inner), style)),
            ));
        }
    }
    lines.push(frame_bottom(&theme));
    if let Some(view) = &page.view {
        lines.push(Line::default());
        summary_lines(&mut lines, view, width, &theme);
        lines.push(Line::default());
        let diff = DiffState {
            diff: &page.diff,
            diff_loading: page.diff_loading.as_ref(),
            diff_error: &page.diff_error,
            file_sel: page.file_sel,
            diff_scroll: page.diff_scroll,
        };
        let remaining = (area.height as usize).saturating_sub(lines.len());
        if diff
            .diff
            .as_ref()
            .is_some_and(|diff| !diff.files.is_empty())
        {
            diff_file_list(
                &mut lines,
                &diff,
                width.saturating_sub(2),
                remaining.saturating_sub(4).max(1),
                &theme,
                false,
            );
            diff_file_frame(&mut lines, &diff, width, &theme);
        } else {
            let (text, style) = match &page.diff_error {
                Some(error) => (error.clone(), theme.status_error),
                None if page.diff_loading.is_some() => ("loading…".to_owned(), theme.hint),
                None => ("no change yet".to_owned(), theme.hint),
            };
            lines.push(Line::from(Span::styled(
                format!("  {}", fit(&text, width.saturating_sub(2))),
                style,
            )));
        }
        lines.push(frame_bottom(&theme));
    }
    // The last row is always a frame's tail: a body longer than the page loses its last rows,
    // never the bottom line.
    let height = area.height as usize;
    if lines.len() > height {
        lines.truncate(height);
        if let Some(last) = lines.last_mut() {
            *last = frame_bottom(&theme);
        }
    }
    draw_lines(frame, area, lines);
}

/// `the branch moved: head <sha> is not verified <sha>; the task is back in verification`.
fn moved_text(view: &radian_arc::read::MergeView) -> String {
    let head = view.head.clone().unwrap_or_else(|| "—".to_owned());
    let verified = view.verified.clone().unwrap_or_else(|| "—".to_owned());
    format!(
        "the branch moved: head {head} is not verified {verified}; the task is back in \
         verification"
    )
}

/// One stage row: mark, name, `on <sha>`, the candidate, and what it ran.
fn stage_line(stage: &radian_arc::read::StageView, moved: bool, theme: &Theme) -> Line<'static> {
    let (glyph, style) = stage_mark(stage.mark, moved, theme);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(format!("{:<11} ", stage.stage), theme.text),
    ];
    if let Some(on) = &stage.on {
        spans.push(Span::styled(format!("on {on}   "), theme.hint));
    }
    if let Some(candidate) = &stage.candidate {
        spans.push(Span::styled(
            format!("→ candidate {candidate}"),
            theme.notice_label,
        ));
        if let Some(main) = &stage.main {
            spans.push(Span::styled(format!(" over main {main}"), theme.hint));
        }
        spans.push(Span::styled("   ", theme.hint));
    }
    match stage.mark {
        radian_arc::read::StageMark::NotInProfile => {
            spans.push(Span::styled("not in this profile", theme.hint));
        }
        radian_arc::read::StageMark::NotRun if stage.reason.is_none() => {
            spans.push(Span::styled("not run yet", theme.hint));
        }
        _ => {
            let mut details = Vec::new();
            if stage.stage == "human" {
                if let Some(approver) = &stage.approver {
                    details.push(format!("approver {approver}"));
                }
            } else if stage.stage == "review" {
                if stage.sessions > 0 {
                    details.push(count(stage.sessions, "session", "sessions"));
                }
            } else if !stage.commands.is_empty() {
                details.push(count(stage.commands.len(), "command", "commands"));
            }
            if let Some(duration) = stage.duration_ms {
                details.push(stage_duration(duration));
            }
            if !details.is_empty() {
                spans.push(Span::styled(details.join(" · "), theme.hint));
            }
            if let Some(reason) = &stage.reason {
                if !details.is_empty() {
                    spans.push(Span::raw("   "));
                }
                spans.push(Span::styled(format!("({reason})"), theme.status_error));
            }
        }
    }
    Line::from(spans)
}

/// The mark and its role. A moved branch turns every mark weak (ADR 0048 §6).
fn stage_mark(mark: radian_arc::read::StageMark, moved: bool, theme: &Theme) -> (char, Style) {
    use radian_arc::read::StageMark;
    if moved {
        return ('◻', theme.hint);
    }
    match mark {
        StageMark::Passed => ('✓', theme.status_ok),
        StageMark::Failed => ('×', theme.status_error),
        StageMark::Running => ('●', theme.model),
        StageMark::NotRun | StageMark::NotInProfile => ('◻', theme.hint),
    }
}

/// One command row: the mark, the line (truncated) and the duration in seconds.
fn command_line(
    command: &radian_arc::read::CommandView,
    selected: bool,
    inner: usize,
    theme: &Theme,
) -> Line<'static> {
    let (glyph, style) = if command.passed {
        ('✓', theme.status_ok)
    } else {
        ('×', theme.status_error)
    };
    let cursor = if selected {
        Span::styled("❯ ", theme.model)
    } else {
        Span::raw("  ")
    };
    let seconds = format!("{}s", command.duration_ms / 1_000);
    let line_width = inner.saturating_sub(4 + seconds.width() + 1);
    let used = 2 + 2 + command.line.width().min(line_width);
    let mut spans = vec![
        cursor,
        Span::styled(format!("{glyph} "), style),
        Span::styled(fit(&command.line, line_width), theme.text),
    ];
    spans.push(Span::raw(
        " ".repeat(inner.saturating_sub(used + seconds.width())),
    ));
    spans.push(Span::styled(seconds, theme.hint));
    Line::from(spans)
}

/// The `summary` frame: the review's summary and its non-empty groups, then the commits.
fn summary_lines(
    lines: &mut Vec<Line<'static>>,
    view: &radian_arc::read::MergeView,
    width: usize,
    theme: &Theme,
) {
    let inner = width.saturating_sub(4);
    lines.push(frame_top(width, "summary", theme));
    if view.moved {
        // A verdict counts only for its sha (ADR 0048 §6): the review is for the old commit.
        lines.push(rail(
            theme,
            Line::from(Span::styled(
                "no review for this commit: the branch moved",
                theme.hint,
            )),
        ));
    } else if !view.review_stage {
        lines.push(rail(
            theme,
            Line::from(Span::styled("no review stage in this profile", theme.hint)),
        ));
    } else if let Some(review) = &view.review {
        for text in crate::render::wrap(&review.summary, inner) {
            lines.push(rail(theme, Line::from(Span::styled(text, theme.text))));
        }
        let groups = [
            ("blocking", &review.blocking, '×', theme.status_error),
            ("suggestions", &review.suggestions, '•', theme.text),
            ("verified", &review.verified, '✓', theme.status_ok),
        ];
        for (label, items, glyph, style) in groups {
            if items.is_empty() {
                continue;
            }
            lines.push(rail(theme, Line::default()));
            lines.push(rail(
                theme,
                Line::from(Span::styled(label.to_owned(), theme.notice_label)),
            ));
            for item in items {
                for (index, text) in crate::render::wrap(&format!("{glyph} {item}"), inner)
                    .into_iter()
                    .enumerate()
                {
                    let text = if index == 0 {
                        text
                    } else {
                        format!("  {text}")
                    };
                    lines.push(rail(theme, Line::from(Span::styled(text, style))));
                }
            }
        }
    } else {
        lines.push(rail(
            theme,
            Line::from(Span::styled("no review yet", theme.hint)),
        ));
    }
    if !view.commits.is_empty() {
        lines.push(rail(theme, Line::default()));
        for commit in &view.commits {
            lines.push(rail(
                theme,
                Line::from(Span::styled(
                    fit(&format!("• {} {}", commit.sha, commit.subject), inner),
                    theme.text,
                )),
            ));
        }
    }
    if let Some(error) = &view.commits_error {
        lines.push(rail(
            theme,
            Line::from(Span::styled(fit(error, inner), theme.status_error)),
        ));
    }
    lines.push(frame_bottom(theme));
}

/// `N commands`, `1 session`.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// A stage's wall time: `12s`, or `5m54s` from one minute up.
fn stage_duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

/// The confirmation or reject float: `panel.rs`'s rounded box, centred over the page.
fn draw_float(frame: &mut Frame, area: Rect, app: &ArcApp, float: &Float) {
    let theme = &app.theme;
    let width = (area.width.saturating_sub(8) as usize)
        .clamp(20, 72)
        .min(area.width as usize)
        .max(1);
    let panel = match float {
        Float::Confirm { message, .. } => crate::panel::Panel::new("arc")
            .row(Line::from(Span::styled(message.clone(), theme.text)))
            .hint("y", "yes")
            .hint("n", "no"),
        Float::Reject { note } => crate::panel::Panel::new("reject")
            .row(Line::from(Span::styled("note (one line):", theme.hint)))
            .row(Line::from(vec![
                Span::styled("> ", theme.notice_label),
                Span::styled(note.clone(), theme.text),
                Span::styled("▏", theme.hint),
            ]))
            .hint("enter", "reject")
            .hint("esc", "cancel"),
    };
    let lines = panel.lines(width, theme, crate::footer::Glyphs::Unicode);
    let height = (lines.len() as u16).min(area.height);
    let x = area.x + area.width.saturating_sub(width as u16) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x,
        y,
        width: width as u16,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(Paragraph::new(lines), rect);
}

/// A command's log in a scrollable float, over the merge page.
fn draw_log(frame: &mut Frame, area: Rect, app: &ArcApp, log: &LogView) {
    let theme = &app.theme;
    let width = (area.width.saturating_sub(6) as usize)
        .clamp(24, 100)
        .min(area.width as usize)
        .max(1);
    let height = (area.height.saturating_sub(4) as usize).clamp(5, 18);
    let window = height.saturating_sub(2);
    let start = log.scroll.min(log.lines.len().saturating_sub(1));
    let inner = width.saturating_sub(4);
    let mut panel = crate::panel::Panel::new(fit(&log.title, inner));
    for line in log.lines.iter().skip(start).take(window) {
        panel = panel.row(Line::from(Span::styled(fit(line, inner), theme.text)));
    }
    let panel = panel.hint("j/k", "scroll").hint("esc", "close");
    let lines = panel.lines(width, theme, crate::footer::Glyphs::Unicode);
    let h = (lines.len() as u16).min(area.height);
    let x = area.x + area.width.saturating_sub(width as u16) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: width as u16,
        height: h,
    };
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(Paragraph::new(lines), rect);
}

/// The loop's log (`L`): the tail of `loop.log`, scrollable, over the page (ADR 0048
/// decision 13).
fn draw_loop_log(frame: &mut Frame, area: Rect, app: &ArcApp) {
    let theme = &app.theme;
    let width = area.width as usize;
    let inner = width.saturating_sub(4);
    let mut lines = vec![frame_top(width, "log", theme)];
    if app.log_missing {
        lines.push(rail(
            theme,
            Line::from(Span::styled("no loop log yet", theme.hint)),
        ));
    } else if app.log.is_empty() {
        lines.push(rail(
            theme,
            Line::from(Span::styled("no lines yet", theme.hint)),
        ));
    } else {
        let rows = (area.height as usize).saturating_sub(2);
        let start = app.log_scroll.min(app.log.len().saturating_sub(1));
        for line in app.log.iter().skip(start).take(rows) {
            lines.push(rail(
                theme,
                Line::from(Span::styled(fit(line, inner), theme.text)),
            ));
        }
    }
    lines.push(frame_bottom(theme));
    draw_lines(frame, area, lines);
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
    frame_top_spans(
        width,
        vec![Span::styled(label.to_owned(), theme.frame)],
        theme,
    )
}

/// [`frame_top`] with a label that carries more than one style: the plan frame draws the
/// priority in its own role (ADR 0048 decision 3). The label is fitted as a whole, so a narrow
/// frame still ends in `…`.
fn frame_top_spans(width: usize, label: Vec<Span<'static>>, theme: &Theme) -> Line<'static> {
    let label = fit_spans(label, width.saturating_sub(4));
    let used = 3 + label.iter().map(span_width).sum::<usize>() + 1;
    let dashes = width.saturating_sub(used);
    let mut spans = vec![Span::raw("  "), Span::styled("╭─ ", theme.frame)];
    spans.extend(label);
    spans.push(Span::styled(
        format!(" {}", "─".repeat(dashes)),
        theme.frame,
    ));
    Line::from(spans)
}

/// Cuts styled spans to `width` cells, ending in `…` when they were longer. The style of each
/// retained character stays its own; the ellipsis takes the last retained span's style.
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if spans.iter().map(span_width).sum::<usize>() <= width {
        return spans;
    }
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut used = 0;
    let mut style = Style::default();
    for span in spans {
        let span_style = span.style;
        let mut text = String::new();
        let mut cut = false;
        for ch in span.content.chars() {
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > width - 1 {
                cut = true;
                break;
            }
            text.push(ch);
            used += cell;
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span_style));
            style = span_style;
        }
        if cut {
            break;
        }
    }
    out.push(Span::styled("…", style));
    out
}

/// The theme role a plan's `P0`..`P5` text takes (ADR 0048 decision 3). A value the store
/// cannot produce falls back to the weak hint level rather than inventing a color.
fn priority_style(theme: &Theme, priority: &str) -> Style {
    match priority {
        "P0" => theme.priority_p0,
        "P1" => theme.priority_p1,
        "P2" => theme.priority_p2,
        "P3" => theme.priority_p3,
        "P4" => theme.priority_p4,
        "P5" => theme.priority_p5,
        _ => theme.hint,
    }
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

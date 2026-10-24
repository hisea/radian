//! The arc TUI's board page (ADR 0048 §1, §6 and §13): every open task of the plans in scope,
//! grouped by who acts next, and the finished list `d` opens.
//!
//! The columns are `todo`, `doing`, `verifying` and `needs you`. A card is one task; a column
//! holds its cards in claim order (the plan's priority, the plan id, the task's order in the
//! plan), which is the order `radian arc list` prints. The board is read-only like the plans
//! page: an owner's key queues the same [`super::actions::Action`] the plans page queues.
//!
//! Three widths (ADR 0035's layout rules):
//!
//! - **wide** (four columns fit): the four columns side by side;
//! - **medium** (two fit): two columns, the left stacking `todo` over `doing`, the right
//!   stacking `verifying` over `needs you`;
//! - **narrow** (one fits): the four groups stacked in the same order.
//!
//! A column is readable at 28 cells; two side-by-side columns need a two-cell gap, and so do
//! four. The selection is the pair (column, card in that column), so a width change keeps the
//! same card selected.

use std::collections::BTreeSet;

use radian_arc::read;
use radian_arc::store::attempt_ops;
use radian_arc::store::model::{PlanFile, TaskFile, TaskState};
use radian_arc::store::{PlanView, Store};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::render::scroll_top;
use super::{ArcApp, Mark, format_cost, task_mark};
use crate::theme::Theme;

/// The columns, left to right.
pub const COLUMNS: usize = 4;
pub const TODO: usize = 0;
pub const DOING: usize = 1;
pub const VERIFYING: usize = 2;
pub const NEEDS_YOU: usize = 3;

/// The heading each group draws, in column order.
pub(crate) const COLUMN_HEADINGS: [&str; COLUMNS] = ["TODO", "DOING", "VERIFYING", "NEEDS YOU"];
/// The one weak line a group with no card shows.
pub const COLUMN_EMPTY: [&str; COLUMNS] = [
    "nothing waiting",
    "nothing running",
    "nothing verifying",
    "nothing needs you",
];

/// The least width one column reads at, in cells. The manual states it.
pub const MIN_COLUMN_WIDTH: u16 = 28;
/// The cells between two side-by-side columns.
pub const COLUMN_GAP: u16 = 2;

/// A card is three rows; the fourth separates it from the next card.
const CARD_LINES: usize = 3;

/// How the board lays its groups out at one width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoardLayout {
    /// Four columns side by side.
    #[default]
    Wide,
    /// Two columns, each stacking two groups.
    Medium,
    /// One column with the four groups stacked.
    Narrow,
}

impl BoardLayout {
    /// The layout that fits `width` cells: four readable columns plus their gaps, else two,
    /// else one.
    #[must_use]
    pub fn of(width: u16) -> Self {
        let wide = COLUMNS as u16 * MIN_COLUMN_WIDTH + (COLUMNS as u16 - 1) * COLUMN_GAP;
        let medium = 2 * MIN_COLUMN_WIDTH + COLUMN_GAP;
        if width >= wide {
            BoardLayout::Wide
        } else if width >= medium {
            BoardLayout::Medium
        } else {
            BoardLayout::Narrow
        }
    }
}

/// One card: three lines of a task.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardCard {
    pub id: String,
    pub title: String,
    pub plan: String,
    pub phase: String,
    pub attempt: u32,
    pub cost: Option<f64>,
    /// The state mark the card would draw outside `needs you`.
    pub mark: Mark,
    /// The card is in `needs you`: it draws the `!` mark instead of the state mark.
    pub needs_you: bool,
    /// For a `doing` card, the age of the wrapper's last heartbeat (`12s`), when it beat.
    pub beat: Option<String>,
}

/// One row of the finished list.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedRow {
    pub id: String,
    pub title: String,
    pub plan: String,
    pub mark: Mark,
    /// The record's RFC 3339 time, the sort key. Empty when no record could be read.
    pub at: String,
    /// Seconds since `at`, for the `12m ago` text.
    pub age: Option<u64>,
}

/// The board's state: the four columns, the selection, the scope and the finished list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardPage {
    pub columns: [Vec<BoardCard>; COLUMNS],
    /// The selected column and the card's index in it.
    pub column: usize,
    pub row: usize,
    /// The first line drawn, one per column (wide) or per pane (medium: 0 and 2; narrow: 0).
    pub top: [usize; COLUMNS],
    /// `p`: only this plan, or every plan of the plans page's view.
    pub plan: Option<String>,
    /// `d`: the finished list replaces the columns.
    pub finished: bool,
    pub done: Vec<FinishedRow>,
    pub done_sel: usize,
    pub done_top: usize,
    /// The last width the frame drew, so the key handlers move the way it looks.
    pub layout: BoardLayout,
}

impl BoardPage {
    /// The selected card, when the selection is on one.
    #[must_use]
    pub fn selected(&self) -> Option<&BoardCard> {
        self.columns.get(self.column)?.get(self.row)
    }

    /// The selected card's plan, when there is a card.
    #[must_use]
    pub fn selected_plan(&self) -> Option<&str> {
        self.selected().map(|card| card.plan.as_str())
    }

    /// The selected finished row, when the list is not empty.
    #[must_use]
    pub fn selected_done(&self) -> Option<&FinishedRow> {
        self.done.get(self.done_sel)
    }

    /// Rebuilds the cards from `plans` (already in claim order) and puts the selection back on
    /// the task it was on. A task that disappeared leaves the selection on the nearest card of
    /// its column. The finished list follows the same rule.
    pub fn rebuild(&mut self, plans: &[PlanView], store: &Store) {
        let keep = self
            .columns
            .get(self.column)
            .and_then(|cards| cards.get(self.row))
            .map(|card| card.id.clone());
        let keep_done = self.done.get(self.done_sel).map(|row| row.id.clone());
        let paused = paused_plans(plans, store);
        let mut columns: [Vec<BoardCard>; COLUMNS] = std::array::from_fn(|_| Vec::new());
        let mut done = Vec::new();
        for view in plans {
            let plan = &view.plan.item;
            let paused = paused.contains(&plan.id);
            for task in &view.tasks {
                let item = &task.item;
                if let Some(column) = column_of(store, plan, item, paused) {
                    columns[column].push(card(store, item, column));
                }
                if is_finished(item) {
                    done.push(finished_row(store, plan, item));
                }
            }
        }
        done.sort_by(|one, other| other.at.cmp(&one.at).then_with(|| one.id.cmp(&other.id)));
        self.columns = columns;
        self.done = done;

        let found = keep.and_then(|id| {
            self.columns.iter().enumerate().find_map(|(column, cards)| {
                cards
                    .iter()
                    .position(|card| card.id == id)
                    .map(|row| (column, row))
            })
        });
        match found {
            Some((column, row)) => {
                self.column = column;
                self.row = row;
            }
            None => {
                let count = self.columns[self.column].len();
                self.row = if count == 0 {
                    0
                } else {
                    self.row.min(count - 1)
                };
            }
        }
        let done_found = keep_done.and_then(|id| self.done.iter().position(|row| row.id == id));
        self.done_sel = match done_found {
            Some(index) => index,
            None if self.done.is_empty() => 0,
            None => self.done_sel.min(self.done.len() - 1),
        };
    }

    /// `h`/`l`: the next column (wide and narrow), or the other pane (medium). The selected
    /// group in the new pane is the one at the same place: `todo` ↔ `verifying`, `doing` ↔
    /// `needs you`.
    pub fn move_horizontal(&mut self, delta: i32) {
        if self.finished || delta == 0 {
            return;
        }
        let next = match self.layout {
            BoardLayout::Medium => {
                let pane = self.column / 2;
                let next_pane = (pane as i32 + delta).clamp(0, 1) as usize;
                next_pane * 2 + self.column % 2
            }
            BoardLayout::Wide | BoardLayout::Narrow => {
                (self.column as i32 + delta).clamp(0, COLUMNS as i32 - 1) as usize
            }
        };
        self.column = next;
        self.row = self.row.min(self.columns[next].len().saturating_sub(1));
    }

    /// `j`/`k`: the next or previous card of the way the layout reads. Wide stays in the
    /// selected column; medium walks the pane's two groups; narrow walks all four.
    pub fn move_vertical(&mut self, delta: i32) {
        if self.finished {
            self.move_done(delta);
            return;
        }
        if delta == 0 {
            return;
        }
        let (cards, anchor) = self.sequence();
        if cards.is_empty() {
            return;
        }
        let current = cards
            .iter()
            .position(|(column, row)| *column == self.column && *row == self.row);
        let index = match current {
            Some(index) => (index as i32 + delta).clamp(0, cards.len() as i32 - 1) as usize,
            None if delta > 0 => anchor.min(cards.len() - 1),
            None => anchor.saturating_sub(1).min(cards.len() - 1),
        };
        (self.column, self.row) = cards[index];
    }

    /// Every card in the order the current layout reads, and where the selected column's cards
    /// would start in it (the anchor an empty column moves from).
    fn sequence(&self) -> (Vec<(usize, usize)>, usize) {
        let groups: &[usize] = match self.layout {
            BoardLayout::Wide => &[],
            BoardLayout::Medium if self.column < 2 => &[TODO, DOING],
            BoardLayout::Medium => &[VERIFYING, NEEDS_YOU],
            BoardLayout::Narrow => &[TODO, DOING, VERIFYING, NEEDS_YOU],
        };
        if groups.is_empty() {
            let cards = (0..self.columns[self.column].len())
                .map(|row| (self.column, row))
                .collect::<Vec<_>>();
            return (cards, 0);
        }
        let mut cards = Vec::new();
        let mut anchor = 0;
        for group in groups {
            if *group == self.column {
                anchor = cards.len();
            }
            cards.extend((0..self.columns[*group].len()).map(|row| (*group, row)));
        }
        if !groups.contains(&self.column) {
            anchor = cards.len();
        }
        (cards, anchor)
    }

    /// `p`: limits the board to the selected card's plan, or shows every plan of the view
    /// again. `false` when there is no card to pin.
    pub fn toggle_pin(&mut self) -> bool {
        if self.plan.is_some() {
            self.plan = None;
            return true;
        }
        match self.selected_plan().map(str::to_owned) {
            Some(plan) => {
                self.plan = Some(plan);
                true
            }
            None => false,
        }
    }

    /// `d`: the finished list.
    pub fn show_finished(&mut self) {
        self.finished = true;
        if self.done.is_empty() {
            self.done_sel = 0;
        } else {
            self.done_sel = self.done_sel.min(self.done.len() - 1);
        }
    }

    /// `d`/`Esc` on the finished list: back to the columns.
    pub fn show_columns(&mut self) {
        self.finished = false;
    }

    /// `j`/`k` on the finished list.
    pub fn move_done(&mut self, delta: i32) {
        if self.done.is_empty() {
            return;
        }
        self.done_sel =
            (self.done_sel as i32 + delta).clamp(0, self.done.len() as i32 - 1) as usize;
    }
}

/// Whether `task` is finished: `done.merged` or a `failed.*` task. Those leave the columns and
/// appear in the finished list only.
#[must_use]
pub fn is_finished(task: &TaskFile) -> bool {
    matches!(task.state, TaskState::Done | TaskState::Failed)
}

/// The column a task belongs to, or `None` for a finished task.
///
/// `paused` marks a plan `arc` paused through one of its tasks: every task of it waits for the
/// owner. A `review.rework_pending` task a scheduling pass would still reverify by itself (a
/// command failure with automatic reverifies left) stays in `verifying`, because a gate runs
/// for it once main moves; the rule is `radian_arc::review::auto_reverify_pending`, the pass's
/// own.
#[must_use]
pub fn column_of(store: &Store, plan: &PlanFile, task: &TaskFile, paused: bool) -> Option<usize> {
    if is_finished(task) {
        return None;
    }
    if paused {
        return Some(NEEDS_YOU);
    }
    match (task.state, task.phase.as_str()) {
        (TaskState::Todo, "proposed" | "paused_budget") => Some(NEEDS_YOU),
        (TaskState::Todo, _) => Some(TODO),
        (TaskState::Doing, _) => Some(DOING),
        (TaskState::Review, "awaiting_human") => Some(if plan.auto == "merge" {
            VERIFYING
        } else {
            NEEDS_YOU
        }),
        (TaskState::Review, "rework_pending") => Some(
            if radian_arc::review::auto_reverify_pending(store, &task.id) {
                VERIFYING
            } else {
                NEEDS_YOU
            },
        ),
        (TaskState::Review, _) => Some(VERIFYING),
        (TaskState::Done | TaskState::Failed, _) => None,
    }
}

/// One card's data.
fn card(store: &Store, task: &TaskFile, column: usize) -> BoardCard {
    let beat = (task.state == TaskState::Doing)
        .then(|| {
            attempt_ops::read_heartbeat(store, &task.id, task.attempt)
                .or_else(|| attempt_ops::read_start_time(store, &task.id, task.attempt))
                .and_then(|at| attempt_ops::utc_age_secs(&at))
                .map(short_age)
        })
        .flatten();
    BoardCard {
        id: task.id.clone(),
        title: task.title.clone(),
        plan: task.plan.clone(),
        phase: task.phase.clone(),
        attempt: task.attempt,
        cost: task.cost_usd,
        mark: task_mark(task.state, &task.phase),
        needs_you: column == NEEDS_YOU,
        beat,
    }
}

/// One finished row's data.
fn finished_row(store: &Store, plan: &PlanFile, task: &TaskFile) -> FinishedRow {
    let at = read::finished_at(store, &task.id).unwrap_or_default();
    FinishedRow {
        id: task.id.clone(),
        title: task.title.clone(),
        plan: plan.id.clone(),
        mark: task_mark(task.state, &task.phase),
        age: attempt_ops::utc_age_secs(&at),
        at,
    }
}

/// Every plan `arc` paused through one of `plans`' tasks: its automatic rework limit wrote
/// `paused.json` next to the attempt and set the plan's `auto` to `off`.
fn paused_plans(plans: &[PlanView], store: &Store) -> BTreeSet<String> {
    let mut paused = BTreeSet::new();
    for view in plans {
        for task in &view.tasks {
            if let Some(plan) = attempt_ops::paused_plan(store, &task.item.id, task.item.attempt) {
                paused.insert(plan);
            }
        }
    }
    paused
}

/// `12m ago`, `3h ago`, `2d ago`; `now` under a minute, `—` without a record.
fn age_text(age: Option<u64>) -> String {
    match age {
        None => "—".to_owned(),
        Some(seconds) if seconds < 60 => "now".to_owned(),
        Some(seconds) if seconds < 3_600 => format!("{}m ago", seconds / 60),
        Some(seconds) if seconds < 86_400 => format!("{}h ago", seconds / 3_600),
        Some(seconds) => format!("{}d ago", seconds / 86_400),
    }
}

/// The heartbeat age a `doing` card prints: `12s`, `3m`, `2h`, `4d`.
fn short_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

// ---- the frame ------------------------------------------------------------------------

/// Draws the board: the finished list when it is open, else the columns the width allows.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut ArcApp) {
    app.board.layout = BoardLayout::of(area.width);
    if app.board.finished {
        draw_finished(frame, area, app);
        return;
    }
    let theme = app.theme.clone();
    let width = area.width as usize;
    match app.board.layout {
        BoardLayout::Wide => {
            let column_width =
                ((width.saturating_sub((COLUMNS - 1) * COLUMN_GAP as usize)) / COLUMNS).max(1);
            for index in 0..COLUMNS {
                let x = (index * (column_width + COLUMN_GAP as usize)) as u16;
                let rect = Rect {
                    x,
                    y: area.y,
                    width: column_width as u16,
                    height: area.height,
                };
                draw_groups(frame, rect, app, &[index], index, column_width, &theme);
            }
        }
        BoardLayout::Medium => {
            let pane_width = (width.saturating_sub(COLUMN_GAP as usize) / 2).max(1);
            for (pane, first) in [(0usize, TODO), (1usize, VERIFYING)] {
                let x = (pane * (pane_width + COLUMN_GAP as usize)) as u16;
                let rect = Rect {
                    x,
                    y: area.y,
                    width: pane_width as u16,
                    height: area.height,
                };
                draw_groups(
                    frame,
                    rect,
                    app,
                    &[first, first + 1],
                    first,
                    pane_width,
                    &theme,
                );
            }
        }
        BoardLayout::Narrow => {
            draw_groups(
                frame,
                area,
                app,
                &[TODO, DOING, VERIFYING, NEEDS_YOU],
                0,
                width,
                &theme,
            );
        }
    }
}

/// Draws one pane: its groups' lines from the stored window, scrolled to keep the selected
/// card visible. `key` names the column whose `top` the pane keeps.
fn draw_groups(
    frame: &mut Frame,
    rect: Rect,
    app: &mut ArcApp,
    groups: &[usize],
    key: usize,
    width: usize,
    theme: &Theme,
) {
    let mut lines = Vec::new();
    let mut selected_line = None;
    for group in groups {
        let selected = (app.board.column == *group).then_some(app.board.row);
        let (group_lines, selected_at) =
            group_lines(*group, &app.board.columns[*group], width, selected, theme);
        if let Some(at) = selected_at {
            selected_line = Some(lines.len() + at);
        }
        lines.extend(group_lines);
    }
    let visible = rect.height as usize;
    let selected = selected_line
        .map(|line| line.saturating_add(CARD_LINES - 1))
        .unwrap_or(0);
    let top = scroll_top(app.board.top[key], selected, visible, lines.len());
    app.board.top[key] = top;
    let lines: Vec<Line<'static>> = lines.into_iter().skip(top).collect();
    frame.render_widget(Paragraph::new(lines), rect);
}

/// One group's lines: the heading with its count, then each card in three rows and a blank, or
/// the one weak line of an empty group. It returns the line index of the selected card, when
/// the selection is in this group.
fn group_lines(
    group: usize,
    cards: &[BoardCard],
    width: usize,
    selected: Option<usize>,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines = vec![Line::from(vec![
        Span::styled(COLUMN_HEADINGS[group], theme.notice_label),
        Span::styled(format!(" {}", cards.len()), theme.hint),
    ])];
    let mut selected_at = None;
    if cards.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", COLUMN_EMPTY[group]),
            theme.hint,
        )));
        return (lines, None);
    }
    for (row, card) in cards.iter().enumerate() {
        if selected == Some(row) {
            selected_at = Some(lines.len());
        }
        let cursor = selected == Some(row);
        lines.push(card_line(card, width, cursor, theme));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(fit(&card.title, width.saturating_sub(2)), theme.text),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                fit(
                    &format!("{} · {}", card.plan, card.phase),
                    width.saturating_sub(2),
                ),
                theme.hint,
            ),
        ]));
        lines.push(Line::default());
    }
    (lines, selected_at)
}

/// A card's first row: the mark, the id, `#attempt`, the cost, and a `doing` task's heartbeat
/// age.
fn card_line(card: &BoardCard, width: usize, cursor: bool, theme: &Theme) -> Line<'static> {
    let (glyph, style) = if card.needs_you {
        ('!', theme.notice_label)
    } else {
        (card.mark.glyph(), card.mark.style(theme))
    };
    let mut tail = format!("#{} {}", card.attempt, format_cost(card.cost));
    if let Some(beat) = &card.beat {
        tail.push_str(&format!(" · {beat}"));
    }
    let id_width = width.saturating_sub(4 + tail.width() + 1);
    let mut spans = vec![
        if cursor {
            Span::styled("❯ ", theme.model)
        } else {
            Span::raw("  ")
        },
        Span::styled(format!("{glyph} "), style),
        Span::styled(fit(&card.id, id_width), theme.text),
    ];
    let used = card.id.width().min(id_width);
    spans.push(Span::raw(" ".repeat(id_width.saturating_sub(used) + 1)));
    spans.push(Span::styled(tail, theme.tier));
    Line::from(spans)
}

// ---- the finished list ----------------------------------------------------------------

fn draw_finished(frame: &mut Frame, area: Rect, app: &mut ArcApp) {
    let theme = app.theme.clone();
    let width = area.width as usize;
    let rows = finished_lines(app, width, &theme);
    let visible = (area.height as usize).saturating_sub(1);
    let top = scroll_top(app.board.done_top, app.board.done_sel, visible, rows.len());
    app.board.done_top = top;
    let mut lines = vec![Line::from(vec![
        Span::styled("FINISHED", theme.notice_label),
        Span::styled(format!(" {}", rows.len()), theme.hint),
    ])];
    lines.extend(rows.into_iter().skip(top));
    frame.render_widget(Paragraph::new(lines), area);
}

/// One line per finished task, newest first: the mark, the id, the title, the plan and when.
fn finished_lines(app: &ArcApp, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if app.board.done.is_empty() {
        return vec![Line::from(Span::styled("  nothing finished", theme.hint))];
    }
    let id_width = app
        .board
        .done
        .iter()
        .map(|row| row.id.width())
        .max()
        .unwrap_or(16)
        .clamp(12, 34);
    app.board
        .done
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let when = age_text(row.age);
            let tail = format!("{} · {when}", row.plan);
            let fixed = 2 + 2 + id_width + 2 + 2 + tail.width();
            let title_width = width.saturating_sub(fixed);
            let title = fit(&row.title, title_width);
            let used = row.title.width().min(title_width);
            let mut spans = vec![
                if index == app.board.done_sel {
                    Span::styled("❯ ", theme.model)
                } else {
                    Span::raw("  ")
                },
                Span::styled(format!("{} ", row.mark.glyph()), row.mark.style(theme)),
                Span::styled(fit(&row.id, id_width), theme.text),
            ];
            let id_used = row.id.width().min(id_width);
            spans.push(Span::raw(" ".repeat(id_width.saturating_sub(id_used) + 2)));
            spans.push(Span::styled(title, theme.text));
            spans.push(Span::raw(" ".repeat(title_width.saturating_sub(used) + 2)));
            spans.push(Span::styled(tail, theme.hint));
            Line::from(spans)
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The width classes: four readable columns and their gaps, else two, else one.
    #[test]
    fn the_layout_classes_follow_the_readable_column_width() {
        let wide = 4 * MIN_COLUMN_WIDTH + 3 * COLUMN_GAP;
        let medium = 2 * MIN_COLUMN_WIDTH + COLUMN_GAP;
        assert_eq!(BoardLayout::of(wide), BoardLayout::Wide);
        assert_eq!(BoardLayout::of(wide - 1), BoardLayout::Medium);
        assert_eq!(BoardLayout::of(medium), BoardLayout::Medium);
        assert_eq!(BoardLayout::of(medium - 1), BoardLayout::Narrow);
    }

    /// The finished list's when text, and the shorter heartbeat age.
    #[test]
    fn the_age_texts_round_to_their_unit() {
        assert_eq!(age_text(None), "—");
        assert_eq!(age_text(Some(5)), "now");
        assert_eq!(age_text(Some(12 * 60)), "12m ago");
        assert_eq!(age_text(Some(3 * 3_600)), "3h ago");
        assert_eq!(age_text(Some(2 * 86_400)), "2d ago");
        assert_eq!(short_age(12), "12s");
        assert_eq!(short_age(3 * 60), "3m");
        assert_eq!(short_age(5 * 3_600), "5h");
        assert_eq!(short_age(9 * 86_400), "9d");
    }
}

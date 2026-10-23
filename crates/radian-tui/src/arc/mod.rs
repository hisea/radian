//! The arc TUI's plans and task pages (ADR 0048 §1, §13): a **read-only** view of the plans,
//! the tasks, the attempt verdicts and a worker's journal.
//!
//! This module is the state and the frame; [`crate::cli::arc`] owns the terminal and the
//! input loop, as `cli/attach.rs` does for the attach view. Data comes only from
//! `radian_arc`'s read side (`radian_arc::read`) and the store's own loaders; nothing here
//! writes a file, holds a lock, or starts a process. `runner.lock` is **probed** (taken and
//! released at once) so the header can say whether another process is the scheduling loop; a
//! missing lock file is never created.
//!
//! The layout follows ADR 0035: columns 1-2 are the marker column, text starts at column 3,
//! open frames (`╭─ label ───`, `│ `, `╰──`) carry the structure, the cursor is `❯`, and every
//! color is a theme role.

pub mod render;

use std::path::PathBuf;

use radian_arc::read::{AttemptView, TaskDiff};
use radian_arc::store::model::{TaskFile, TaskState};
use radian_arc::store::{Loaded, PlanView, Store};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;

use crate::attach::AttachView;
use crate::theme::Theme;

/// The left plan column's width in cells, from column 3 to the divider.
pub const LEFT_WIDTH: u16 = 35;
/// The left column starts here (columns 1-2 are the marker column, ADR 0035).
pub const LEFT_X: u16 = 2;
/// The divider sits at this cell, and the right column two cells after it.
pub const DIVIDER_X: u16 = LEFT_X + LEFT_WIDTH;
/// The right column's left edge: the divider plus `│  `.
pub const RIGHT_X: u16 = DIVIDER_X + 3;

/// The pages the header names. `Merge` is shown but never active in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Plans,
    Task,
    Merge,
}

impl Page {
    /// The word the header draws.
    pub fn name(self) -> &'static str {
        match self {
            Page::Plans => "plans",
            Page::Task => "task",
            Page::Merge => "merge",
        }
    }
}

/// Which column of the plans page takes Up/Down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Plans,
    Tasks,
}

/// The task page's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTab {
    Conversation,
    Diff,
    Attempts,
}

impl TaskTab {
    pub fn name(self) -> &'static str {
        match self {
            TaskTab::Conversation => "conversation",
            TaskTab::Diff => "diff",
            TaskTab::Attempts => "attempts",
        }
    }

    fn next(self) -> Self {
        match self {
            TaskTab::Conversation => TaskTab::Diff,
            TaskTab::Diff => TaskTab::Attempts,
            TaskTab::Attempts => TaskTab::Conversation,
        }
    }

    fn previous(self) -> Self {
        match self {
            TaskTab::Conversation => TaskTab::Attempts,
            TaskTab::Diff => TaskTab::Conversation,
            TaskTab::Attempts => TaskTab::Diff,
        }
    }
}

/// What one key press did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArcKey {
    /// Leave the terminal.
    Quit,
    /// The frame, or the data to fetch, changed.
    Redraw,
    /// Not a key this page acts on.
    Ignored,
}

/// A task's state mark (ADR 0048's state table, drawn as ADR 0035's marks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// `✓`: `done.merged`.
    Done,
    /// `×`: a `failed` task.
    Failed,
    /// `●`: `doing`, or a `review` phase that is working (verifying, integrating,
    /// ready_to_merge).
    Active,
    /// `!`: `review.awaiting_human` or `review.rework_pending` — the owner or the lead acts.
    Waiting,
    /// `◻`: `todo`.
    Todo,
}

impl Mark {
    /// The glyph.
    pub fn glyph(self) -> char {
        match self {
            Mark::Done => '✓',
            Mark::Failed => '×',
            Mark::Active => '●',
            Mark::Waiting => '!',
            Mark::Todo => '◻',
        }
    }

    /// The theme role the mark and the `state.phase` text take.
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            Mark::Done => theme.status_ok,
            Mark::Failed => theme.status_error,
            Mark::Active => theme.model,
            Mark::Waiting => theme.notice_label,
            Mark::Todo => theme.hint,
        }
    }
}

/// The mark of one task.
pub fn task_mark(state: TaskState, phase: &str) -> Mark {
    match state {
        TaskState::Done => Mark::Done,
        TaskState::Failed => Mark::Failed,
        TaskState::Todo => Mark::Todo,
        TaskState::Doing => Mark::Active,
        TaskState::Review => match phase {
            "awaiting_human" | "rework_pending" => Mark::Waiting,
            _ => Mark::Active,
        },
    }
}

/// One row of a plan's task list, whether drawn as the tree or as the flat list.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub id: String,
    pub phase: String,
    pub attempt: u32,
    pub cost: Option<f64>,
    pub mark: Mark,
    /// The tree connector (`""` for a root, `"├─"`, `"└─"`, or a deeper prefix).
    pub connector: String,
    /// The other parents in this plan (`also after …`).
    pub also_after: Vec<String>,
    /// For a `todo` task: the parents that are not merged yet (`waits …`).
    pub waits: Vec<String>,
    /// The flat list's `after` names (empty in the tree).
    pub after: Vec<String>,
}

/// One plan and the tasks under it, as the left list and the frame need them.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    pub id: String,
    pub status: String,
    pub auto: String,
    pub cost: Option<f64>,
    pub merged: usize,
    pub total: usize,
    pub mark: Mark,
    pub tasks: Vec<TaskRow>,
}

/// The task page's data.
pub struct TaskPage {
    pub id: String,
    pub title: String,
    pub phase: String,
    pub cost: Option<f64>,
    pub plan: String,
    pub tab: TaskTab,
    /// The selected attempt (the latest until the user switches, then the choice sticks
    /// until the task gains a newer attempt).
    pub attempt: Option<u32>,
    pub latest: Option<u32>,
    /// Whether the selection follows a new latest attempt.
    follow_latest: bool,
    pub attempts: Vec<AttemptView>,
    pub journal: Option<AttachView>,
    pub journal_error: Option<String>,
    /// The journal file was looked for (a missing one draws one weak line).
    pub journal_checked: bool,
    pub diff: Option<TaskDiff>,
    /// The `(task, attempt)` a diff is loaded for, or is being loaded for.
    pub diff_key: Option<(String, u32)>,
    /// The request in flight, tied to its key: a reset (an attempt switch, a moved head)
    /// clears it, so a stale reply can never keep the next request from starting.
    pub diff_loading: Option<(String, u32)>,
    pub diff_error: Option<String>,
    pub file_sel: usize,
    pub diff_scroll: usize,
    /// A scroll the runner applies to the conversation view (`j`/`k`), in lines.
    pub pending_scroll: i32,
    /// The runner pins the conversation to its end (`f`).
    pub pending_follow: bool,
    /// The task's `head_sha` when the data was last loaded; a change invalidates the diff.
    head: Option<String>,
}

impl TaskPage {
    fn new(task: &TaskFile, attempts: Vec<AttemptView>) -> Self {
        let attempts = attempts_with_current(attempts, task.attempt);
        let latest = attempts.last().map(|attempt| attempt.attempt);
        let attempt = latest.or((task.attempt > 0).then_some(task.attempt));
        Self {
            id: task.id.clone(),
            title: task.title.clone(),
            phase: format!("{}.{}", task.state.as_str(), task.phase),
            cost: task.cost_usd,
            plan: task.plan.clone(),
            tab: TaskTab::Conversation,
            attempt,
            latest,
            follow_latest: true,
            attempts,
            journal: None,
            journal_error: None,
            journal_checked: false,
            diff: None,
            diff_key: None,
            diff_loading: None,
            diff_error: None,
            file_sel: 0,
            diff_scroll: 0,
            pending_scroll: 0,
            pending_follow: false,
            head: task.head_sha.clone(),
        }
    }

    /// The selected attempt's index in [`TaskPage::attempts`].
    pub fn attempt_index(&self) -> Option<usize> {
        let attempt = self.attempt?;
        self.attempts
            .iter()
            .position(|view| view.attempt == attempt)
    }

    /// The selected file of the diff, if any.
    pub fn selected_file(&self) -> Option<&radian_arc::read::DiffFile> {
        self.diff.as_ref()?.files.get(self.file_sel)
    }

    /// The selected file's diff body rows: exactly the rows the diff frame draws (the
    /// metadata lines are skipped). It does not depend on the width, so the key handlers can
    /// clamp the scroll to the end of the body.
    pub fn diff_body_len(&self) -> usize {
        let Some(file) = self.selected_file() else {
            return 0;
        };
        file.text
            .lines()
            .filter(|line| !is_diff_metadata(line))
            .count()
    }

    fn switch_attempt(&mut self, delta: i32) {
        if self.attempts.is_empty() {
            return;
        }
        let index = self.attempt_index().unwrap_or(self.attempts.len() - 1);
        let next = (index as i32 + delta).clamp(0, self.attempts.len() as i32 - 1) as usize;
        self.attempt = Some(self.attempts[next].attempt);
        self.follow_latest = Some(self.attempts[next].attempt) == self.latest;
        // A diff belongs to one attempt: the next draw asks for the new one, and an in-flight
        // request for the old attempt stops counting as loading.
        self.diff = None;
        self.diff_key = None;
        self.diff_loading = None;
        self.diff_error = None;
        self.diff_scroll = 0;
    }
}

/// The arc TUI's state.
pub struct ArcApp {
    pub store: Store,
    session_dir: Option<PathBuf>,
    pub plans: Vec<PlanView>,
    pub plan_sel: usize,
    pub focus: Focus,
    pub task_sel: usize,
    pub flat: bool,
    pub page: Page,
    pub task: Option<TaskPage>,
    pub main_sha: Option<String>,
    pub loop_running: bool,
    pub theme: Theme,
    /// A store read error, drawn as one weak line instead of a blank page.
    pub notice: Option<String>,
}

impl ArcApp {
    /// Opens the view on `store`; the session directory resolves the worker journals
    /// (`~/.radian/sessions`).
    pub fn open(store: Store) -> Self {
        let session_dir = radian_host::session_dir(None).ok();
        Self::with_session_dir(store, session_dir)
    }

    /// [`ArcApp::open`] with an explicit session directory (tests, and a caller that already
    /// resolved it).
    pub fn with_session_dir(store: Store, session_dir: Option<PathBuf>) -> Self {
        let mut app = Self {
            store,
            session_dir,
            plans: Vec::new(),
            plan_sel: 0,
            focus: Focus::Plans,
            task_sel: 0,
            flat: false,
            page: Page::Plans,
            task: None,
            main_sha: None,
            loop_running: false,
            theme: Theme::default(),
            notice: None,
        };
        app.refresh();
        app
    }

    /// Re-reads the store. On the plans page this re-reads every plan; on the task page it
    /// also re-reads the task, its verdicts and its journal. Returns whether anything the
    /// frame shows may have changed.
    pub fn refresh(&mut self) -> bool {
        self.loop_running = loop_running(&self.store);
        let selected_plan = self
            .plans
            .get(self.plan_sel)
            .map(|view| view.plan.item.id.clone());
        let selected_task = self.selected_task_id();
        match self.store.plans() {
            Ok(plans) => {
                self.plans = plans;
                self.notice = None;
            }
            Err(error) => {
                self.notice = Some(error.message());
                return true;
            }
        }
        if self.plans.is_empty() {
            self.plan_sel = 0;
        } else {
            self.plan_sel = selected_plan
                .and_then(|id| self.plans.iter().position(|view| view.plan.item.id == id))
                .unwrap_or_else(|| self.plan_sel.min(self.plans.len() - 1));
        }
        self.clamp_task_selection(selected_task.as_deref());
        if self.page == Page::Task {
            self.refresh_task_page();
        }
        true
    }

    /// Re-reads the open task page's task, attempts and journal. A task that disappeared (or
    /// became unreadable) closes the page and says why.
    fn refresh_task_page(&mut self) {
        let Some(id) = self.task.as_ref().map(|page| page.id.clone()) else {
            return;
        };
        let loaded = match self.store.load_task(&id) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.notice = Some(error.message());
                self.task = None;
                self.page = Page::Plans;
                return;
            }
        };
        let task = loaded.item;
        let theme = self.theme.clone();
        let attempts = radian_arc::read::task_attempts(&self.store, &id).unwrap_or_default();
        let attempts = attempts_with_current(attempts, task.attempt);
        if let Some(page) = &mut self.task {
            let head_changed = page.head != task.head_sha;
            page.title = task.title.clone();
            page.phase = format!("{}.{}", task.state.as_str(), task.phase);
            page.cost = task.cost_usd;
            page.plan = task.plan.clone();
            page.head = task.head_sha.clone();
            page.latest = attempts.last().map(|attempt| attempt.attempt);
            page.attempts = attempts;
            if page.follow_latest {
                page.attempt = page.latest.or((task.attempt > 0).then_some(task.attempt));
            } else if page
                .attempt
                .is_some_and(|attempt| page.latest.is_some_and(|latest| attempt > latest))
            {
                page.attempt = page.latest;
                page.follow_latest = true;
            }
            if head_changed {
                page.diff = None;
                page.diff_key = None;
                page.diff_loading = None;
                page.diff_error = None;
                page.diff_scroll = 0;
            }
            if let Some(view) = &mut page.journal {
                view.set_theme(theme);
            }
        }
        self.refresh_journal();
    }

    /// Opens, or polls, the selected task's worker journal.
    pub fn refresh_journal(&mut self) {
        let Some(id) = self.task.as_ref().map(|page| page.id.clone()) else {
            return;
        };
        let Some(page) = &mut self.task else {
            return;
        };
        if page.journal.is_some() {
            if let Some(view) = &mut page.journal
                && let Err(error) = view.poll()
            {
                page.journal_error = Some(error);
            }
            return;
        }
        let Some(dir) = self.session_dir.clone() else {
            page.journal_checked = true;
            page.journal_error = Some("no session directory is known".to_owned());
            return;
        };
        let path = dir.join(format!("{id}.jsonl"));
        if !path.is_file() {
            page.journal_checked = true;
            return;
        }
        match AttachView::open(&path, &id) {
            Ok(mut view) => {
                view.set_theme(self.theme.clone());
                page.journal = Some(view);
                page.journal_error = None;
            }
            Err(error) => page.journal_error = Some(error),
        }
        page.journal_checked = true;
    }

    /// The selected plan's view.
    pub fn selected_plan(&self) -> Option<&PlanView> {
        self.plans.get(self.plan_sel)
    }

    /// The selected task, in the selected plan's **display** order: the tree (`g` off) or the
    /// flat list (`g` on). The page draws that order, so the cursor, the moves and this
    /// selection are all one index into it.
    pub fn selected_task(&self) -> Option<&Loaded<TaskFile>> {
        let view = self.selected_plan()?;
        let rows = self.task_rows(view);
        let row = rows.get(self.task_sel.min(rows.len().saturating_sub(1)))?;
        view.tasks.iter().find(|task| task.item.id == row.id)
    }

    fn selected_task_id(&self) -> Option<String> {
        self.selected_task().map(|task| task.item.id.clone())
    }

    fn clamp_task_selection(&mut self, keep: Option<&str>) {
        let rows = self
            .selected_plan()
            .map(|view| self.task_rows(view))
            .unwrap_or_default();
        let index = keep.and_then(|id| rows.iter().position(|row| row.id == id));
        self.task_sel = match index {
            Some(index) => index,
            None => self.task_sel.min(rows.len().saturating_sub(1)),
        };
    }

    /// Every plan as the page draws it, in the store's order.
    pub fn plan_rows(&self) -> Vec<PlanRow> {
        self.plans
            .iter()
            .map(|view| plan_row(view, self.task_rows(view)))
            .collect()
    }

    /// One plan's tasks: the dependency tree, or the flat list with `after`.
    pub fn task_rows(&self, view: &PlanView) -> Vec<TaskRow> {
        if self.flat {
            return flat_rows(view);
        }
        tree_rows(view)
    }

    /// Opens the selected task's page.
    pub fn open_task(&mut self) {
        let Some(view) = self.selected_plan() else {
            return;
        };
        let rows = self.task_rows(view);
        let index = self.task_sel.min(rows.len().saturating_sub(1));
        let Some(row) = rows.get(index) else {
            return;
        };
        let Some(task) = view.tasks.iter().find(|task| task.item.id == row.id) else {
            return;
        };
        let id = task.item.id.clone();
        let attempts = radian_arc::read::task_attempts(&self.store, &id).unwrap_or_default();
        self.task = Some(TaskPage::new(&task.item, attempts));
        self.page = Page::Task;
        self.refresh_journal();
    }

    /// Leaves the task page.
    pub fn close_task(&mut self) {
        self.page = Page::Plans;
        self.task = None;
    }

    /// The diff the runner should fetch now, if any: the open task page's selected attempt
    /// has no diff, and no request for that same attempt is in flight.
    pub fn diff_request(&self) -> Option<(String, u32)> {
        let page = self.task.as_ref()?;
        if self.page != Page::Task || page.tab != TaskTab::Diff {
            return None;
        }
        let key = (page.id.clone(), page.attempt.unwrap_or(0));
        if page.diff_key.as_ref() == Some(&key) || page.diff_loading.as_ref() == Some(&key) {
            return None;
        }
        Some(key)
    }

    /// Records the diff a worker read. A reply for another task or attempt is dropped, and
    /// only the request it belongs to stops being in flight.
    pub fn apply_diff(&mut self, id: &str, attempt: u32, result: Result<TaskDiff, String>) -> bool {
        let Some(page) = &mut self.task else {
            return false;
        };
        if page.id != id || page.attempt.unwrap_or(0) != attempt {
            return false;
        }
        let key = (id.to_owned(), attempt);
        if page.diff_loading.as_ref() == Some(&key) {
            page.diff_loading = None;
        }
        page.diff_key = Some(key);
        match result {
            Ok(diff) => {
                page.diff_error = None;
                let count = diff.files.len();
                page.diff = Some(diff);
                page.file_sel = page.file_sel.min(count.saturating_sub(1));
            }
            Err(error) => page.diff_error = Some(error),
        }
        true
    }

    /// Marks the request `key` as in flight, so no second request for it is started.
    pub fn mark_diff_loading(&mut self, key: (String, u32)) {
        if let Some(page) = &mut self.task {
            page.diff_loading = Some(key);
        }
    }

    /// Applies the keys that need the measured transcript: a `j`/`k` scroll and an `f`
    /// follow. The runner calls this before the frame; the page state itself owns no viewport.
    pub fn apply_pending_scroll(&mut self) {
        let Some(page) = &mut self.task else {
            return;
        };
        if let Some(view) = &mut page.journal {
            if page.pending_scroll != 0 {
                view.scroll_lines(page.pending_scroll);
                page.pending_scroll = 0;
            }
            if page.pending_follow {
                view.follow_end();
                page.pending_follow = false;
            }
        } else {
            page.pending_scroll = 0;
            page.pending_follow = false;
        }
    }

    /// Sets the header's remote-main sha (the git worker's reply).
    pub fn apply_main(&mut self, sha: Option<String>) {
        self.main_sha = sha;
    }

    /// One key press.
    pub fn on_key(&mut self, key: &KeyEvent) -> ArcKey {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if !ctrl => ArcKey::Quit,
            KeyCode::Char('c') if ctrl => ArcKey::Quit,
            _ => match self.page {
                Page::Plans => self.on_plans_key(key),
                Page::Task => self.on_task_key(key),
                Page::Merge => ArcKey::Ignored,
            },
        }
    }

    fn on_plans_key(&mut self, key: &KeyEvent) -> ArcKey {
        match key.code {
            KeyCode::Esc => ArcKey::Quit,
            KeyCode::Tab => {
                if self.selected_task().is_some() {
                    self.open_task();
                    ArcKey::Redraw
                } else {
                    ArcKey::Ignored
                }
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Right => {
                if self.focus == Focus::Plans && self.selected_task().is_some() {
                    self.focus = Focus::Tasks;
                    ArcKey::Redraw
                } else {
                    ArcKey::Ignored
                }
            }
            KeyCode::Left => {
                if self.focus == Focus::Tasks {
                    self.focus = Focus::Plans;
                    ArcKey::Redraw
                } else {
                    ArcKey::Ignored
                }
            }
            KeyCode::Char('g') => {
                let keep = self.selected_task_id();
                self.flat = !self.flat;
                self.clamp_task_selection(keep.as_deref());
                ArcKey::Redraw
            }
            KeyCode::Enter => {
                if self.selected_task().is_some() {
                    self.open_task();
                    ArcKey::Redraw
                } else {
                    ArcKey::Ignored
                }
            }
            _ => ArcKey::Ignored,
        }
    }

    fn move_selection(&mut self, delta: i32) -> ArcKey {
        match self.focus {
            Focus::Plans => {
                if self.plans.is_empty() {
                    return ArcKey::Ignored;
                }
                let last = self.plans.len() - 1;
                self.plan_sel = (self.plan_sel as i32 + delta).clamp(0, last as i32) as usize;
                self.task_sel = 0;
                ArcKey::Redraw
            }
            Focus::Tasks => {
                let count = self
                    .selected_plan()
                    .map(|view| self.task_rows(view).len())
                    .unwrap_or(0);
                if count == 0 {
                    return ArcKey::Ignored;
                }
                self.task_sel = (self.task_sel as i32 + delta).clamp(0, count as i32 - 1) as usize;
                ArcKey::Redraw
            }
        }
    }

    fn on_task_key(&mut self, key: &KeyEvent) -> ArcKey {
        let Some(page) = &mut self.task else {
            return ArcKey::Ignored;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.close_task();
                ArcKey::Redraw
            }
            KeyCode::Char('[') => {
                page.switch_attempt(-1);
                ArcKey::Redraw
            }
            KeyCode::Char(']') => {
                page.switch_attempt(1);
                ArcKey::Redraw
            }
            KeyCode::Char('h') | KeyCode::Left => {
                page.tab = page.tab.previous();
                ArcKey::Redraw
            }
            KeyCode::Char('l') | KeyCode::Right => {
                page.tab = page.tab.next();
                ArcKey::Redraw
            }
            KeyCode::Char('j') | KeyCode::Down => match page.tab {
                TaskTab::Diff => {
                    let end = page.diff_body_len().saturating_sub(1);
                    page.diff_scroll = page.diff_scroll.saturating_add(1).min(end);
                    ArcKey::Redraw
                }
                TaskTab::Conversation => {
                    page.pending_scroll += 1;
                    ArcKey::Redraw
                }
                TaskTab::Attempts => ArcKey::Ignored,
            },
            KeyCode::Char('k') | KeyCode::Up => match page.tab {
                TaskTab::Diff => {
                    page.diff_scroll = page.diff_scroll.saturating_sub(1);
                    ArcKey::Redraw
                }
                TaskTab::Conversation => {
                    page.pending_scroll -= 1;
                    ArcKey::Redraw
                }
                TaskTab::Attempts => ArcKey::Ignored,
            },
            KeyCode::Char('f') => {
                match page.tab {
                    // Follow the end means the last body row is the first visible one; it is
                    // stored, so `k` moves up from there instead of from a sentinel.
                    TaskTab::Diff => page.diff_scroll = page.diff_body_len().saturating_sub(1),
                    TaskTab::Conversation => page.pending_follow = true,
                    TaskTab::Attempts => {}
                }
                ArcKey::Redraw
            }
            KeyCode::Char('n') => {
                if page.tab == TaskTab::Diff {
                    let count = page.diff.as_ref().map(|diff| diff.files.len()).unwrap_or(0);
                    if count > 0 {
                        page.file_sel = (page.file_sel + 1).min(count - 1);
                        page.diff_scroll = 0;
                    }
                }
                ArcKey::Redraw
            }
            KeyCode::Char('p') => {
                if page.tab == TaskTab::Diff {
                    page.file_sel = page.file_sel.saturating_sub(1);
                    page.diff_scroll = 0;
                }
                ArcKey::Redraw
            }
            _ => ArcKey::Ignored,
        }
    }
}

/// The attempt rows a page shows: the run directories' attempts, or — for a task whose run
/// directories are missing — the attempts the task file itself claims (`1..=attempt`), each
/// with no verdicts. The selector and the attempts tab then agree with `attempt` in the task.
fn attempts_with_current(attempts: Vec<AttemptView>, attempt: u32) -> Vec<AttemptView> {
    if !attempts.is_empty() || attempt == 0 {
        return attempts;
    }
    (1..=attempt)
        .map(|number| AttemptView {
            attempt: number,
            verdicts: Vec::new(),
        })
        .collect()
}

/// Whether another process holds `runner.lock` (ADR 0048 §13). The lock is taken and released
/// at once: the view never holds it. A repository with no `runner.lock` has no loop, and the
/// probe never creates the file.
pub fn loop_running(store: &Store) -> bool {
    let path = store.root().join(radian_arc::store::RUNNER_LOCK);
    if !path.is_file() {
        return false;
    }
    match store.try_runner_lock() {
        Ok(Some(guard)) => {
            drop(guard);
            false
        }
        Ok(None) => true,
        Err(_) => false,
    }
}

/// A plan as the page draws it: the summary line and its tasks.
fn plan_row(view: &PlanView, tasks: Vec<TaskRow>) -> PlanRow {
    let total = view.plan.item.tasks.len();
    let merged = view
        .tasks
        .iter()
        .filter(|task| task.item.state == TaskState::Done && task.item.phase == "merged")
        .count();
    let mark = if total > 0 && merged == total {
        Mark::Done
    } else if view.plan.item.status == "draft" {
        Mark::Todo
    } else {
        Mark::Active
    };
    let costs: Vec<f64> = view
        .tasks
        .iter()
        .filter_map(|task| task.item.cost_usd)
        .collect();
    let cost = (!costs.is_empty()).then(|| costs.iter().sum::<f64>());
    PlanRow {
        id: view.plan.item.id.clone(),
        status: view.plan.item.status.clone(),
        auto: view.plan.item.auto.clone(),
        cost,
        merged,
        total,
        mark,
        tasks,
    }
}

/// Every task in plan order, with `after` named on the row.
fn flat_rows(view: &PlanView) -> Vec<TaskRow> {
    view.tasks
        .iter()
        .map(|task| {
            let mut row = row(&task.item, String::new(), Vec::new(), Vec::new());
            row.after = task
                .item
                .after
                .iter()
                .filter(|parent| {
                    view.tasks
                        .iter()
                        .any(|candidate| candidate.item.id == **parent)
                })
                .cloned()
                .collect();
            row
        })
        .collect()
}

/// The dependency tree: a task with a parent in this plan sits under its first such parent;
/// roots keep the plan's order. Every task appears once.
fn tree_rows(view: &PlanView) -> Vec<TaskRow> {
    let mut out = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    for index in 0..view.tasks.len() {
        if first_parent(view, index).is_none() {
            walk(view, index, "", true, true, &mut visited, &mut out);
        }
    }
    // A plan with a cycle has no root on it: name the rest in plan order so a task is never
    // hidden. The cycle itself stays visible through `after` (the flat list) or `also after`.
    for index in 0..view.tasks.len() {
        if !visited.contains(&view.tasks[index].item.id) {
            walk(view, index, "", true, true, &mut visited, &mut out);
        }
    }
    out
}

fn walk(
    view: &PlanView,
    index: usize,
    prefix: &str,
    root: bool,
    last: bool,
    visited: &mut Vec<String>,
    out: &mut Vec<TaskRow>,
) {
    let task = &view.tasks[index];
    if visited.contains(&task.item.id) {
        return;
    }
    visited.push(task.item.id.clone());
    let (also_after, waits) = dependencies(view, &task.item);
    let connector = if root {
        String::new()
    } else if last {
        format!("{prefix}└─")
    } else {
        format!("{prefix}├─")
    };
    out.push(row(&task.item, connector, also_after, waits));
    let children: Vec<usize> = (0..view.tasks.len())
        .filter(|&candidate| first_parent(view, candidate) == Some(index))
        .collect();
    let child_prefix = if root {
        String::new()
    } else if last {
        format!("{prefix}  ")
    } else {
        format!("{prefix}│ ")
    };
    let count = children.len();
    for (position, child) in children.into_iter().enumerate() {
        walk(
            view,
            child,
            &child_prefix,
            false,
            position + 1 == count,
            visited,
            out,
        );
    }
}

/// The index of the task's first `after` entry that is a task of this plan.
fn first_parent(view: &PlanView, index: usize) -> Option<usize> {
    let task = &view.tasks[index];
    task.item.after.iter().find_map(|parent| {
        view.tasks
            .iter()
            .position(|candidate| candidate.item.id == *parent && candidate.item.id != task.item.id)
    })
}

/// The other parents of a task in this plan, and the parents not merged yet.
fn dependencies(view: &PlanView, task: &TaskFile) -> (Vec<String>, Vec<String>) {
    let mut parents = Vec::new();
    for parent in &task.after {
        let found = view
            .tasks
            .iter()
            .find(|candidate| candidate.item.id == *parent && candidate.item.id != task.id);
        if let Some(found) = found {
            parents.push((
                found.item.id.clone(),
                found.item.state == TaskState::Done && found.item.phase == "merged",
            ));
        }
    }
    let also_after = parents
        .iter()
        .skip(1)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let waits = if task.state == TaskState::Todo {
        parents
            .iter()
            .filter(|(_, merged)| !merged)
            .map(|(id, _)| id.clone())
            .collect()
    } else {
        Vec::new()
    };
    (also_after, waits)
}

fn row(task: &TaskFile, connector: String, also_after: Vec<String>, waits: Vec<String>) -> TaskRow {
    TaskRow {
        id: task.id.clone(),
        phase: format!("{}.{}", task.state.as_str(), task.phase),
        attempt: task.attempt,
        cost: task.cost_usd,
        mark: task_mark(task.state, &task.phase),
        connector,
        also_after,
        waits,
        after: Vec::new(),
    }
}

/// A diff line the task page does not draw: the `diff --git` line, the index line and the
/// old/new file headers. The path is the diff frame's title instead.
pub fn is_diff_metadata(line: &str) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
}

/// One cost, as the pages print it: `$1.23`, or `—` when nothing was recorded.
pub fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(cost) => format!("${cost:.2}"),
        None => "—".to_owned(),
    }
}

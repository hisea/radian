//! The arc TUI's plans, task and merge pages (ADR 0048 §1, §2, §11 and §13): a view of the
//! plans, the tasks, the attempt verdicts, a worker's journal and the gate's evidence for one
//! task, plus the owner's actions.
//!
//! This module is the state and the frame; [`crate::cli::arc`] owns the terminal and the
//! input loop, as `cli/attach.rs` does for the attach view. Data comes only from
//! `radian_arc`'s read side (`radian_arc::read`), the store's own loaders and a command's log
//! (queued here, read by the runner on a worker thread); nothing here writes a file, holds a
//! lock, or starts a process. An owner's action is queued here and executed by the runner
//! through [`actions`], in this process, with the library calls the CLI makes. `runner.lock` is **probed** (taken and released at once) so the header can say
//! whether another process is the scheduling loop; a missing lock file is never created.
//!
//! The layout follows ADR 0035: columns 1-2 are the marker column, text starts at column 3,
//! open frames (`╭─ label ───`, `│ `, `╰──`) carry the structure, the cursor is `❯`, and every
//! color is a theme role.

pub mod actions;
pub mod board;
pub mod render;

use std::path::{Path, PathBuf};
use std::time::Instant;

use radian_arc::read::{AttemptView, CommandView, MergeView, TaskDiff};
use radian_arc::store::model::{
    PLAN_PRIORITIES, PROPOSED_PHASE, TaskFile, TaskState, claim_order, priority_rank,
};
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

/// The pages the header names. `Board` groups the open tasks by who acts next; `Merge` is
/// enabled for a task in `review.*` or `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Plans,
    Board,
    Task,
    Merge,
}

impl Page {
    /// The word the header draws.
    pub fn name(self) -> &'static str {
        match self {
            Page::Plans => "plans",
            Page::Board => "board",
            Page::Task => "task",
            Page::Merge => "merge",
        }
    }
}

/// Which plans the plans page lists. `f` cycles them in this order; the view is not saved,
/// so every start opens on [`PlanFilter::Active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanFilter {
    /// The plans `radian arc list` prints without `--all`: not cancelled and not finished.
    #[default]
    Active,
    /// Every plan.
    All,
    /// The finished plans only: every task `done.merged`, and not cancelled.
    Done,
    /// The cancelled plans only.
    Cancelled,
}

impl PlanFilter {
    /// The word the header draws.
    pub fn name(self) -> &'static str {
        match self {
            PlanFilter::Active => "active",
            PlanFilter::All => "all",
            PlanFilter::Done => "done",
            PlanFilter::Cancelled => "cancelled",
        }
    }

    /// The next view in the `f` cycle: `active → all → done → cancelled → active`.
    pub fn next(self) -> Self {
        match self {
            PlanFilter::Active => PlanFilter::All,
            PlanFilter::All => PlanFilter::Done,
            PlanFilter::Done => PlanFilter::Cancelled,
            PlanFilter::Cancelled => PlanFilter::Active,
        }
    }

    /// Whether this view lists `view`. The "finished" test is the one `radian arc list` uses
    /// ([`radian_arc::store::PlanView::is_finished`]); a plan with no tasks is active.
    pub fn shows(self, view: &PlanView) -> bool {
        match self {
            PlanFilter::Active => !view.plan.item.is_cancelled() && !view.is_finished(),
            PlanFilter::All => true,
            PlanFilter::Done => view.is_finished(),
            PlanFilter::Cancelled => view.plan.item.is_cancelled(),
        }
    }

    /// The one weak line the plan column shows when this view holds no plan.
    pub fn empty_text(self) -> &'static str {
        match self {
            PlanFilter::Active => "no active plans",
            PlanFilter::All => "no plans",
            PlanFilter::Done => "no done plans",
            PlanFilter::Cancelled => "no cancelled plans",
        }
    }
}

/// Which column of the plans page takes Up/Down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Plans,
    Tasks,
}

/// The task page's tabs, in cycle order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTab {
    /// The task file's body (the brief), Markdown as the store loads it.
    Brief,
    Conversation,
    Diff,
    Attempts,
}

impl TaskTab {
    pub fn name(self) -> &'static str {
        match self {
            TaskTab::Brief => "brief",
            TaskTab::Conversation => "conversation",
            TaskTab::Diff => "diff",
            TaskTab::Attempts => "attempts",
        }
    }

    fn next(self) -> Self {
        match self {
            TaskTab::Brief => TaskTab::Conversation,
            TaskTab::Conversation => TaskTab::Diff,
            TaskTab::Diff => TaskTab::Attempts,
            TaskTab::Attempts => TaskTab::Brief,
        }
    }

    fn previous(self) -> Self {
        match self {
            TaskTab::Brief => TaskTab::Attempts,
            TaskTab::Conversation => TaskTab::Brief,
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

/// The mark of one plan (ADR 0048 decision 11): the circle family, one per plan, the first
/// rule that holds. A plan no rule matches draws [`PlanMark::Queued`]. The task marks stay
/// [`Mark`]; these are only the plan list's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMark {
    /// `✕`: the plan is `cancelled`. The rest of the row is weak.
    Cancelled,
    /// `◌`: the plan is `draft`.
    Draft,
    /// `◉`: a task is `review.awaiting_human` or `todo.proposed`; or no task is open and a
    /// task is `failed.*` other than `failed.cancelled`.
    NeedsYou,
    /// `●`: a task is `doing.*`, or a `review.*` phase other than `awaiting_human`.
    Running,
    /// `◍`: `auto` is `off` and a task is open.
    Paused,
    /// `○`: a task is open, and nothing is running.
    Queued,
    /// `✓`: every task is `done.*` or `failed.cancelled`, and at least one is `done`.
    Done,
}

impl PlanMark {
    /// The glyph.
    pub fn glyph(self) -> char {
        match self {
            PlanMark::Cancelled => '✕',
            PlanMark::Draft => '◌',
            PlanMark::NeedsYou => '◉',
            PlanMark::Running => '●',
            PlanMark::Paused => '◍',
            PlanMark::Queued => '○',
            PlanMark::Done => '✓',
        }
    }

    /// The theme role the mark takes.
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            PlanMark::Cancelled => theme.plan_cancelled,
            PlanMark::Draft => theme.plan_draft,
            PlanMark::NeedsYou => theme.plan_needs_you,
            PlanMark::Running => theme.plan_running,
            PlanMark::Paused => theme.plan_paused,
            PlanMark::Queued => theme.plan_queued,
            PlanMark::Done => theme.plan_done,
        }
    }
}

/// The mark of one plan: the first rule of ADR 0048 decision 11's table that holds.
pub fn plan_mark(view: &PlanView) -> PlanMark {
    let plan = &view.plan.item;
    if plan.status == "cancelled" {
        return PlanMark::Cancelled;
    }
    if plan.status == "draft" {
        return PlanMark::Draft;
    }
    let tasks = &view.tasks;
    let needs_you = tasks.iter().any(|task| {
        (task.item.state == TaskState::Review && task.item.phase == "awaiting_human")
            || (task.item.state == TaskState::Todo && task.item.phase == PROPOSED_PHASE)
    });
    let open = tasks.iter().any(|task| task_open(&task.item));
    let failed_other = tasks
        .iter()
        .any(|task| task.item.state == TaskState::Failed && task.item.phase != "cancelled");
    if needs_you || (!open && failed_other) {
        return PlanMark::NeedsYou;
    }
    let running = tasks.iter().any(|task| {
        task.item.state == TaskState::Doing
            || (task.item.state == TaskState::Review && task.item.phase != "awaiting_human")
    });
    if running {
        return PlanMark::Running;
    }
    if open && plan.auto == "off" {
        return PlanMark::Paused;
    }
    if open {
        return PlanMark::Queued;
    }
    let all_finished = tasks.iter().all(|task| {
        task.item.state == TaskState::Done
            || (task.item.state == TaskState::Failed && task.item.phase == "cancelled")
    });
    let any_done = tasks.iter().any(|task| task.item.state == TaskState::Done);
    if all_finished && any_done {
        return PlanMark::Done;
    }
    PlanMark::Queued
}

/// Whether a task is open for the plan mark's rules (ADR 0048 decision 11): `todo.ready`,
/// `todo.retry_wait`, `doing.*` or `review.*`. `todo.proposed` has its own rule (needs you),
/// and `todo.blocked` and `todo.paused_budget` are not open.
fn task_open(task: &TaskFile) -> bool {
    match task.state {
        TaskState::Todo => matches!(task.phase.as_str(), "ready" | "retry_wait"),
        TaskState::Doing | TaskState::Review => true,
        TaskState::Done | TaskState::Failed => false,
    }
}

/// How alive a `doing` task's worker looks (ADR 0048 decisions 7.9 and 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The wrapper has written no heartbeat yet.
    Starting,
    /// The last beat was `age` seconds ago, inside the attempt's stall window.
    Beating { age: u64 },
    /// The last beat is older than the attempt's stall window.
    Stalled { age: u64 },
}

/// One row of a plan's task list, whether drawn as the tree or as the flat list.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub id: String,
    pub phase: String,
    pub attempt: u32,
    pub cost: Option<f64>,
    pub mark: Mark,
    /// For a `doing` task: whether its worker is alive, from the wrapper's heartbeat.
    pub freshness: Option<Freshness>,
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
    pub priority: String,
    pub auto: String,
    pub cost: Option<f64>,
    pub merged: usize,
    pub total: usize,
    pub mark: PlanMark,
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
    /// The task file's body: the brief, rework sections included, without the hand-off a
    /// fresh attempt's stdin adds.
    pub brief: String,
    /// The first line of the brief the tab draws. The window is clamped in the frame, where
    /// the wrapped line count is known.
    pub brief_scroll: usize,
    /// The runner pins the brief to its end (`f`) at the next frame.
    pub brief_follow: bool,
    /// The selected attempt (the latest until the user switches, then the choice sticks
    /// until the task gains a newer attempt).
    pub attempt: Option<u32>,
    pub latest: Option<u32>,
    /// The first attempt row the attempts tab draws: the list scrolls only when the selection
    /// leaves the visible rows.
    pub attempt_top: usize,
    /// Whether the selection follows a new latest attempt.
    follow_latest: bool,
    pub attempts: Vec<AttemptView>,
    pub journal: Option<AttachView>,
    pub journal_error: Option<String>,
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
            // A task that has not run opens on its brief; the owner reads why it exists
            // before its (empty) conversation. A task with an attempt opens on the
            // conversation, as it always has.
            tab: if task.attempt == 0 {
                TaskTab::Brief
            } else {
                TaskTab::Conversation
            },
            brief: task.body.clone(),
            brief_scroll: 0,
            brief_follow: false,
            attempt,
            latest,
            attempt_top: 0,
            follow_latest: true,
            attempts,
            journal: None,
            journal_error: None,
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

/// A command's log the runner should read on a worker thread: a log can be a whole test
/// run, and a frame never waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    /// The merge page's task.
    pub task: String,
    /// The command line, as the float's title.
    pub title: String,
    pub path: PathBuf,
}

/// The most bytes of a command's log the TUI reads: its tail, which is where a failure is.
/// Bounded so a log that is one giant file never loads whole into memory.
pub const LOG_BYTES: u64 = 512 * 1024;

/// Reads the last [`LOG_BYTES`] of `path` as text, dropping the line the cut may have split.
///
/// A cut that lands exactly on a line start keeps that first line whole: only a cut inside a
/// line drops through the newline that ends it.
///
/// # Errors
///
/// The file cannot be opened, seeked or read.
pub fn read_log_tail(path: &Path) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let len = file.metadata().map_err(|error| error.to_string())?.len();
    let cut = len.saturating_sub(LOG_BYTES);
    // The byte before the cut says whether the cut split a line or landed on its start.
    let at_line_start = if cut > 0 {
        file.seek(SeekFrom::Start(cut - 1))
            .map_err(|error| error.to_string())?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)
            .map_err(|error| error.to_string())?;
        previous[0] == b'\n'
    } else {
        false
    };
    if cut > 0 {
        file.seek(SeekFrom::Start(cut))
            .map_err(|error| error.to_string())?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if cut == 0 || at_line_start {
        return Ok(text);
    }
    // The first line of a tail starts mid-line; the next newline ends it.
    match text.find('\n') {
        Some(newline) => Ok(text[newline + 1..].to_owned()),
        None => Ok(text),
    }
}

/// One command's log in the merge page's scrollable float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogView {
    /// The command line, as the float's title.
    pub title: String,
    /// The log's lines. A very long log keeps its last [`LOG_LINES`].
    pub lines: Vec<String>,
    pub scroll: usize,
}

/// The most log lines a float keeps: enough to read a failure, bounded so a giant log never
/// grows the view without limit.
pub const LOG_LINES: usize = 5_000;

impl LogView {
    /// Opens `text` as the log of `title`, keeping its last [`LOG_LINES`] lines.
    pub fn new(title: impl Into<String>, text: &str) -> Self {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        if lines.is_empty() {
            lines.push("(the log is empty)".to_owned());
        }
        if lines.len() > LOG_LINES {
            lines.drain(..lines.len() - LOG_LINES);
        }
        Self {
            title: title.into(),
            lines,
            scroll: 0,
        }
    }

    /// Moves the visible window by `delta` lines, clamped to the end.
    pub fn scroll(&mut self, delta: i32) {
        let target = (self.scroll as i32).saturating_add(delta);
        self.scroll = target.clamp(0, self.lines.len().saturating_sub(1) as i32) as usize;
    }
}

/// A float on the merge page: it takes the keys until it closes (ADR 0035: floats only).
#[derive(Debug, Clone, PartialEq)]
pub enum Float {
    /// `merge TK… onto main? y/n`, carrying the action `y` runs.
    Confirm {
        message: String,
        action: actions::Action,
    },
    /// The reject note, one line.
    Reject { note: String },
}

/// The merge page's state: the task's stage rows, the review's findings, the commits, the
/// task's diff, a command's log and the float on top.
pub struct MergePage {
    pub id: String,
    /// The attempt the rows belong to (the task's current one).
    pub attempt: u32,
    /// The loaded view; `None` while the first read is in flight.
    pub view: Option<MergeView>,
    /// Why the view could not be read.
    pub error: Option<String>,
    /// The read in flight.
    pub loading: bool,
    /// The refresh interval asked for a new read.
    pub reload: bool,
    /// The selected command row across every stage.
    pub command_sel: usize,
    pub log: Option<LogView>,
    pub diff: Option<TaskDiff>,
    pub diff_key: Option<(String, u32)>,
    pub diff_loading: Option<(String, u32)>,
    pub diff_error: Option<String>,
    pub file_sel: usize,
    pub diff_scroll: usize,
}

impl MergePage {
    fn new(id: String, attempt: u32) -> Self {
        Self {
            id,
            attempt,
            view: None,
            error: None,
            loading: false,
            reload: true,
            command_sel: 0,
            log: None,
            diff: None,
            diff_key: None,
            diff_loading: None,
            diff_error: None,
            file_sel: 0,
            diff_scroll: 0,
        }
    }

    /// Every command row, in the order the page draws them.
    pub fn command_rows(&self) -> Vec<&CommandView> {
        self.view
            .as_ref()
            .map(|view| {
                view.stages
                    .iter()
                    .flat_map(|stage| stage.commands.iter())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The selected command row, when there is one.
    pub fn selected_command(&self) -> Option<&CommandView> {
        self.command_rows().get(self.command_sel).copied()
    }

    /// The selected file of the diff, if any.
    pub fn selected_file(&self) -> Option<&radian_arc::read::DiffFile> {
        self.diff.as_ref()?.files.get(self.file_sel)
    }

    /// The selected file's diff body rows: the rows the diff frame draws.
    pub fn diff_body_len(&self) -> usize {
        let Some(file) = self.selected_file() else {
            return 0;
        };
        file.text
            .lines()
            .filter(|line| !is_diff_metadata(line))
            .count()
    }
}

/// One action the runner is executing: the footer's spinner runs until its reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub label: String,
    pub started: Instant,
}

/// The footer's result line: the CLI's line, or its error text and exit reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultLine {
    pub text: String,
    pub failed: bool,
}

/// The arc TUI's state.
pub struct ArcApp {
    pub store: Store,
    session_dir: Option<PathBuf>,
    /// The plans the current view lists, in the store's order. `all_plans` holds every plan,
    /// for the hidden count and a task's `after` lookup.
    pub plans: Vec<PlanView>,
    /// Every plan, whatever the view.
    all_plans: Vec<PlanView>,
    /// How many plans the current view hides.
    pub hidden: usize,
    /// The plans page's view; `f` cycles it, and it is not saved.
    pub filter: PlanFilter,
    pub plan_sel: usize,
    pub focus: Focus,
    pub task_sel: usize,
    /// The first plan drawn in the list: the list scrolls only when the selection leaves the
    /// visible rows.
    pub plan_top: usize,
    /// The first task drawn in the selected plan's frame, with the same rule.
    pub task_top: usize,
    pub flat: bool,
    pub page: Page,
    /// The board page's columns, selection, scope and finished list.
    pub board: board::BoardPage,
    pub task: Option<TaskPage>,
    pub merge: Option<MergePage>,
    /// The page `Esc`/`Tab` return to from the task page, and the one the merge page opened
    /// over.
    task_return: Page,
    merge_return: Page,
    /// The action the runner should execute.
    action: Option<actions::Action>,
    /// The action the runner is executing.
    pub pending: Option<Pending>,
    /// The last action's line, shown until the next key.
    pub result: Option<ResultLine>,
    /// The command log the runner should read, off the UI thread.
    log_request: Option<LogRequest>,
    /// The log the runner is reading: one at a time, and a reply for anything else is stale.
    log_pending: Option<LogRequest>,
    /// The confirmation or reject float, which takes the keys until it closes.
    pub float: Option<Float>,
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
            all_plans: Vec::new(),
            hidden: 0,
            filter: PlanFilter::default(),
            plan_sel: 0,
            focus: Focus::Plans,
            task_sel: 0,
            plan_top: 0,
            task_top: 0,
            flat: false,
            page: Page::Plans,
            board: board::BoardPage::default(),
            task: None,
            merge: None,
            task_return: Page::Plans,
            merge_return: Page::Plans,
            action: None,
            pending: None,
            result: None,
            log_request: None,
            log_pending: None,
            float: None,
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
        let selected_plan = self.selected_plan().map(|view| view.plan.item.id.clone());
        let selected_task = self.selected_task_id();
        match self.store.plans() {
            Ok(plans) => {
                self.all_plans = plans;
                self.notice = None;
            }
            Err(error) => {
                self.notice = Some(error.message());
                return true;
            }
        }
        self.rebuild_visible(selected_plan.as_deref());
        // The view is kept, so a plan can leave it during a re-read: when the selected plan
        // changed, the task selection resets; otherwise the selected task stays.
        if self.selected_plan().map(|view| view.plan.item.id.clone()) != selected_plan {
            self.task_sel = 0;
            self.task_top = 0;
        } else {
            self.clamp_task_selection(selected_task.as_deref());
        }
        if self.page == Page::Task {
            self.refresh_task_page();
        }
        if self.page == Page::Board {
            self.rebuild_board();
        }
        if self.page == Page::Merge
            && let Some(page) = &mut self.merge
        {
            page.reload = true;
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
            page.brief = task.body.clone();
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

    /// Opens, or polls, the selected attempt's worker journal.
    ///
    /// The file is named by the attempt's session id (ADR 0048 decision 4): the wrapper's
    /// `execution.json` or `identity.json`, else the claim's `start.json`. Only an attempt that
    /// names none (an older attempt, or a worker-less `reverify`) falls back to the task id. A
    /// view opened for another attempt is dropped, so `[`/`]` never keeps showing the previous
    /// conversation.
    pub fn refresh_journal(&mut self) {
        let Some(page) = self.task.as_ref() else {
            return;
        };
        let id = page.id.clone();
        let attempt = page.attempt;
        let session = match attempt.and_then(|attempt| {
            radian_arc::store::attempt_ops::attempt_session_id(&self.store, &id, attempt)
        }) {
            Some(session) => session,
            None => id.clone(),
        };
        let Some(page) = self.task.as_mut() else {
            return;
        };
        if page
            .journal
            .as_ref()
            .is_some_and(|view| view.id() == session.as_str())
        {
            if let Some(view) = &mut page.journal
                && let Err(error) = view.poll()
            {
                page.journal_error = Some(error);
            }
            return;
        }
        // No view yet, or one for another attempt: start over from the selected session.
        page.journal = None;
        page.journal_error = None;
        let Some(dir) = self.session_dir.clone() else {
            page.journal_error = Some("no session directory is known".to_owned());
            return;
        };
        let path = dir.join(format!("{session}.jsonl"));
        if !path.is_file() {
            return;
        }
        match AttachView::open(&path, &session) {
            Ok(mut view) => {
                view.set_theme(self.theme.clone());
                page.journal = Some(view);
                page.journal_error = None;
            }
            Err(error) => page.journal_error = Some(error),
        }
    }

    /// The selected plan's view.
    pub fn selected_plan(&self) -> Option<&PlanView> {
        self.plans.get(self.plan_sel)
    }

    /// Rebuilds the view's plans and puts the selection back on `keep` when it is in the new
    /// view; otherwise the selection moves to the first plan of the view (rule 4).
    fn rebuild_visible(&mut self, keep: Option<&str>) {
        self.plans = self
            .all_plans
            .iter()
            .filter(|view| self.filter.shows(view))
            .cloned()
            .collect();
        self.hidden = self.all_plans.len() - self.plans.len();
        self.plan_sel = keep
            .and_then(|id| self.plans.iter().position(|view| view.plan.item.id == id))
            .unwrap_or(0);
    }

    /// `f` on the plans page: the next view. The selection stays on the same plan when the
    /// new view has it; otherwise it moves to the first plan of the view. The task selection
    /// resets only when the plan changes.
    fn cycle_filter(&mut self) -> ArcKey {
        let keep = self.selected_plan().map(|view| view.plan.item.id.clone());
        self.filter = self.filter.next();
        self.rebuild_visible(keep.as_deref());
        if self.selected_plan().map(|view| view.plan.item.id.clone()) != keep {
            self.task_sel = 0;
            self.task_top = 0;
        }
        ArcKey::Redraw
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

    /// Every plan of the current view, as the page draws it, in the store's order.
    pub fn plan_rows(&self) -> Vec<PlanRow> {
        self.plans
            .iter()
            .map(|view| plan_row(view, self.task_rows(view)))
            .collect()
    }

    /// One plan's tasks: the dependency tree, or the flat list with `after`. A `doing` task's
    /// row carries its worker's freshness (ADR 0048 decisions 7.9 and 9): the frame's state
    /// alone cannot say whether the attempt is alive.
    pub fn task_rows(&self, view: &PlanView) -> Vec<TaskRow> {
        let mut rows = if self.flat {
            flat_rows(view)
        } else {
            tree_rows(view)
        };
        let default_stall = self
            .store
            .config()
            .map(|config| config.limits.stall_timeout_secs)
            .unwrap_or(0);
        for row in &mut rows {
            let Some(task) = view.tasks.iter().find(|task| task.item.id == row.id) else {
                continue;
            };
            row.freshness = freshness(&self.store, &task.item, default_stall);
        }
        rows
    }

    /// The plans the board shows: the plan `p` pinned, else the plans page's current view, in
    /// claim order (the plan's priority, then its id) — the order `radian arc list` prints.
    fn board_plans(&self) -> Vec<PlanView> {
        let mut plans: Vec<PlanView> = match &self.board.plan {
            Some(id) => self
                .all_plans
                .iter()
                .filter(|view| view.plan.item.id == *id)
                .cloned()
                .collect(),
            None => self.plans.clone(),
        };
        plans.sort_by(|one, other| claim_order(&one.plan.item, &other.plan.item));
        plans
    }

    /// Rebuilds the board from the store, keeping its selection on the same task. A pinned plan
    /// that disappeared unpins it.
    pub fn rebuild_board(&mut self) {
        if self
            .board
            .plan
            .as_ref()
            .is_some_and(|id| !self.all_plans.iter().any(|view| view.plan.item.id == *id))
        {
            self.board.plan = None;
        }
        let plans = self.board_plans();
        self.board.rebuild(&plans, &self.store);
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
        self.open_task_id(&id, Page::Plans);
    }

    /// Opens one task's page and remembers where `Esc`/`Tab` return to.
    pub fn open_task_id(&mut self, id: &str, from: Page) -> bool {
        let loaded = match self.store.load_task(id) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.result = Some(ResultLine {
                    text: error.message(),
                    failed: true,
                });
                return false;
            }
        };
        let attempts = radian_arc::read::task_attempts(&self.store, id).unwrap_or_default();
        self.task = Some(TaskPage::new(&loaded.item, attempts));
        self.task_return = from;
        self.page = Page::Task;
        self.refresh_journal();
        true
    }

    /// Leaves the task page, back to the page it was opened from.
    pub fn close_task(&mut self) {
        self.page = self.task_return;
        self.task = None;
        self.merge = None;
        if self.page == Page::Board {
            self.rebuild_board();
        }
    }

    /// Whether the merge page applies to `task`: `review.*` or `done` (ADR 0048 §6 and §10).
    pub fn merge_page_applies(task: &TaskFile) -> bool {
        matches!(task.state, TaskState::Review | TaskState::Done)
    }

    /// Opens the merge page for the task the task page shows, or says why it cannot.
    pub fn open_merge(&mut self) -> bool {
        let Some(id) = self.task.as_ref().map(|page| page.id.clone()) else {
            return false;
        };
        self.open_merge_for(&id, Page::Task)
    }

    /// Opens the merge page for `id` and remembers where `Esc` returns to.
    pub fn open_merge_for(&mut self, id: &str, from: Page) -> bool {
        let task = match self.store.load_task(id) {
            Ok(loaded) => loaded.item,
            Err(error) => {
                self.result = Some(ResultLine {
                    text: error.message(),
                    failed: true,
                });
                return false;
            }
        };
        if !Self::merge_page_applies(&task) {
            self.result = Some(ResultLine {
                text: format!(
                    "{} is {}.{}; the merge page opens for a task in review or done",
                    task.id,
                    task.state.as_str(),
                    task.phase
                ),
                failed: true,
            });
            return false;
        }
        self.merge = Some(MergePage::new(task.id.clone(), task.attempt));
        self.merge_return = from;
        self.page = Page::Merge;
        true
    }

    /// Leaves the merge page: back to the task page or the board it was opened from.
    pub fn close_merge(&mut self) {
        self.merge = None;
        self.page = self.merge_return;
        if self.page == Page::Board {
            self.rebuild_board();
        }
    }

    /// The merge view the runner should read now, if any.
    pub fn merge_request(&self) -> Option<String> {
        if self.page != Page::Merge {
            return None;
        }
        let page = self.merge.as_ref()?;
        if page.loading {
            return None;
        }
        (page.view.is_none() || page.reload).then(|| page.id.clone())
    }

    /// Marks the merge view's read as in flight.
    pub fn mark_merge_loading(&mut self) {
        if let Some(page) = &mut self.merge {
            page.loading = true;
        }
    }

    /// Records the merge view a worker read. A reply for another task is dropped.
    pub fn apply_merge(&mut self, id: &str, result: Result<MergeView, String>) -> bool {
        let Some(page) = &mut self.merge else {
            return false;
        };
        if page.id != id {
            return false;
        }
        page.loading = false;
        page.reload = false;
        match result {
            Ok(view) => {
                if page.attempt != view.attempt {
                    page.attempt = view.attempt;
                    page.diff = None;
                    page.diff_key = None;
                    page.diff_loading = None;
                    page.diff_error = None;
                    page.diff_scroll = 0;
                    page.file_sel = 0;
                }
                page.view = Some(view);
                page.error = None;
                let rows = page.command_rows().len();
                page.command_sel = page.command_sel.min(rows.saturating_sub(1));
            }
            Err(error) => page.error = Some(error),
        }
        true
    }

    /// Queues one owner action and starts the footer's spinner.
    pub fn queue_action(&mut self, action: actions::Action) {
        self.result = None;
        self.pending = Some(Pending {
            label: action.label().to_owned(),
            started: Instant::now(),
        });
        self.action = Some(action);
    }

    /// The runner takes the queued action, if any.
    pub fn take_action(&mut self) -> Option<actions::Action> {
        self.action.take()
    }

    /// The runner takes the command log it should read on a worker thread, if any. The request
    /// stays pending until the runner applies its reply, so a second read cannot start meanwhile.
    pub fn take_log_request(&mut self) -> Option<LogRequest> {
        let request = self.log_request.take()?;
        self.log_pending = Some(request.clone());
        Some(request)
    }

    /// Records the log a worker read for `request`: the float opens on the merge page that
    /// asked for it, and a reply for anything but the pending request is dropped (a reply that
    /// arrives out of order must not open an earlier command's log).
    pub fn apply_log(&mut self, request: &LogRequest, result: Result<String, String>) -> bool {
        if self.log_pending.as_ref() != Some(request) {
            return false;
        }
        self.log_pending = None;
        let Some(page) = &mut self.merge else {
            return false;
        };
        if page.id != request.task {
            return false;
        }
        match result {
            Ok(text) => {
                page.log = Some(LogView::new(request.title.clone(), &text));
                true
            }
            Err(error) => {
                self.result = Some(ResultLine {
                    text: format!("cannot read {}: {error}", request.path.display()),
                    failed: true,
                });
                true
            }
        }
    }

    /// Records an action's result: the spinner stops and the line stays until the next key.
    pub fn finish_action(&mut self, outcome: actions::Outcome) {
        self.pending = None;
        self.result = Some(ResultLine {
            text: outcome.text,
            failed: !outcome.ok,
        });
        self.refresh();
        // The task page was opened before the action and the merge page may sit on top of it:
        // keep the state it shows current without waiting for the next refresh tick.
        if self.task.is_some() {
            self.refresh_task_page();
        }
    }

    /// Why the selected task cannot start now, or `None` when it can. The footer's `s` hint is
    /// weak while this names something, and pressing `s` shows the same line (ADR 0048 §6:
    /// only a `todo.ready` task whose `after` tasks are merged, in a confirmed plan, starts).
    pub fn start_blocker(&self) -> Option<String> {
        let Some(view) = self.selected_plan() else {
            return Some("no plan selected".to_owned());
        };
        let Some(task) = self.selected_task() else {
            return Some("no task selected".to_owned());
        };
        self.start_blocker_for(view, &task.item)
    }

    /// [`ArcApp::start_blocker`] for one task of one plan: the board asks it about the card it
    /// shows.
    fn start_blocker_for(&self, view: &PlanView, item: &TaskFile) -> Option<String> {
        if view.plan.item.is_cancelled() {
            return Some(format!("plan {} is cancelled", view.plan.item.id));
        }
        if view.plan.item.status != "confirmed" {
            return Some(format!(
                "plan {} is {}; confirm it before starting its tasks",
                view.plan.item.id, view.plan.item.status
            ));
        }
        if item.state != TaskState::Todo || item.phase != "ready" {
            return Some(format!(
                "{} is {}.{}; only a todo.ready task starts",
                item.id,
                item.state.as_str(),
                item.phase
            ));
        }
        let mut waiting = Vec::new();
        for id in &item.after {
            let dependency = self
                .all_plans
                .iter()
                .flat_map(|view| view.tasks.iter())
                .find(|task| task.item.id == *id)
                .map(|task| &task.item);
            let Some(dependency) = dependency else {
                return Some(format!("`after` names a task that does not exist: {id}"));
            };
            if !dependency.is_merged() {
                waiting.push(format!(
                    "{} ({}.{})",
                    dependency.id,
                    dependency.state.as_str(),
                    dependency.phase
                ));
            }
        }
        (!waiting.is_empty()).then(|| format!("waits {}", waiting.join(", ")))
    }

    /// The next auto level in the plans page's `a` cycle: `off → run → merge → off`.
    pub fn next_auto(level: &str) -> &'static str {
        match level {
            "off" => "run",
            "run" => "merge",
            _ => "off",
        }
    }

    /// The diff the runner should fetch now, if any: the page's selected attempt has no diff,
    /// and no request for that same attempt is in flight.
    pub fn diff_request(&self) -> Option<(String, u32)> {
        match self.page {
            Page::Task => {
                let page = self.task.as_ref()?;
                if page.tab != TaskTab::Diff {
                    return None;
                }
                let key = (page.id.clone(), page.attempt.unwrap_or(0));
                if page.diff_key.as_ref() == Some(&key) || page.diff_loading.as_ref() == Some(&key)
                {
                    return None;
                }
                Some(key)
            }
            Page::Merge => {
                let page = self.merge.as_ref()?;
                let key = (page.id.clone(), page.attempt);
                if page.diff_key.as_ref() == Some(&key) || page.diff_loading.as_ref() == Some(&key)
                {
                    return None;
                }
                Some(key)
            }
            Page::Plans | Page::Board => None,
        }
    }

    /// Records the diff a worker read. A reply for another task or attempt is dropped, and
    /// only the request it belongs to stops being in flight.
    pub fn apply_diff(&mut self, id: &str, attempt: u32, result: Result<TaskDiff, String>) -> bool {
        match self.page {
            Page::Task => {
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
                        let count = diff.files.len();
                        page.diff_error = None;
                        page.diff = Some(diff);
                        page.file_sel = page.file_sel.min(count.saturating_sub(1));
                    }
                    Err(error) => page.diff_error = Some(error),
                }
                true
            }
            Page::Merge => {
                let Some(page) = &mut self.merge else {
                    return false;
                };
                if page.id != id || page.attempt != attempt {
                    return false;
                }
                let key = (id.to_owned(), attempt);
                if page.diff_loading.as_ref() == Some(&key) {
                    page.diff_loading = None;
                }
                page.diff_key = Some(key);
                match result {
                    Ok(diff) => {
                        let count = diff.files.len();
                        page.diff_error = None;
                        page.diff = Some(diff);
                        page.file_sel = page.file_sel.min(count.saturating_sub(1));
                    }
                    Err(error) => page.diff_error = Some(error),
                }
                true
            }
            Page::Plans | Page::Board => false,
        }
    }

    /// Marks the request `key` as in flight, so no second request for it is started.
    pub fn mark_diff_loading(&mut self, key: (String, u32)) {
        match self.page {
            Page::Task => {
                if let Some(page) = &mut self.task {
                    page.diff_loading = Some(key);
                }
            }
            Page::Merge => {
                if let Some(page) = &mut self.merge {
                    page.diff_loading = Some(key);
                }
            }
            Page::Plans | Page::Board => {}
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
        // Ctrl-C always leaves, whatever is on top.
        if key.code == KeyCode::Char('c') && ctrl {
            return ArcKey::Quit;
        }
        // An open float takes every key first (ADR 0035: floats only): the reject note may
        // hold a `q` without leaving the TUI.
        if self.float.is_some() {
            self.result = None;
            return self.on_float_key(key);
        }
        if key.code == KeyCode::Char('q') && !ctrl {
            return ArcKey::Quit;
        }
        // An action in flight takes no page key: its result lands on the footer when it ends.
        if self.pending.is_some() {
            return ArcKey::Ignored;
        }
        // The next key clears the last action's line (ADR 0048 §11).
        self.result = None;
        // `b` opens the board from any page; on the board it goes back to the plans page.
        if key.code == KeyCode::Char('b') && !ctrl {
            if self.page == Page::Board {
                self.page = Page::Plans;
            } else {
                self.page = Page::Board;
                self.rebuild_board();
            }
            return ArcKey::Redraw;
        }
        match self.page {
            Page::Plans => self.on_plans_key(key),
            Page::Board => self.on_board_key(key),
            Page::Task => self.on_task_key(key),
            Page::Merge => self.on_merge_key(key),
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
            KeyCode::Char('f') => self.cycle_filter(),
            KeyCode::Char('c') => {
                // With the task column focused `c` confirms a proposed task; with the plan
                // column focused it confirms a draft plan, as it always has (ADR 0048 §11).
                let target = if self.focus == Focus::Tasks {
                    self.selected_task()
                        .filter(|task| {
                            task.item.state == TaskState::Todo && task.item.phase == "proposed"
                        })
                        .map(|task| task.item.id.clone())
                } else {
                    self.selected_plan()
                        .filter(|view| view.plan.item.status == "draft")
                        .map(|view| view.plan.item.id.clone())
                };
                match target {
                    Some(id) => {
                        self.queue_action(actions::Action::Confirm(id));
                    }
                    None => {
                        self.result = Some(ResultLine {
                            text: "only a draft plan or a proposed task is confirmed".to_owned(),
                            failed: true,
                        });
                    }
                }
                ArcKey::Redraw
            }
            KeyCode::Char('x') => {
                // `x` always asks first, the way the merge page's `M` does (ADR 0048 §11);
                // the CLI's own refusal is what a done task or a cancelled plan gets.
                let (message, action) = match self.focus {
                    Focus::Tasks => {
                        let Some(task) = self.selected_task() else {
                            return ArcKey::Ignored;
                        };
                        let id = task.item.id.clone();
                        (format!("cancel {id} ?"), actions::Action::Cancel(id))
                    }
                    Focus::Plans => {
                        let Some(view) = self.selected_plan() else {
                            return ArcKey::Ignored;
                        };
                        let id = view.plan.item.id.clone();
                        let open = view
                            .tasks
                            .iter()
                            .filter(|task| {
                                !task.changed_outside_arc
                                    && matches!(
                                        task.item.state,
                                        TaskState::Todo | TaskState::Doing | TaskState::Review
                                    )
                            })
                            .count();
                        (
                            format!("cancel plan {id} and its {open} open tasks?"),
                            actions::Action::Cancel(id),
                        )
                    }
                };
                self.float = Some(Float::Confirm { message, action });
                ArcKey::Redraw
            }
            KeyCode::Char('+') => self.step_priority(-1),
            KeyCode::Char('-') => self.step_priority(1),
            KeyCode::Char('a') => {
                let Some(view) = self.selected_plan() else {
                    return ArcKey::Ignored;
                };
                let plan = view.plan.item.id.clone();
                let status = view.plan.item.status.clone();
                let auto = view.plan.item.auto.clone();
                self.cycle_auto(&plan, &status, &auto)
            }
            KeyCode::Char('p') => {
                let Some(view) = self.selected_plan() else {
                    return ArcKey::Ignored;
                };
                if view.plan.item.auto == "off" {
                    return ArcKey::Ignored;
                }
                let plan = view.plan.item.id.clone();
                self.queue_action(actions::Action::Pause(plan));
                ArcKey::Redraw
            }
            KeyCode::Char('s') => {
                if let Some(blocker) = self.start_blocker() {
                    self.result = Some(ResultLine {
                        text: blocker,
                        failed: false,
                    });
                    return ArcKey::Redraw;
                }
                if let Some(task) = self.selected_task() {
                    let id = task.item.id.clone();
                    self.queue_action(actions::Action::Start(id));
                    return ArcKey::Redraw;
                }
                ArcKey::Ignored
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

    /// `+`/`-` on the plans page: the selected plan's priority one step, `+` toward `P0` and
    /// `-` toward `P5` (ADR 0048 decision 3). At an end the footer says so and nothing is
    /// queued; the next refresh draws the new value.
    fn step_priority(&mut self, delta: i32) -> ArcKey {
        let Some(view) = self.selected_plan() else {
            return ArcKey::Ignored;
        };
        let plan = view.plan.item.id.clone();
        let current = view.plan.item.priority().to_owned();
        self.step_plan_priority(&plan, &current, delta)
    }

    /// [`ArcApp::step_priority`] for one plan: the board steps the selected card's plan.
    fn step_plan_priority(&mut self, plan: &str, current: &str, delta: i32) -> ArcKey {
        let last = PLAN_PRIORITIES.len() as i32 - 1;
        let rank = (priority_rank(current) as i32).min(last);
        let level = PLAN_PRIORITIES[(rank + delta).clamp(0, last) as usize].to_owned();
        if level == current {
            self.result = Some(ResultLine {
                text: format!("plan {plan} is already {current}"),
                failed: false,
            });
            return ArcKey::Redraw;
        }
        self.queue_action(actions::Action::Priority {
            plan: plan.to_owned(),
            level,
        });
        ArcKey::Redraw
    }

    /// `a` on the plans page and the board: the next auto level of `plan`. Only a confirmed
    /// plan has one; setting `merge` asks first (ADR 0048 §11).
    fn cycle_auto(&mut self, plan: &str, status: &str, auto: &str) -> ArcKey {
        if status != "confirmed" {
            self.result = Some(ResultLine {
                text: format!("plan {plan} is {status}; only a confirmed plan has an auto level"),
                failed: true,
            });
            return ArcKey::Redraw;
        }
        let level = Self::next_auto(auto).to_owned();
        let action = actions::Action::Auto {
            plan: plan.to_owned(),
            level,
        };
        let asks = matches!(&action, actions::Action::Auto { level, .. } if level == "merge");
        if asks {
            self.merge_float(action);
        } else {
            self.queue_action(action);
        }
        ArcKey::Redraw
    }

    /// The plan view and task of a card, looked up by the task id in every plan.
    fn card_plan_task(&self, id: &str) -> Option<(&PlanView, &TaskFile)> {
        self.all_plans.iter().find_map(|view| {
            view.tasks
                .iter()
                .find(|task| task.item.id == id)
                .map(|task| (view, &task.item))
        })
    }

    /// The selected card's plan and task.
    fn board_card(&self) -> Option<(&PlanView, &TaskFile)> {
        let id = self.board.selected()?.id.clone();
        self.card_plan_task(&id)
    }

    /// The board's keys. The owner's keys act on the selected card with the plans page's rules,
    /// floats and footer lines (ADR 0048 §11).
    fn on_board_key(&mut self, key: &KeyEvent) -> ArcKey {
        if self.board.finished {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('d') => {
                    self.board.show_columns();
                    ArcKey::Redraw
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.board.move_done(1);
                    ArcKey::Redraw
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.board.move_done(-1);
                    ArcKey::Redraw
                }
                KeyCode::Enter | KeyCode::Tab => {
                    let Some(id) = self.board.selected_done().map(|row| row.id.clone()) else {
                        return ArcKey::Ignored;
                    };
                    if self.open_task_id(&id, Page::Board) {
                        ArcKey::Redraw
                    } else {
                        ArcKey::Ignored
                    }
                }
                _ => ArcKey::Ignored,
            };
        }
        match key.code {
            KeyCode::Esc => {
                self.page = Page::Plans;
                ArcKey::Redraw
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.board.move_horizontal(-1);
                ArcKey::Redraw
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.board.move_horizontal(1);
                ArcKey::Redraw
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.board.move_vertical(-1);
                ArcKey::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.board.move_vertical(1);
                ArcKey::Redraw
            }
            KeyCode::Enter | KeyCode::Tab => {
                let Some(id) = self.board.selected().map(|card| card.id.clone()) else {
                    return ArcKey::Ignored;
                };
                if self.open_task_id(&id, Page::Board) {
                    ArcKey::Redraw
                } else {
                    ArcKey::Ignored
                }
            }
            KeyCode::Char('d') => {
                self.board.show_finished();
                ArcKey::Redraw
            }
            KeyCode::Char('p') => {
                if self.board.toggle_pin() {
                    self.rebuild_board();
                    ArcKey::Redraw
                } else {
                    ArcKey::Ignored
                }
            }
            KeyCode::Char('s') => {
                let Some((view, task)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                if let Some(blocker) = self.start_blocker_for(view, task) {
                    self.result = Some(ResultLine {
                        text: blocker,
                        failed: false,
                    });
                    return ArcKey::Redraw;
                }
                let id = task.id.clone();
                self.queue_action(actions::Action::Start(id));
                ArcKey::Redraw
            }
            KeyCode::Char('c') => {
                let Some((_, task)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                if task.state != TaskState::Todo || task.phase != "proposed" {
                    self.result = Some(ResultLine {
                        text: "only a draft plan or a proposed task is confirmed".to_owned(),
                        failed: true,
                    });
                    return ArcKey::Redraw;
                }
                let id = task.id.clone();
                self.queue_action(actions::Action::Confirm(id));
                ArcKey::Redraw
            }
            KeyCode::Char('x') => {
                let Some((_, task)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                let id = task.id.clone();
                self.float = Some(Float::Confirm {
                    message: format!("cancel {id} ?"),
                    action: actions::Action::Cancel(id),
                });
                ArcKey::Redraw
            }
            KeyCode::Char('a') => {
                let Some((view, _)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                let plan = view.plan.item.id.clone();
                let status = view.plan.item.status.clone();
                let auto = view.plan.item.auto.clone();
                self.cycle_auto(&plan, &status, &auto)
            }
            KeyCode::Char('+') => {
                let Some((view, _)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                let plan = view.plan.item.id.clone();
                let current = view.plan.item.priority().to_owned();
                self.step_plan_priority(&plan, &current, -1)
            }
            KeyCode::Char('-') => {
                let Some((view, _)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                let plan = view.plan.item.id.clone();
                let current = view.plan.item.priority().to_owned();
                self.step_plan_priority(&plan, &current, 1)
            }
            // The merge page's own keys land there: a card in `review.*` opens it, and its `M`,
            // `r` and `v` then act on the task (ADR 0048 §11). `m` is the task page's key.
            KeyCode::Char('M') | KeyCode::Char('m') | KeyCode::Char('r') | KeyCode::Char('v') => {
                let Some((_, task)) = self.board_card() else {
                    return ArcKey::Ignored;
                };
                let id = task.id.clone();
                self.open_merge_for(&id, Page::Board);
                ArcKey::Redraw
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
            KeyCode::Esc => {
                self.close_task();
                ArcKey::Redraw
            }
            // The tabs cycle both ways; `Tab` no longer leaves the page (`Esc` does).
            KeyCode::BackTab => {
                page.tab = page.tab.previous();
                ArcKey::Redraw
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                page.tab = page.tab.previous();
                ArcKey::Redraw
            }
            KeyCode::Tab => {
                page.tab = page.tab.next();
                ArcKey::Redraw
            }
            KeyCode::Char('m') => {
                self.open_merge();
                ArcKey::Redraw
            }
            KeyCode::Char('[') => {
                page.switch_attempt(-1);
                self.refresh_journal();
                ArcKey::Redraw
            }
            KeyCode::Char(']') => {
                page.switch_attempt(1);
                self.refresh_journal();
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
                TaskTab::Brief => {
                    page.brief_follow = false;
                    page.brief_scroll = page.brief_scroll.saturating_add(1);
                    ArcKey::Redraw
                }
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
                TaskTab::Brief => {
                    page.brief_follow = false;
                    page.brief_scroll = page.brief_scroll.saturating_sub(1);
                    ArcKey::Redraw
                }
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
                    // The frame clamps the brief to its last page and clears the follow, so a
                    // later `k` moves up from the real end.
                    TaskTab::Brief => page.brief_follow = true,
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

    /// A confirmation float carrying the action `y` runs (merge or the `merge` auto level).
    fn merge_float(&mut self, action: actions::Action) {
        let message = match &action {
            actions::Action::Merge(task) => format!("merge {task} onto main?"),
            actions::Action::Auto { plan, .. } => format!("set auto merge on {plan}?"),
            _ => return,
        };
        self.float = Some(Float::Confirm { message, action });
    }

    /// The keys a float takes: `y`/`n` for a confirmation, typing/Enter/Esc for the reject note.
    fn on_float_key(&mut self, key: &KeyEvent) -> ArcKey {
        let Some(float) = self.float.clone() else {
            return ArcKey::Ignored;
        };
        match float {
            Float::Confirm { action, .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.float = None;
                    self.queue_action(action);
                    ArcKey::Redraw
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.float = None;
                    ArcKey::Redraw
                }
                _ => ArcKey::Ignored,
            },
            Float::Reject { note } => match key.code {
                KeyCode::Esc => {
                    self.float = None;
                    ArcKey::Redraw
                }
                KeyCode::Enter => {
                    if note.trim().is_empty() {
                        // The CLI refuses an empty note (exit 2); the float stays open.
                        self.result = Some(ResultLine {
                            text: "the reject note is empty".to_owned(),
                            failed: true,
                        });
                        return ArcKey::Redraw;
                    }
                    let Some(task) = self.merge.as_ref().map(|page| page.id.clone()) else {
                        self.float = None;
                        return ArcKey::Redraw;
                    };
                    self.float = None;
                    self.queue_action(actions::Action::Reject { task, note });
                    ArcKey::Redraw
                }
                KeyCode::Backspace => {
                    let mut note = note;
                    note.pop();
                    self.float = Some(Float::Reject { note });
                    ArcKey::Redraw
                }
                KeyCode::Char(ch) if !ch.is_control() => {
                    let mut note = note;
                    note.push(ch);
                    self.float = Some(Float::Reject { note });
                    ArcKey::Redraw
                }
                _ => ArcKey::Ignored,
            },
        }
    }

    /// The merge page's keys (ADR 0048 §11).
    fn on_merge_key(&mut self, key: &KeyEvent) -> ArcKey {
        // A command's log takes the keys first: it scrolls, and Esc closes it.
        if self.merge.as_ref().is_some_and(|page| page.log.is_some()) {
            let Some(page) = self.merge.as_mut() else {
                return ArcKey::Ignored;
            };
            match key.code {
                KeyCode::Esc => page.log = None,
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(log) = &mut page.log {
                        log.scroll(1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(log) = &mut page.log {
                        log.scroll(-1);
                    }
                }
                KeyCode::Char('f') => {
                    if let Some(log) = &mut page.log {
                        log.scroll(i32::MAX);
                    }
                }
                _ => return ArcKey::Ignored,
            }
            return ArcKey::Redraw;
        }
        let Some(id) = self.merge.as_ref().map(|page| page.id.clone()) else {
            return ArcKey::Ignored;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.close_merge();
                ArcKey::Redraw
            }
            KeyCode::Char('M') => {
                self.merge_float(actions::Action::Merge(id));
                ArcKey::Redraw
            }
            KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.merge_float(actions::Action::Merge(id));
                ArcKey::Redraw
            }
            KeyCode::Char('r') => {
                self.float = Some(Float::Reject {
                    note: String::new(),
                });
                ArcKey::Redraw
            }
            KeyCode::Char('v') => {
                self.queue_action(actions::Action::Reverify(id));
                ArcKey::Redraw
            }
            KeyCode::Up => self.move_command(-1),
            KeyCode::Down => self.move_command(1),
            KeyCode::Char('j') => self.scroll_diff(1),
            KeyCode::Char('k') => self.scroll_diff(-1),
            KeyCode::Char('f') => self.follow_diff(),
            KeyCode::Enter => {
                self.open_log();
                ArcKey::Redraw
            }
            KeyCode::Char('n') => self.move_file(1),
            KeyCode::Char('p') => self.move_file(-1),
            _ => ArcKey::Ignored,
        }
    }

    /// Moves the command selection by `delta`, clamped to the rows.
    fn move_command(&mut self, delta: i32) -> ArcKey {
        let Some(page) = &mut self.merge else {
            return ArcKey::Ignored;
        };
        let count = page.command_rows().len();
        if count == 0 {
            return ArcKey::Ignored;
        }
        page.command_sel = (page.command_sel as i32 + delta).clamp(0, count as i32 - 1) as usize;
        ArcKey::Redraw
    }

    /// Moves the selected diff file by `delta`, clamped to the list.
    fn move_file(&mut self, delta: i32) -> ArcKey {
        let Some(page) = &mut self.merge else {
            return ArcKey::Ignored;
        };
        let count = page.diff.as_ref().map(|diff| diff.files.len()).unwrap_or(0);
        if count == 0 {
            return ArcKey::Redraw;
        }
        page.file_sel = (page.file_sel as i32 + delta).clamp(0, count as i32 - 1) as usize;
        page.diff_scroll = 0;
        ArcKey::Redraw
    }

    /// Scrolls the merge page's diff by `delta` rows, clamped to its body.
    fn scroll_diff(&mut self, delta: i32) -> ArcKey {
        let Some(page) = &mut self.merge else {
            return ArcKey::Ignored;
        };
        let end = page.diff_body_len().saturating_sub(1);
        page.diff_scroll = (page.diff_scroll as i32 + delta).clamp(0, end as i32) as usize;
        ArcKey::Redraw
    }

    /// Follows the diff's end: its last body row becomes the first visible one.
    fn follow_diff(&mut self) -> ArcKey {
        let Some(page) = &mut self.merge else {
            return ArcKey::Ignored;
        };
        page.diff_scroll = page.diff_body_len().saturating_sub(1);
        ArcKey::Redraw
    }

    /// Queues the selected command's log for the runner to read on a worker thread.
    fn open_log(&mut self) {
        // One read at a time: the pending reply opens its float, and a second Enter must not
        // queue a read whose reply could land first.
        if self.log_pending.is_some() {
            return;
        }
        let selected = self
            .merge
            .as_ref()
            .and_then(|page| page.selected_command())
            .map(|command| (command.line.clone(), command.log.clone()));
        let Some((title, log)) = selected else {
            return;
        };
        let Some(path) = log else {
            self.result = Some(ResultLine {
                text: format!("no log for `{title}`"),
                failed: false,
            });
            return;
        };
        let Some(task) = self.merge.as_ref().map(|page| page.id.clone()) else {
            return;
        };
        self.log_request = Some(LogRequest { task, title, path });
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
        .filter(|task| task.item.is_merged())
        .count();
    let mark = plan_mark(view);
    let costs: Vec<f64> = view
        .tasks
        .iter()
        .filter_map(|task| task.item.cost_usd)
        .collect();
    let cost = (!costs.is_empty()).then(|| costs.iter().sum::<f64>());
    PlanRow {
        id: view.plan.item.id.clone(),
        status: view.plan.item.status.clone(),
        priority: view.plan.item.priority().to_owned(),
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
            parents.push((found.item.id.clone(), found.item.is_merged()));
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
        freshness: None,
        connector,
        also_after,
        waits,
        after: Vec::new(),
    }
}

/// Whether a `doing` task's worker is alive, from the wrapper's heartbeat and the attempt's
/// stall window (ADR 0048 decisions 7.9 and 9). `None` for a task that is not `doing`: only a
/// running attempt has a heartbeat to read.
fn freshness(store: &Store, task: &TaskFile, default_stall: u64) -> Option<Freshness> {
    if task.state != TaskState::Doing {
        return None;
    }
    let Some(beat) = radian_arc::store::attempt_ops::read_heartbeat(store, &task.id, task.attempt)
    else {
        return Some(Freshness::Starting);
    };
    let stall = task.stall_timeout_secs.unwrap_or(default_stall);
    let window = radian_arc::store::attempt_ops::liveness_window_secs(stall);
    match radian_arc::store::attempt_ops::utc_age_secs(&beat) {
        Some(age) if age > window => Some(Freshness::Stalled { age }),
        Some(age) => Some(Freshness::Beating { age }),
        None => Some(Freshness::Starting),
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

//! The arc TUI board's rule (ADR 0048 decisions 1, 9 and 13): the column an open task belongs
//! to, and whether a plan `arc` paused through one of its tasks waits for the owner.
//!
//! The rule lives here, not in the TUI, because `radian arc list --json` and `radian arc show
//! --json` write the answer as a task's `board_column`, and the hub's item routes return those
//! bytes: a client reads the board's column without guessing it from reason text.

use std::collections::BTreeSet;

use crate::store::attempt_ops;
use crate::store::model::{PlanFile, TaskFile, TaskState};
use crate::store::{PlanView, Store};

/// The board's four columns, left to right.
///
/// The JSON's `board_column` value is [`BoardColumn::as_str`], and [`BoardColumn::ALL`] is the
/// one list the serializer, the arc TUI and the manual's drift test read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardColumn {
    /// A `todo.*` task the owner or a pass may start.
    Todo,
    /// A `doing.*` task a worker holds.
    Doing,
    /// A task a gate or the merge advances by itself.
    Verifying,
    /// A task that waits for the owner.
    NeedsYou,
}

impl BoardColumn {
    /// The four columns, left to right.
    pub const ALL: [BoardColumn; 4] = [
        BoardColumn::Todo,
        BoardColumn::Doing,
        BoardColumn::Verifying,
        BoardColumn::NeedsYou,
    ];

    /// The name `board_column` writes in JSON and the manual uses for the column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BoardColumn::Todo => "todo",
            BoardColumn::Doing => "doing",
            BoardColumn::Verifying => "verifying",
            BoardColumn::NeedsYou => "needs_you",
        }
    }

    /// The column's place, left to right: the arc TUI indexes its columns with it.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            BoardColumn::Todo => 0,
            BoardColumn::Doing => 1,
            BoardColumn::Verifying => 2,
            BoardColumn::NeedsYou => 3,
        }
    }
}

/// The column a task belongs to, or `None` for a finished task (`done.*` or `failed.*`).
///
/// `paused` marks a plan `arc` paused through one of its tasks: every open task of it waits for
/// the owner. A `review.rework_pending` task a scheduling pass would still reverify by itself (a
/// command failure with automatic reverifies left) stays in `verifying`, because a gate runs
/// for it once main moves; the rule is [`crate::review::auto_reverify_pending`], the pass's own.
#[must_use]
pub fn column_of(
    store: &Store,
    plan: &PlanFile,
    task: &TaskFile,
    paused: bool,
) -> Option<BoardColumn> {
    if matches!(task.state, TaskState::Done | TaskState::Failed) {
        return None;
    }
    if paused {
        return Some(BoardColumn::NeedsYou);
    }
    match (task.state, task.phase.as_str()) {
        (TaskState::Todo, "proposed" | "paused_budget") => Some(BoardColumn::NeedsYou),
        (TaskState::Todo, _) => Some(BoardColumn::Todo),
        (TaskState::Doing, _) => Some(BoardColumn::Doing),
        (TaskState::Review, "awaiting_human") => Some(if plan.auto == "merge" {
            BoardColumn::Verifying
        } else {
            BoardColumn::NeedsYou
        }),
        (TaskState::Review, "rework_pending") => {
            Some(if crate::review::auto_reverify_pending(store, &task.id) {
                BoardColumn::Verifying
            } else {
                BoardColumn::NeedsYou
            })
        }
        (TaskState::Review, _) => Some(BoardColumn::Verifying),
        (TaskState::Done | TaskState::Failed, _) => None,
    }
}

/// Whether `view`'s plan is paused through one of its tasks: its automatic rework limit wrote
/// `paused.json` next to an attempt, or a plan budget set the plan's `auto` to `off` and left a
/// task it stopped in `todo.paused_budget`.
///
/// A task budget also leaves its task in `todo.paused_budget`, but it leaves the plan's `auto`
/// alone, so only the task itself waits for the owner. `radian arc pause` writes no marker
/// either: the plan's `auto` is `off`, which is also the level a plan is confirmed at, so the
/// board cannot tell the two apart.
#[must_use]
pub fn paused_plan(store: &Store, view: &PlanView) -> bool {
    let plan_paused = view
        .tasks
        .iter()
        .any(|task| attempt_ops::paused_plan(store, &task.item.id, task.item.attempt).is_some());
    let budget_paused = view.plan.item.auto == "off"
        && view
            .tasks
            .iter()
            .any(|task| task.item.state == TaskState::Todo && task.item.phase == "paused_budget");
    plan_paused || budget_paused
}

/// Every plan `arc` paused through one of `plans`' tasks, by plan id (see [`paused_plan`]).
#[must_use]
pub fn paused_plans(plans: &[PlanView], store: &Store) -> BTreeSet<String> {
    plans
        .iter()
        .filter(|view| paused_plan(store, view))
        .map(|view| view.plan.item.id.clone())
        .collect()
}

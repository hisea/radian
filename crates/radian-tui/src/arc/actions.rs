//! The owner's arc actions in the TUI (ADR 0048 §2 and §11).
//!
//! Every action is the CLI's library call in this process — `plan_ops::confirm`,
//! `plan_ops::set_auto`, `plan_ops::pause`, `attempt::start`, `review::merge`,
//! `review::reject` and `review::reverify` — never a `radian arc` subprocess. The TUI queues
//! one [`Action`] and the runner executes it on a worker thread, so a merge or a `start` that
//! takes minutes never blocks a frame.
//!
//! The result line is the text the CLI would print, or the error text and the CLI's exit
//! reason. [`ArcOps`] is the seam a test uses to see which library call an action made.

use radian_arc::attempt::{self, Cancelled, Started};
use radian_arc::review::{self, Merged, Rejected, Reverified};
use radian_arc::store::plan_ops::{self, Confirmed};
use radian_arc::store::{Error, Store};

/// One owner action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `radian arc confirm <id>`: a draft plan or a `todo.proposed` task.
    Confirm(String),
    /// `radian arc priority <plan> <level>`.
    Priority { plan: String, level: String },
    /// `radian arc auto <plan> <level>`.
    Auto { plan: String, level: String },
    /// `radian arc pause <plan>`.
    Pause(String),
    /// `radian arc cancel <id>`: one task, or a whole plan.
    Cancel(String),
    /// `radian arc start <task>`.
    Start(String),
    /// `radian arc merge <task>`.
    Merge(String),
    /// `radian arc reject <task> --note <note>`.
    Reject { task: String, note: String },
    /// `radian arc reverify <task>`.
    Reverify(String),
}

impl Action {
    /// The word the footer's spinner shows.
    pub fn label(&self) -> &'static str {
        match self {
            Action::Confirm(_) => "confirm",
            Action::Cancel(_) => "cancel",
            Action::Priority { .. } => "priority",
            Action::Auto { .. } => "auto",
            Action::Pause(_) => "pause",
            Action::Start(_) => "start",
            Action::Merge(_) => "merge",
            Action::Reject { .. } => "reject",
            Action::Reverify(_) => "reverify",
        }
    }
}

/// What one action produced: the CLI's line, and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub text: String,
    pub ok: bool,
}

impl Outcome {
    fn line(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: true,
        }
    }

    fn failed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: false,
        }
    }

    fn error(error: &Error) -> Self {
        Self::failed(format!(
            "{} (exit {})",
            error.message(),
            error.exit().code()
        ))
    }
}

/// The library calls one action makes. [`Library`] is what the TUI uses; a test hands in its
/// own implementation to see which call ran.
pub trait ArcOps {
    fn confirm(&self, store: &Store, id: &str) -> Result<Confirmed, Error>;
    /// `radian arc cancel <id>`: the task or plan form.
    fn cancel(&self, store: &Store, id: &str) -> Result<Cancelled, Error>;
    /// `radian arc priority <plan> <level>`.
    fn set_priority(&self, store: &Store, plan: &str, level: &str) -> Result<(), Error>;
    fn set_auto(&self, store: &Store, plan: &str, level: &str) -> Result<(), Error>;
    fn pause(&self, store: &Store, plan: &str) -> Result<(), Error>;
    fn start(&self, store: &Store, task: &str) -> Result<Started, Error>;
    fn merge(&self, store: &Store, task: &str) -> Result<Merged, Error>;
    fn reject(&self, store: &Store, task: &str, note: &str) -> Result<Rejected, Error>;
    fn reverify(&self, store: &Store, task: &str) -> Result<Reverified, Error>;
}

/// The real implementation: the same functions `radian arc`'s subcommands call.
pub struct Library;

impl ArcOps for Library {
    fn confirm(&self, store: &Store, id: &str) -> Result<Confirmed, Error> {
        plan_ops::confirm(store, id)
    }

    fn cancel(&self, store: &Store, id: &str) -> Result<Cancelled, Error> {
        attempt::cancel_any(store, id)
    }

    fn set_priority(&self, store: &Store, plan: &str, level: &str) -> Result<(), Error> {
        plan_ops::set_priority(store, plan, level)
    }

    fn set_auto(&self, store: &Store, plan: &str, level: &str) -> Result<(), Error> {
        plan_ops::set_auto(store, plan, level)
    }

    fn pause(&self, store: &Store, plan: &str) -> Result<(), Error> {
        plan_ops::pause(store, plan)
    }

    fn start(&self, store: &Store, task: &str) -> Result<Started, Error> {
        attempt::start(store, task)
    }

    fn merge(&self, store: &Store, task: &str) -> Result<Merged, Error> {
        review::merge(store, task)
    }

    fn reject(&self, store: &Store, task: &str, note: &str) -> Result<Rejected, Error> {
        review::reject(store, task, note)
    }

    fn reverify(&self, store: &Store, task: &str) -> Result<Reverified, Error> {
        review::reverify(store, task)
    }
}

/// Runs one action and turns what it did into the footer's result line.
pub fn run(action: &Action, store: &Store, ops: &dyn ArcOps) -> Outcome {
    match action {
        Action::Confirm(id) => match ops.confirm(store, id) {
            Ok(confirmed) => Outcome::line(confirmed.line()),
            Err(error) => Outcome::error(&error),
        },
        Action::Cancel(id) => match ops.cancel(store, id) {
            Ok(cancelled) => Outcome::line(cancelled.lines().join(" · ")),
            Err(error) => Outcome::error(&error),
        },
        Action::Priority { plan, level } => match ops.set_priority(store, plan, level) {
            Ok(()) => Outcome::line(format!("{plan} priority {level}")),
            Err(error) => Outcome::error(&error),
        },
        Action::Auto { plan, level } => match ops.set_auto(store, plan, level) {
            Ok(()) => Outcome::line(format!("{plan} auto {level}")),
            Err(error) => Outcome::error(&error),
        },
        Action::Pause(plan) => match ops.pause(store, plan) {
            Ok(()) => Outcome::line(format!("{plan} auto off")),
            Err(error) => Outcome::error(&error),
        },
        Action::Start(task) => match ops.start(store, task) {
            Ok(started) => {
                let text = attempt::lines(&started).join(" · ");
                // The CLI prints the attempt's own line and then the refusal with its exit
                // code; the TUI keeps both on the one line.
                match &started.verification {
                    Some(Err(error)) => Outcome::failed(format!(
                        "{text} · {} (exit {})",
                        error.message(),
                        error.exit().code()
                    )),
                    _ => Outcome::line(text),
                }
            }
            Err(error) => Outcome::error(&error),
        },
        Action::Merge(task) => match ops.merge(store, task) {
            Ok(merged) => {
                let mut text = merged.line();
                if let Some(warning) = &merged.warning {
                    text.push_str(&format!(" · warning: {warning}"));
                }
                Outcome::line(text)
            }
            Err(error) => Outcome::error(&error),
        },
        Action::Reject { task, note } => match ops.reject(store, task, note) {
            Ok(rejected) => Outcome::line(rejected.line()),
            Err(error) => Outcome::error(&error),
        },
        Action::Reverify(task) => match ops.reverify(store, task) {
            Ok(reverified) => {
                let text = reverified.lines().join(" · ");
                match &reverified.verification {
                    Err(error) => Outcome::failed(format!(
                        "{text} · {} (exit {})",
                        error.message(),
                        error.exit().code()
                    )),
                    Ok(_) => Outcome::line(text),
                }
            }
            Err(error) => Outcome::error(&error),
        },
    }
}

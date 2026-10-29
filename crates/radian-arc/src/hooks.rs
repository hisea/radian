//! `[hooks]`: the commands arc runs at fixed points (ADR 0048 decisions 10 and 13).
//!
//! A transition that owes a hook records its point in the task's `pending_hooks` field in the
//! same transaction that moves the task. The process that holds `runner.lock` runs the pending
//! points: every scheduling pass runs them before its steps, and the owner's `merge` command
//! runs them when it can take the lock without waiting. A point is cleared only after its
//! commands have run, success or failure, so a process killed mid-hook runs it again on the
//! next pass: delivery is at least once, and a command must be safe to run twice.
//!
//! The commands run in the repository root through the gate's shell runner. Each point is an
//! ordered array, and a failing command logs one `warning:` line without stopping the next.
//! Hooks never undo a merge and never change a task's state; the only write they make is the
//! removal of the point they ran.

use std::time::Duration;

use crate::gate::{SeriesOutcome, run_shell};
use crate::store::config::Config;
use crate::store::model::TaskFile;
use crate::store::{Error, Store};

/// The point a task reaching `done.merged` owes.
pub const AFTER_MERGE: &str = "after_merge";

/// The point a task entering any `failed.*` phase owes.
pub const AFTER_FAIL: &str = "after_fail";

/// The point a task entering `review.rework_pending` owes.
pub const AFTER_REWORK: &str = "after_rework";

/// The point a task entering `review.awaiting_human` owes.
pub const AFTER_AWAITING_HUMAN: &str = "after_awaiting_human";

/// The point a new task entering `todo.proposed` owes.
pub const AFTER_PROPOSED: &str = "after_proposed";

/// Every hook point, in the order the manual lists them. A task file's `pending_hooks` list may
/// name any of them; the parser accepts the set and refuses another.
pub const HOOK_POINTS: &[&str] = &[
    AFTER_MERGE,
    AFTER_FAIL,
    AFTER_REWORK,
    AFTER_AWAITING_HUMAN,
    AFTER_PROPOSED,
];

/// The environment every hook command receives, in the order the manual lists it. One list for
/// the code and the manual's drift test. They are set with `Command::env` by name, so the source
/// scan in `radian-host/tests/env_docs.rs` does not take them for inputs a person sets.
pub const HOOK_ENV: &[&str] = &[
    "ARC_TASK",
    "ARC_PLAN",
    "ARC_ATTEMPT",
    "ARC_BRANCH",
    "ARC_WORKTREE",
    "ARC_TARGET_BRANCH",
    "ARC_MERGED_SHA",
    "ARC_STATE",
    "ARC_HOOK",
];

/// Runs every pending hook of every task, in task-id order, and returns one message for each
/// command that failed or ran past its timeout. The caller prints each as a `warning:` line.
/// The caller holds `runner.lock`; this function does not take it.
///
/// A task file that changed outside arc is left alone: its hook stays pending, and `adopt` is
/// how the owner makes it arc's again.
///
/// # Errors
///
/// A task or the configuration cannot be read, or the point cannot be cleared from the task
/// record. A per-command failure is a warning, never an error: the merge stands.
pub fn run_pending(store: &Store) -> Result<Vec<String>, Error> {
    let config = store.config()?;
    let mut warnings = Vec::new();
    for task_id in store.task_ids()? {
        let loaded = store.load_task(&task_id)?;
        if loaded.changed_outside_arc {
            continue;
        }
        let task = loaded.item;
        for point in task.pending_hooks.clone() {
            warnings.extend(run_point(store, &config, &task, &point)?);
        }
    }
    Ok(warnings)
}

/// Runs one point of one task and clears it, success or failure.
fn run_point(
    store: &Store,
    config: &Config,
    task: &TaskFile,
    point: &str,
) -> Result<Vec<String>, Error> {
    let commands = config.hooks.commands(point);
    let mut warnings = Vec::new();
    let log_dir = store.root().join("runs").join(&task.id).join("hooks");
    if !commands.is_empty()
        && let Err(error) = std::fs::create_dir_all(&log_dir)
    {
        return Err(Error::Failed(format!(
            "cannot create {}: {error}",
            log_dir.display()
        )));
    }
    let env = env_for(store, config, task, point);
    let timeout = Duration::from_secs(config.gate.command_timeout_secs);
    for (index, line) in commands.iter().enumerate() {
        let log = log_dir.join(format!("{point}-{:02}.log", index + 1));
        match run_shell(store.repo(), line, timeout, &log, &env, None) {
            Ok(outcome) if outcome.outcome == SeriesOutcome::Passed => {}
            Ok(outcome) => {
                let reason = if outcome.outcome == SeriesOutcome::TimedOut {
                    format!("timed out after {} s", config.gate.command_timeout_secs)
                } else {
                    format!(
                        "exited {}",
                        outcome
                            .exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "on a signal".to_owned())
                    )
                };
                warnings.push(format!(
                    "{} hook `{point}` command {} {reason}: {line}",
                    task.id,
                    index + 1
                ));
            }
            Err(error) => warnings.push(format!(
                "{} hook `{point}` command {} could not run: {error}",
                task.id,
                index + 1
            )),
        }
    }
    let point = point.to_owned();
    store.change_task(&task.id, move |task| {
        task.pending_hooks.retain(|pending| pending != &point);
        Ok(())
    })?;
    Ok(warnings)
}

/// The environment one point receives (decision 10). `ARC_MERGED_SHA` belongs to
/// [`AFTER_MERGE`] only.
fn env_for(
    store: &Store,
    config: &Config,
    task: &TaskFile,
    point: &str,
) -> Vec<(&'static str, String)> {
    let worktree = store.root().join("worktrees").join(&task.id);
    let worktree = std::fs::canonicalize(&worktree).unwrap_or(worktree);
    let mut env = vec![
        ("ARC_TASK", task.id.clone()),
        ("ARC_PLAN", task.plan.clone()),
        ("ARC_ATTEMPT", format!("{}-{}", task.id, task.attempt)),
        ("ARC_BRANCH", format!("work/{}", task.id)),
        ("ARC_WORKTREE", worktree.display().to_string()),
        ("ARC_TARGET_BRANCH", config.target_branch().to_owned()),
        (
            "ARC_STATE",
            format!("{}.{}", task.state.as_str(), task.phase),
        ),
        ("ARC_HOOK", point.to_owned()),
    ];
    if point == AFTER_MERGE
        && let Some(sha) = &task.merged_sha
    {
        env.push(("ARC_MERGED_SHA", sha.clone()));
    }
    env
}

/// The point a task file may name, when it is one of [`HOOK_POINTS`]. The task reader uses this
/// so a hand edit names its bad point instead of leaving a hook that can never run.
pub fn valid_point(point: &str) -> bool {
    HOOK_POINTS.contains(&point)
}

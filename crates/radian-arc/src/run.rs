//! `radian arc run`: the scheduling loop without a TUI (ADR 0048 decision 13).
//!
//! One `run` holds `runner.lock` for its lifetime and makes every transition it can make
//! now. A pass, repeated until a pass changes nothing:
//!
//! 1. reconcile every task in `doing` (apply a result that is there, leave a live attempt);
//! 2. release every retry whose `retry_at` has passed;
//! 3. verify every task in `review.verifying`; this is also how a task a moved branch or a
//!    changed digest sent back reaches `review.awaiting_human` again;
//! 4. for every confirmed plan at `auto` `run` or `merge`, claim and run each `todo.ready`
//!    task whose `after` tasks are merged, up to `[limits] concurrency` attempts at once;
//! 5. for every plan at `auto` `merge`, merge each task whose verification passed.
//!
//! Waiting for a person, a future `retry_at`, or a lead's rework does not keep it running:
//! the loop ends at the first pass that changes nothing. A task of a plan at `auto` `off` is
//! never started or merged by `run`; its verification still advances.
//!
//! The loop prints one line per transition and a final summary of what waits on whom.

use crate::store::attempt_ops::{self, Applied, Claim};
use crate::store::model::{self, TaskState};
use crate::store::{Error, Store, plan_ops};

/// A hard stop on the pass count. Every pass that continues changes at least one task; this
/// only keeps a store that something else keeps rewriting from looping forever.
pub const MAX_PASSES: u32 = 1000;

/// What one `run` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One line per transition, in the order the loop made them.
    pub transitions: Vec<String>,
    /// Why a task that could not advance waits; computed after the loop ran to quiet.
    pub waiting: Vec<String>,
    /// A refusal the loop met at quiet (a merge that cannot proceed, a verification that
    /// this build cannot run).
    pub notes: Vec<String>,
}

impl Report {
    /// The text `radian arc run` prints: the transitions, then the summary.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.transitions {
            out.push_str(line);
            out.push('\n');
        }
        if self.transitions.is_empty() {
            out.push_str("no transition\n");
        }
        out.push_str("summary\n");
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        for line in &self.waiting {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// Runs the loop until nothing more can happen now.
///
/// # Errors
///
/// Another process holds `runner.lock` ([`Error::Locked`], exit 4), a file arc needs cannot be
/// read or written, or the store did not settle within [`MAX_PASSES`] passes.
pub fn run(store: &Store) -> Result<Report, Error> {
    let _lock = store
        .try_runner_lock()?
        .ok_or(Error::Locked(crate::store::RUNNER_LOCK))?;
    let mut report = Report::default();
    let mut passes = 0;
    loop {
        passes += 1;
        if passes > MAX_PASSES {
            return Err(Error::Failed(format!(
                "the scheduling loop did not settle within {MAX_PASSES} passes"
            )));
        }
        if !one_pass(store, &mut report)? {
            break;
        }
    }
    report.waiting = waiting(store)?;
    Ok(report)
}

/// One pass. `true` when it changed a stored file, so the loop takes another pass.
fn one_pass(store: &Store, report: &mut Report) -> Result<bool, Error> {
    let mut changed = false;

    // 1. Doing: apply a finished attempt, leave a live one alone.
    for applied in crate::attempt::reconcile(store)? {
        report
            .transitions
            .push(format!("reconciled: {}", applied.line()));
        changed = true;
    }

    // 2. A retry whose time has come.
    for id in attempt_ops::release_retries(store, &plan_ops::utc_now())? {
        report
            .transitions
            .push(format!("{id} released: todo.retry_wait -> todo.ready"));
        changed = true;
    }

    // 3. Verification, for every plan: a plan at `auto` `off` still advances here.
    for id in verifying_tasks(store)? {
        match crate::review::verify(store, &id) {
            Ok(verified) => {
                report.transitions.push(verified.line());
                changed = true;
                if verified.phase == "rework_pending"
                    && let Some(paused) = attempt_ops::record_auto_rework(store, &id)?
                {
                    report
                        .transitions
                        .push(format!("{} paused: {}", paused.plan, paused.text));
                    changed = true;
                }
            }
            // A moved branch or a stage this build does not run: the task stays in
            // `review.verifying` and the reason is recorded; the summary reports it.
            Err(Error::Refused(_)) => {}
            Err(error) => return Err(error),
        }
    }

    // 4. Start what the auto levels allow.
    changed |= start_ready(store, report)?;

    // 5. Merge what passed, at auto level `merge`.
    changed |= merge_auto(store, report)?;

    Ok(changed)
}

/// Every task in `review.verifying`, in id order. A file that changed outside arc is skipped.
fn verifying_tasks(store: &Store) -> Result<Vec<String>, Error> {
    let mut ids = Vec::new();
    for id in store.task_ids()? {
        let loaded = store.load_task(&id)?;
        if loaded.changed_outside_arc {
            continue;
        }
        if loaded.item.state == TaskState::Review && loaded.item.phase == "verifying" {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Claims and runs ready tasks of plans at `auto` `run` or `merge`. `true` when it ran at
/// least one attempt.
fn start_ready(store: &Store, report: &mut Report) -> Result<bool, Error> {
    let config = store.config()?;
    let concurrency = u64::from(config.limits.concurrency.max(1));
    let doing = count_doing(store)?;
    if doing >= concurrency {
        return Ok(false);
    }
    let mut slots = concurrency - doing;
    let mut claims: Vec<(Claim, model::TaskFile)> = Vec::new();
    for view in store.plans()? {
        if slots == 0 {
            break;
        }
        if view.plan.changed_outside_arc
            || view.plan.item.status != "confirmed"
            || !matches!(view.plan.item.auto.as_str(), "run" | "merge")
        {
            continue;
        }
        for task in &view.tasks {
            if slots == 0 {
                break;
            }
            if task.changed_outside_arc
                || task.item.state != TaskState::Todo
                || task.item.phase != "ready"
                || !after_merged(store, &task.item)?
            {
                continue;
            }
            match attempt_ops::claim(store, &task.item.id) {
                Ok(claim) => {
                    slots -= 1;
                    claims.push((claim, task.item.clone()));
                }
                // The claim refused: another writer took the slot or the task moved. The
                // next pass re-reads everything.
                Err(Error::Refused(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    if claims.is_empty() {
        return Ok(false);
    }
    for applied in execute(store, &claims, &config)? {
        report.transitions.push(applied.line());
    }
    Ok(true)
}

/// Runs the claimed attempts. With more than one claim each attempt runs on its own thread,
/// so `[limits] concurrency` attempts really overlap; the results are returned in claim
/// order.
fn execute(
    store: &Store,
    claims: &[(Claim, model::TaskFile)],
    config: &crate::store::config::Config,
) -> Result<Vec<Applied>, Error> {
    if claims.len() == 1 {
        let (claim, task) = &claims[0];
        return Ok(vec![crate::attempt::run_claim(store, claim, task, config)?]);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = claims
            .iter()
            .map(|(claim, task)| {
                scope.spawn(move || crate::attempt::run_claim(store, claim, task, config))
            })
            .collect();
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let result = handle
                .join()
                .map_err(|_| Error::Failed("an attempt thread panicked".to_owned()))?;
            results.push(result?);
        }
        Ok(results)
    })
}

/// Merges every finished task of a plan at `auto` `merge`. `true` when a task changed state.
fn merge_auto(store: &Store, report: &mut Report) -> Result<bool, Error> {
    let mut changed = false;
    for view in store.plans()? {
        if view.plan.changed_outside_arc
            || view.plan.item.status != "confirmed"
            || view.plan.item.auto != "merge"
        {
            continue;
        }
        for task in &view.tasks {
            if task.changed_outside_arc || task.item.state != TaskState::Review {
                continue;
            }
            let phase = task.item.phase.as_str();
            if !matches!(phase, "awaiting_human" | "integrating" | "ready_to_merge") {
                continue;
            }
            match crate::review::merge_as(store, &task.item.id, Some("auto")) {
                Ok(merged) => {
                    report.transitions.push(merged.line());
                    changed = true;
                    if merged.phase == "rework_pending"
                        && let Some(paused) = attempt_ops::record_auto_rework(store, &task.item.id)?
                    {
                        report
                            .transitions
                            .push(format!("{} paused: {}", paused.plan, paused.text));
                        changed = true;
                    }
                }
                Err(Error::Refused(reason)) => {
                    // A moved branch and a changed digest both return the task to
                    // `review.verifying`; that is a transition and the next pass verifies.
                    let after = store.load_task(&task.item.id)?.item;
                    if after.state != TaskState::Review || after.phase != phase {
                        report.transitions.push(format!(
                            "{} merge -> {}.{}: {reason}",
                            task.item.id,
                            after.state.as_str(),
                            after.phase
                        ));
                        changed = true;
                    } else {
                        // The state allows no merge and nothing moved: the summary says why
                        // instead of the loop spinning on it.
                        report
                            .notes
                            .push(format!("{} cannot merge: {reason}", task.item.id));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(changed)
}

/// Whether every task in `after` is `done.merged`.
fn after_merged(store: &Store, task: &model::TaskFile) -> Result<bool, Error> {
    for id in &task.after {
        let loaded = store.load_task(id)?;
        if loaded.changed_outside_arc
            || loaded.item.state != TaskState::Done
            || loaded.item.phase != "merged"
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How many tasks are in `doing`.
fn count_doing(store: &Store) -> Result<u64, Error> {
    let mut count = 0;
    for id in store.task_ids()? {
        if store.load_task(&id)?.item.state == TaskState::Doing {
            count += 1;
        }
    }
    Ok(count)
}

/// The final summary: for each plan and task that is not merged, what it waits on.
fn waiting(store: &Store) -> Result<Vec<String>, Error> {
    let mut lines = Vec::new();
    for view in store.plans()? {
        let plan = &view.plan.item;
        let open: Vec<&crate::store::Loaded<model::TaskFile>> = view
            .tasks
            .iter()
            .filter(|task| !(task.item.state == TaskState::Done && task.item.phase == "merged"))
            .collect();
        if open.is_empty() && !view.plan.changed_outside_arc {
            continue;
        }
        let header = if view.plan.changed_outside_arc {
            format!(
                "{} (changed outside arc; run `radian arc adopt {}`)",
                plan.id, plan.id
            )
        } else {
            format!("{} (auto {})", plan.id, plan.auto)
        };
        lines.push(header);
        for task in open {
            let reason = task_reason(store, plan, task)?;
            lines.push(format!(
                "  {} {}.{}: {reason}",
                task.item.id,
                task.item.state.as_str(),
                task.item.phase
            ));
        }
    }
    if lines.is_empty() {
        lines.push("nothing waits".to_owned());
    }
    Ok(lines)
}

fn task_reason(
    store: &Store,
    plan: &model::PlanFile,
    task: &crate::store::Loaded<model::TaskFile>,
) -> Result<String, Error> {
    let item = &task.item;
    let reason = match (item.state, item.phase.as_str()) {
        (TaskState::Todo, "ready") => {
            if plan.auto == "off" {
                "waiting for you to start it (`auto` is `off`)".to_owned()
            } else {
                let missing: Vec<&str> = item
                    .after
                    .iter()
                    .filter(|id| {
                        store.load_task(id).is_ok_and(|loaded| {
                            loaded.item.state != TaskState::Done || loaded.item.phase != "merged"
                        })
                    })
                    .map(String::as_str)
                    .collect();
                if missing.is_empty() {
                    "waiting for a free concurrency slot".to_owned()
                } else {
                    format!("waiting for {} to merge", missing.join(", "))
                }
            }
        }
        (TaskState::Todo, "retry_wait") => format!(
            "waiting for its retry at {}",
            item.retry_at.as_deref().unwrap_or("an unrecorded time")
        ),
        (TaskState::Todo, "paused_budget") => "waiting for `radian arc reset-budget`".to_owned(),
        (TaskState::Todo, _) => "waiting for its `after` tasks".to_owned(),
        (TaskState::Doing, "starting" | "running") => "running its attempt".to_owned(),
        (TaskState::Doing, _) => "needs you: arc cannot prove what the worker did".to_owned(),
        (TaskState::Review, "verifying") => match crate::review::rework_reason(store, item) {
            Some(reason) => format!("verification cannot run: {reason}"),
            None => "waiting to be verified".to_owned(),
        },
        (TaskState::Review, "rework_pending") => match crate::review::rework_reason(store, item) {
            Some(reason) => format!("waiting for a rework brief from the lead ({reason})"),
            None => "waiting for a rework brief from the lead".to_owned(),
        },
        (TaskState::Review, "awaiting_human") => {
            if plan.auto == "merge" {
                "waiting: its verification passed".to_owned()
            } else {
                "waiting for you to merge or reject it".to_owned()
            }
        }
        (TaskState::Review, "integrating" | "ready_to_merge") => {
            "waiting for the integration to finish (run `radian arc merge`)".to_owned()
        }
        (TaskState::Review, _) => "waiting in review".to_owned(),
        (TaskState::Failed, phase) => format!("failed.{phase}; nothing starts it again"),
        (TaskState::Done, phase) => format!("done.{phase}"),
    };
    Ok(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_no_transition_says_so_and_lists_the_summary() {
        let report = Report {
            transitions: Vec::new(),
            waiting: vec!["nothing waits".to_owned()],
            notes: Vec::new(),
        };
        assert_eq!(report.render(), "no transition\nsummary\nnothing waits\n");
    }
}

//! The transition poller the MCP channel and the hub's event stream share (ADR 0048
//! decisions 11 and 16).
//!
//! A [`Poller`] reads a list of repositories, compares each read with the one before it, and
//! returns the transitions the closed lists below name. The MCP server feeds it its one store
//! and pushes each transition over the channel; the hub feeds it every registered store and
//! writes each one to its event stream. Neither adds a transition of its own: a change outside
//! these lists is silent in both.
//!
//! The poller holds one snapshot per repository name. The first read of a name is the
//! baseline and sends nothing; a name that leaves the list is forgotten, so a repository
//! registered while a stream is open starts with its own baseline and a removed one stops
//! occupying memory. A read that fails sends nothing that round and is retried next round; it
//! never ends the caller.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

use serde_json::Value;

use crate::store::model::TaskFile;
use crate::store::plan_ops::{CONFLICT_REWORK_SUFFIX, REVIEW_REWORK_SUFFIX, REWORK_HEADING};
use crate::store::{self, Error, Store};

/// How often a caller reads every plan and its tasks.
pub const CHANNEL_INTERVAL_SECS: u64 = 5;

/// The phases that cost the lead a turn when a task enters them. Every other change goes
/// unsent on purpose: each event starts one of the lead's model turns. This is the closed
/// list the manual's drift test reads.
pub const CHANNEL_PHASES: &[&str] = &[
    "todo.proposed",
    "review.awaiting_human",
    "review.rework_pending",
    "done.merged",
    "failed.exhausted",
    "failed.cancelled",
    "failed.irrecoverable",
];

/// The previous phases whose move into `todo.ready` also sends a task event: the owner's
/// confirm of a proposed task, and the lead's `add_rework` of a task in a `review.*` phase. The
/// owner's reject is not one of these: it moves the task to `review.rework_pending`, which
/// [`CHANNEL_PHASES`] sends with the note as its second line. An entry that ends in `.*` is a
/// prefix. This is the second closed task list the manual's drift test reads.
pub const CHANNEL_READY_FROM: &[&str] = &["todo.proposed", "review.*"];

/// The plan fields that cost the lead a turn when they change: the plan's `status`, and
/// whether it is `paused` (a `confirmed` plan whose `auto` is `off`). This is the closed list
/// the manual's drift test reads.
pub const CHANNEL_PLAN_FIELDS: &[&str] = &["status", "paused"];

/// One repository the poller reads: the name the caller knows it by, and its store.
#[derive(Debug, Clone)]
pub struct Repo {
    pub name: String,
    pub store: Store,
}

/// One transition, with the fields the MCP channel's meta carries.
#[derive(Debug, Clone)]
pub enum Transition {
    /// A task's move to `task.full` from `previous` (`new` for a task the previous read did
    /// not have).
    Task {
        id: String,
        task: TaskSnapshot,
        previous: String,
    },
    /// A plan's `status` or `paused` change.
    Plan { id: String, event: PlanEvent },
}

impl Transition {
    /// The task or plan id, whichever kind this is.
    pub fn id(&self) -> &str {
        match self {
            Transition::Task { id, .. } | Transition::Plan { id, .. } => id,
        }
    }
}

/// One task as the previous poll read it. `full` is `state.phase`, the value the channel
/// compares and reports; `state` and `phase` are its two parts for the meta.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub plan: String,
    pub state: String,
    pub phase: String,
    pub full: String,
    /// Whether the newest rework heading is one arc wrote itself (a conflict with main, or a
    /// review that failed with blocking items), so the poller can tell it from the owner's or
    /// the lead's move into `todo.ready`.
    arc_rework: bool,
}

/// One plan as the previous poll read it. `paused` is a `confirmed` plan whose `auto` is
/// `off`: a `draft` plan waits for the owner's confirm and is not paused. `pause_record` is
/// the record that belongs to the pause the poller knows about: the one a read found while
/// the plan was paused, the one the automatic rework limit has written but not applied yet,
/// or a record an earlier pause left behind (see [`carry_pause_records`]).
#[derive(Debug, Clone)]
struct PlanSnapshot {
    status: String,
    paused: bool,
    /// A plan that is `cancelled`, by its status or by its tasks, is final and sends nothing.
    cancelled: bool,
    /// The record the automatic rework limit wrote (decision 12), when a task still carries
    /// one on its current attempt. The text goes only with the pause that record caused: the
    /// record stays after the owner resumes, so a later `radian arc pause` must not repeat it.
    pause_record: Option<PauseRecord>,
}

/// The newest `paused.json` record the automatic rework limit wrote for a plan: which record,
/// the reason it carries, and whether its pause has been applied yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PauseRecord {
    /// The task, its attempt and the record's `at`: the same record read twice has the same
    /// identity, and a later attempt's record has a different one.
    id: String,
    /// The record's `text`, the pause's reason line.
    text: String,
    /// Whether the automatic rework limit has not applied this record's pause yet: it writes
    /// the record before it sets `auto` to `off` and marks the record applied after. A
    /// record a running plan shows is still waiting for its pause only while it is pending;
    /// one marked applied was read after that pause (and possibly its resume), so it is a
    /// pause the poller missed, not the one that follows.
    pending: bool,
}

/// Every plan and task one store holds, as one poll reads them.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    tasks: BTreeMap<String, TaskSnapshot>,
    plans: BTreeMap<String, PlanSnapshot>,
}

/// The transition poller: one snapshot per repository name, so a repository that appears or
/// disappears between two reads needs no coordination with the caller.
#[derive(Debug, Default)]
pub struct Poller {
    previous: BTreeMap<String, Snapshot>,
}

impl Poller {
    /// A poller with no baseline: the first read of every repository sends nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads every repository and returns the transitions since the previous call, each with
    /// the name it was read under. Repositories are read and reported in `repos` order; a
    /// repository absent from `repos` is forgotten, and one that appears starts with its own
    /// baseline. A read that fails sends nothing for that repository that round.
    pub fn poll(&mut self, repos: &[Repo]) -> Vec<(String, Transition)> {
        let present: BTreeSet<&str> = repos.iter().map(|repo| repo.name.as_str()).collect();
        self.previous
            .retain(|name, _| present.contains(name.as_str()));
        let mut transitions = Vec::new();
        for repo in repos {
            let Ok(mut current) = snapshot(&repo.store) else {
                continue;
            };
            let before = self.previous.get(&repo.name);
            carry_pause_records(before.map(|before| &before.plans), &mut current.plans);
            if let Some(before) = before {
                for (id, task, previous) in task_transitions(&before.tasks, &current.tasks) {
                    transitions.push((repo.name.clone(), Transition::Task { id, task, previous }));
                }
                for (id, event) in plan_transitions(&before.plans, &current.plans) {
                    transitions.push((repo.name.clone(), Transition::Plan { id, event }));
                }
            }
            self.previous.insert(repo.name.clone(), current);
        }
        transitions
    }
}

/// Every plan and task the store currently holds, by id. The read is the same read-only call
/// the CLI's `list` uses: it takes no lock.
fn snapshot(store: &Store) -> Result<Snapshot, Error> {
    let mut snapshot = Snapshot::default();
    for view in store.plans()? {
        // A file someone changed outside arc makes the round fail: arc refuses to act on it,
        // and the channel sends nothing from a read it cannot trust. The next round tries
        // again (the owner may adopt it).
        if view.plan.changed_outside_arc {
            return Err(Error::ChangedOutsideArc(view.plan.item.id.clone()));
        }
        let plan = &view.plan.item;
        let cancelled = view.is_cancelled();
        let paused = plan.status == "confirmed" && plan.auto == "off";
        snapshot.plans.insert(
            plan.id.clone(),
            PlanSnapshot {
                status: plan.status.clone(),
                paused,
                cancelled,
                pause_record: pause_record(store, &plan.id, &view.tasks),
            },
        );
        for task in &view.tasks {
            if task.changed_outside_arc {
                return Err(Error::ChangedOutsideArc(task.item.id.clone()));
            }
            let state = task.item.state.as_str().to_owned();
            let phase = task.item.phase.clone();
            snapshot.tasks.insert(
                task.item.id.clone(),
                TaskSnapshot {
                    plan: view.plan.item.id.clone(),
                    full: format!("{state}.{phase}"),
                    state,
                    phase,
                    arc_rework: arc_wrote_rework(&task.item.body),
                },
            );
        }
    }
    Ok(snapshot)
}

/// The newest `paused.json` record the automatic rework limit left for `plan` on one of its
/// tasks' current attempts: the record's identity and its `text` (decision 12). `radian arc
/// pause` records none. The record stays on its attempt after the owner resumes, so a caller
/// compares it with the record that belongs to the previous pause before it trusts the text
/// as this pause's reason; another task's older record is not this pause's reason either.
fn pause_record(
    store: &Store,
    plan: &str,
    tasks: &[store::Loaded<TaskFile>],
) -> Option<PauseRecord> {
    let mut newest: Option<(String, PauseRecord)> = None;
    for task in tasks {
        let path = crate::store::attempt_ops::run_dir(store, &task.item.id, task.item.attempt)
            .join(crate::store::attempt_ops::PAUSED_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("plan").and_then(Value::as_str) != Some(plan) {
            continue;
        }
        let Some(text) = value.get("text").and_then(Value::as_str) else {
            continue;
        };
        let at = value
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let record = PauseRecord {
            id: format!("{}@{}@{at}", task.item.id, task.item.attempt),
            text: text.to_owned(),
            // A record without the marker (one an older arc wrote) is one whose pause has
            // been applied: it must not become a pause's reason again.
            pending: value
                .get("pending")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        if newest
            .as_ref()
            .is_none_or(|(newest_at, _)| at >= *newest_at)
        {
            newest = Some((at, record));
        }
    }
    newest.map(|(_, record)| record)
}

/// Whether the newest `## Rework <n>` heading of a task body is one arc wrote itself. The
/// lead's `add_rework` and the owner's reject leave the heading without a suffix; a conflict
/// with main and a review that failed with blocking items carry one (decision 9).
fn arc_wrote_rework(body: &str) -> bool {
    body.lines()
        .rfind(|line| line.starts_with(REWORK_HEADING))
        .is_some_and(|line| {
            line.ends_with(CONFLICT_REWORK_SUFFIX) || line.ends_with(REVIEW_REWORK_SUFFIX)
        })
}

/// The task events to send, with the phase each task had before (`new` for a task the previous
/// read did not have). `previous` is a task map, not a phase map, so a task that moved to a
/// different plan is still one task.
fn task_transitions(
    previous: &BTreeMap<String, TaskSnapshot>,
    current: &BTreeMap<String, TaskSnapshot>,
) -> Vec<(String, TaskSnapshot, String)> {
    let mut events = Vec::new();
    for (task_id, task) in current {
        let before = previous
            .get(task_id)
            .map(|task| task.full.clone())
            .unwrap_or_else(|| "new".to_owned());
        if !sends_task(&before, task) {
            continue;
        }
        events.push((task_id.clone(), task.clone(), before));
    }
    events
}

/// Whether a task's move from `before` to its current phase sends an event.
fn sends_task(before: &str, task: &TaskSnapshot) -> bool {
    if before == task.full {
        return false;
    }
    if CHANNEL_PHASES.contains(&task.full.as_str()) {
        return true;
    }
    if !ready_from(before) || task.full != "todo.ready" {
        return false;
    }
    // The owner's reject leaves the task in `review.rework_pending` (decision 6), so the phase
    // list above sends it, note and all. A move into `todo.ready` from a `review.*` phase is the
    // lead's `add_rework`, and a rework arc wrote itself sends nothing.
    !task.arc_rework
}

/// Whether `before` is one of [`CHANNEL_READY_FROM`]'s phases. An entry ending in `.*` is a
/// prefix.
fn ready_from(before: &str) -> bool {
    CHANNEL_READY_FROM
        .iter()
        .any(|from| match from.strip_suffix(".*") {
            Some(prefix) => before.starts_with(prefix),
            None => before == *from,
        })
}

/// One plan event: the field, the two values `meta` carries, and the reason line to put under
/// them when the event is a pause the automatic rework limit caused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    pub field: &'static str,
    pub previous: String,
    pub value: String,
    pub reason: Option<String>,
}

/// The plan events to send. A plan the previous read did not have sends nothing, and neither
/// does a cancelled one.
fn plan_transitions(
    previous: &BTreeMap<String, PlanSnapshot>,
    current: &BTreeMap<String, PlanSnapshot>,
) -> Vec<(String, PlanEvent)> {
    let mut events = Vec::new();
    for (plan_id, plan) in current {
        let Some(before) = previous.get(plan_id) else {
            continue;
        };
        // A cancelled plan is final (decision 3): it sends no plan event.
        if plan.cancelled {
            continue;
        }
        if before.status == "draft" && plan.status == "confirmed" {
            events.push((
                plan_id.clone(),
                PlanEvent {
                    field: "status",
                    previous: before.status.clone(),
                    value: plan.status.clone(),
                    reason: None,
                },
            ));
        }
        // A plan whose `status` changed is not paused or resumed by this read: `update_plan`'s
        // change record returns a confirmed plan to `draft` and sets `auto` to `off` in the
        // same write, and a confirm takes a draft plan to `confirmed`. Neither is a pause.
        if before.status == plan.status && before.paused != plan.paused {
            events.push((
                plan_id.clone(),
                PlanEvent {
                    field: "paused",
                    previous: bool_text(before.paused).to_owned(),
                    value: bool_text(plan.paused).to_owned(),
                    reason: pause_reason(before, plan),
                },
            ));
        }
    }
    events
}

/// Keeps each plan's pause record with the knowledge the poller already has about it.
///
/// A read that finds the plan paused keeps the record it found, and that record's pause has
/// now been reported: it is no longer pending, whatever the record says. A read that finds
/// the plan running keeps the record the previous read knew when it is the same record, and
/// the record is still pending only while both reads say so. A record the read finds for the
/// first time takes its `pending` marker from the store: the automatic rework limit writes the
/// record before it sets `auto` to `off` and marks it applied after, so a record a running
/// plan still shows as pending belongs to the pause that follows, while one it marks applied
/// was read after that pause and is not its reason. The poller's first read has no previous
/// read to take the marker from, so the marker is what tells a pause in progress from a
/// record an earlier pause left behind.
fn carry_pause_records(
    previous: Option<&BTreeMap<String, PlanSnapshot>>,
    current: &mut BTreeMap<String, PlanSnapshot>,
) {
    for (plan_id, plan) in current.iter_mut() {
        if plan.paused {
            if let Some(record) = &mut plan.pause_record {
                record.pending = false;
            }
            continue;
        }
        let Some(before) = previous.and_then(|previous| previous.get(plan_id)) else {
            continue;
        };
        if let (Some(found), Some(before)) = (&plan.pause_record, &before.pause_record)
            && found.id == before.id
        {
            // The marker says the pause is still to come only while both reads say so: a
            // read that already reported the pause does not resurrect the marker, and a
            // store that no longer marks the record pending says its pause has been applied
            // (or its plan write failed, leaving no pause to report).
            plan.pause_record = Some(PauseRecord {
                pending: before.pending && found.pending,
                ..before.clone()
            });
        }
    }
}

/// The reason line of a pause event: the text of the record that caused this pause. The
/// automatic rework limit writes the record just before it sets `auto` to `off`, so the record
/// the paused read finds is this pause's cause. A record from an earlier pause stays on its
/// attempt after the owner resumes; [`carry_pause_records`] keeps it as an applied record, so
/// a later `radian arc pause` sends no second line. A record a running read saw while it was
/// still pending is the one the pause that follows caused, so it is reported even though the
/// previous read already carried it.
fn pause_reason(before: &PlanSnapshot, plan: &PlanSnapshot) -> Option<String> {
    if !plan.paused {
        return None;
    }
    let record = plan.pause_record.as_ref()?;
    if let Some(before) = &before.pause_record
        && before.id == record.id
        && !before.pending
    {
        return None;
    }
    Some(record.text.clone())
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// The delay between two reads. It is the channel's cadence and the hub's event-stream
/// cadence, so both see a transition within the same window.
pub fn poll_interval() -> Duration {
    Duration::from_secs(CHANNEL_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_record_seen_while_the_plan_still_ran_is_kept_for_its_pause() {
        let record = "the plan paused: task TK0001-a used 3 automatic reworks";
        let plan = |paused: bool, record: Option<(&str, bool)>| {
            BTreeMap::from([(
                "PL0001-a".to_owned(),
                PlanSnapshot {
                    status: "confirmed".to_owned(),
                    paused,
                    cancelled: false,
                    pause_record: record.map(|(text, pending)| PauseRecord {
                        id: "TK0001-a@0@2026-01-01T00:00:00Z".to_owned(),
                        text: text.to_owned(),
                        pending,
                    }),
                },
            )])
        };

        // The automatic rework limit writes the record first and sets `auto` to `off` after
        // marking it applied. The read in that window finds a pending record on a plan that is
        // not paused; the paused read after it must still report the record as the pause's
        // reason.
        let before = plan(false, None);
        let mut racing = plan(false, Some((record, true)));
        carry_pause_records(Some(&before), &mut racing);
        let paused = plan(true, Some((record, true)));
        let events = plan_transitions(&racing, &paused);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].1.field, "paused");
        assert_eq!(events[0].1.reason.as_deref(), Some(record));

        // The poller's first read can land in that window too: the client connects after
        // the record is written and before `auto` goes `off`. That read has no previous
        // read to take the marker from, so the record's own `pending` marker is what tells
        // it the pause is still to come; the pause that follows reports it.
        let mut baseline = plan(false, Some((record, true)));
        carry_pause_records(None, &mut baseline);
        let paused = plan(true, Some((record, true)));
        let events = plan_transitions(&baseline, &paused);
        assert_eq!(events[0].1.field, "paused");
        assert_eq!(events[0].1.reason.as_deref(), Some(record));

        // A pause the poller already reported is not repeated by a later manual pause: the
        // record carried out of the resume is an applied one.
        let resumed = plan(false, Some((record, false)));
        let manual = plan(true, Some((record, false)));
        assert_eq!(plan_transitions(&resumed, &manual)[0].1.reason, None);

        // A client whose first read lands after that pause was resumed finds the applied
        // record on a running plan. It belongs to the pause the client missed, not to the
        // owner's manual pause that follows.
        let mut stale = plan(false, Some((record, false)));
        carry_pause_records(None, &mut stale);
        let manual = plan(true, Some((record, false)));
        assert_eq!(
            plan_transitions(&stale, &manual)[0].1.reason,
            None,
            "a record an earlier pause left behind is not a manual pause's reason"
        );

        // A plan write that failed leaves the record marked applied on a plan that still
        // runs. A read that carried the record as pending must not keep it pending: no pause
        // is left to report, so the owner's next pause sends no second line.
        let carried = plan(false, Some((record, true)));
        let mut failed = plan(false, Some((record, false)));
        carry_pause_records(Some(&carried), &mut failed);
        let manual = plan(true, Some((record, false)));
        assert_eq!(plan_transitions(&failed, &manual)[0].1.reason, None);

        // The other way round: a pause the poller reported stays reported even when the
        // store still marks its record pending (the write that clears the marker failed).
        let reported = plan(false, Some((record, false)));
        let mut still_pending = plan(false, Some((record, true)));
        carry_pause_records(Some(&reported), &mut still_pending);
        let manual = plan(true, Some((record, false)));
        assert_eq!(plan_transitions(&still_pending, &manual)[0].1.reason, None);
    }

    /// One store with a plan and a task, at `state.phase`, for the baseline and transition
    /// checks. The store lives in a scratch directory the test removes.
    #[test]
    fn a_new_repository_is_its_own_baseline_and_a_removed_one_is_forgotten() {
        use std::path::PathBuf;

        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "radian-arc-transitions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        )));
        fs::create_dir_all(scratch.0.join(crate::store::ARC_DIR)).expect("the arc dir");
        let store = Store::at(scratch.0.clone());

        // A poller with no baseline reports nothing for a repository it reads for the first
        // time, even when that repository already holds a plan and a task.
        let repo = Repo {
            name: "one".to_owned(),
            store: store.clone(),
        };
        let mut poller = Poller::new();
        assert!(poller.poll(std::slice::from_ref(&repo)).is_empty());

        // A read that fails is retried next round and sends nothing meanwhile.
        let mut gone = Poller::new();
        let missing = Repo {
            name: "missing".to_owned(),
            store: Store::at(scratch.0.join("not-a-repository")),
        };
        assert!(gone.poll(&[missing]).is_empty());

        // A repository absent from the next list is forgotten, so a name that returns later
        // starts from a fresh baseline instead of replaying the time it was gone.
        assert!(poller.poll(&[]).is_empty());
        assert!(poller.poll(std::slice::from_ref(&repo)).is_empty());
    }
}

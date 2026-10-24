//! The `after` dependency chain of one task, both ways: the tasks it waits on and the tasks
//! that wait on it (ADR 0048 decisions 3 and 6).
//!
//! A task's `after` lists the tasks that must be merged before it is claimed. This module is
//! the one place that walks that graph, so `show`, `show --json`, `confirm`, its `--dry-run`
//! and the MCP `show` tool all report the same chain. The walk is breadth-first, nearest
//! first, and visits each task once, so a hand-edited cycle cannot loop.
//!
//! A task that can never reach `done.merged` stops the chain for good: a `failed.*` task, a
//! task of a `cancelled` plan, or an `after` id that names no task at all (possible only in a
//! file a person edited and then adopted). A cycle is reported once and the walk stops there.

use std::collections::{HashMap, HashSet, VecDeque};

use super::model::{self, TaskFile, TaskState};
use super::{Error, Store};

/// One task in a chain, as `<id> <state>.<phase>`, or `<id> missing` when the id names no
/// stored task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// The task's state, or `None` when no task file has this id.
    pub state: Option<String>,
    /// The task's phase, or `None` when no task file has this id.
    pub phase: Option<String>,
}

impl Entry {
    /// The text form: `<id> <state>.<phase>`, or `<id> missing`.
    pub fn text(&self) -> String {
        match (&self.state, &self.phase) {
            (Some(state), Some(phase)) => format!("{} {state}.{phase}", self.id),
            _ => format!("{} missing", self.id),
        }
    }

    /// The JSON form: `{"id", "state", "phase"}`, with `null` state and phase for an id that
    /// names no task.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "state": self.state,
            "phase": self.phase,
        })
    }
}

/// Everything `show` and `confirm` report about one task's dependencies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chain {
    /// Every task in the `after` chain, nearest first, each once.
    pub waits_on: Vec<Entry>,
    /// Every other task whose `after` chain contains this task, nearest first, each once.
    pub blocks: Vec<Entry>,
    /// The first task in [`Chain::waits_on`] that can never become `done.merged`.
    pub never_runs: Option<Entry>,
    /// One `after` cycle, as `[a, b, a]`, or `None`.
    pub cycle: Option<Vec<String>>,
}

/// Computes the chain of `task_id` (see [`Chain`]).
pub fn chain(store: &Store, task_id: &str) -> Result<Chain, Error> {
    let (waits_on, never_runs) = forward(store, task_id)?;
    Ok(Chain {
        waits_on,
        blocks: reverse(store, task_id)?,
        never_runs,
        cycle: find_cycle(store, task_id),
    })
}

/// The wait chain and the first entry that can never finish.
fn forward(store: &Store, task_id: &str) -> Result<(Vec<Entry>, Option<Entry>), Error> {
    let mut waits_on = Vec::new();
    let mut never_runs = None;
    let mut seen: HashSet<String> = HashSet::from([task_id.to_owned()]);
    let mut queue: VecDeque<String> = after_of(store, task_id).into();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let loaded = load(store, &id);
        let entry = match &loaded {
            Some(task) => Entry {
                id: id.clone(),
                state: Some(task.state.as_str().to_owned()),
                phase: Some(task.phase.clone()),
            },
            None => Entry {
                id: id.clone(),
                state: None,
                phase: None,
            },
        };
        if never_runs.is_none() && never_finishes(store, loaded.as_ref()) {
            never_runs = Some(entry.clone());
        }
        waits_on.push(entry);
        if let Some(task) = &loaded {
            for next in &task.after {
                if !seen.contains(next) {
                    queue.push_back(next.clone());
                }
            }
        }
    }
    Ok((waits_on, never_runs))
}

/// Whether the task a chain entry names can never reach `done.merged`: a missing file, a
/// `failed.*` task, or a task of a `cancelled` plan that is not already merged. A plan is
/// cancelled by its `status` or by all its tasks being `failed.cancelled`
/// ([`Store::plan_is_cancelled`]).
fn never_finishes(store: &Store, task: Option<&TaskFile>) -> bool {
    let Some(task) = task else {
        return true;
    };
    if task.state == TaskState::Failed {
        return true;
    }
    if task.state == TaskState::Done {
        return false;
    }
    matches!(store.plan_is_cancelled(&task.plan), Ok(true))
}

/// The tasks whose `after` chain contains `task_id`, nearest first.
fn reverse(store: &Store, task_id: &str) -> Result<Vec<Entry>, Error> {
    // Every stored task's `after` edges, reversed. `task_ids` is sorted, so two tasks at the
    // same distance are reported in id order.
    let mut blocking: HashMap<String, Vec<String>> = HashMap::new();
    for id in store.task_ids()? {
        if let Some(task) = load(store, &id) {
            for dependency in &task.after {
                blocking
                    .entry(dependency.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
    }
    let mut blocks = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([task_id.to_owned()]);
    let mut queue: VecDeque<String> = blocking.get(task_id).cloned().unwrap_or_default().into();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(task) = load(store, &id) {
            blocks.push(Entry {
                id: id.clone(),
                state: Some(task.state.as_str().to_owned()),
                phase: Some(task.phase.clone()),
            });
            for next in blocking.get(&id).cloned().unwrap_or_default() {
                if !seen.contains(&next) {
                    queue.push_back(next);
                }
            }
        }
    }
    Ok(blocks)
}

/// One cycle reachable over `after` from `task_id`, as `[a, b, a]`.
///
/// The walk is depth-first and keeps the path it is on, so it sees a cycle the breadth-first
/// chain walk only survives: reaching a task that is already on the path closes the cycle,
/// and the walk stops there instead of following it again.
fn find_cycle(store: &Store, task_id: &str) -> Option<Vec<String>> {
    struct Frame {
        id: String,
        after: Vec<String>,
        index: usize,
    }
    let mut path: Vec<String> = vec![task_id.to_owned()];
    let mut on_path: HashSet<String> = HashSet::from([task_id.to_owned()]);
    let mut done: HashSet<String> = HashSet::new();
    let mut stack = vec![Frame {
        id: task_id.to_owned(),
        after: after_of(store, task_id),
        index: 0,
    }];
    while let Some(frame) = stack.last_mut() {
        if frame.index >= frame.after.len() {
            let frame = stack.pop().expect("the frame just read");
            on_path.remove(&frame.id);
            done.insert(frame.id);
            path.pop();
            continue;
        }
        let next = frame.after[frame.index].clone();
        frame.index += 1;
        if on_path.contains(&next) {
            let start = path.iter().position(|id| *id == next).unwrap_or(0);
            let mut cycle = path[start..].to_vec();
            cycle.push(next);
            return Some(cycle);
        }
        if done.contains(&next) {
            continue;
        }
        on_path.insert(next.clone());
        path.push(next.clone());
        let after = after_of(store, &next);
        stack.push(Frame {
            id: next,
            after,
            index: 0,
        });
    }
    None
}

/// A task's `after` list, or an empty list when no task file has its id.
fn after_of(store: &Store, id: &str) -> Vec<String> {
    load(store, id)
        .map(|task| task.after.clone())
        .unwrap_or_default()
}

/// The task file `id` names, or `None` for a malformed or missing id.
fn load(store: &Store, id: &str) -> Option<TaskFile> {
    if !model::valid_id(id) {
        return None;
    }
    store.load_task(id).ok().map(|loaded| loaded.item)
}

/// The place `task_id` holds among every `todo.ready` task plus itself, in claim order (the
/// plan's priority, the plan id, the task's order in the plan): `(n, m)`, 1-based.
///
/// It is where the task would stand at this moment, not a promise.
pub fn queue_position(store: &Store, task_id: &str) -> Result<(usize, usize), Error> {
    let mut order = Vec::new();
    for id in crate::run::task_claim_order(store)? {
        let loaded = store.load_task(&id);
        let ready = loaded.as_ref().is_ok_and(|loaded| {
            loaded.item.state == TaskState::Todo && loaded.item.phase == "ready"
        });
        if ready || (id == task_id && loaded.is_ok()) {
            order.push(id);
        }
    }
    // A task its plan no longer lists (an adopted hand edit) still has a place: the end, where
    // `task_claim_order` sorts it.
    match order.iter().position(|id| id == task_id) {
        Some(index) => Ok((index + 1, order.len())),
        None => Ok((order.len() + 1, order.len() + 1)),
    }
}

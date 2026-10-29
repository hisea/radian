//! The plan changes the owner makes and the rework step the lead makes (ADR 0048
//! decisions 3, 6 and 12).
//!
//! Every function here is one of the store's transactions: it runs inside
//! [`Store::change_plan`] or [`Store::change_task`], so it takes `state.lock`, compares the
//! file with the ledger, refuses a file that changed outside arc, and writes the file, its
//! ledger entry and the `.last.md` copy in one step.
//!
//! - `confirm` moves a `draft` plan to `confirmed`, or a `todo.proposed` task to
//!   `todo.ready`. A plan with no tasks is not worth confirming, so it is refused; a task in
//!   any other state is refused too.
//! - `auto` sets the plan's auto level. Only a `confirmed` plan may leave `off`.
//! - `pause` sets the level back to `off`. It does not look at the tasks: the scheduling
//!   loop that could be running one does not exist yet.
//! - `update_plan` replaces a draft plan's title or request text, and appends a change
//!   record to a confirmed one. A change record returns the plan to `draft` with `auto`
//!   `off`, and the tasks keep their state.
//! - `update_task` replaces a `todo` task's title or original brief, its `after` list,
//!   `allow_noop` or `extra_stages`, keeps the phase, and appends one line to the task's
//!   `## Changes` section (ADR 0048 decision 11).
//! - `add_rework` appends a rework brief to a task in `review` and returns it to
//!   `todo.ready`.

use super::model::{self, TaskState};
use super::{Error, Store, check_after, check_flow, check_stages, check_title};

/// The heading change records are appended under.
pub const CHANGE_RECORDS_HEADING: &str = "## Change records";

/// The heading each rework brief is appended under, with its number.
pub const REWORK_HEADING: &str = "## Rework";

/// The heading a task's `update_task` records are appended under, at the end of the body
/// (ADR 0048 decision 11).
pub const TASK_CHANGES_HEADING: &str = "## Changes";

/// What the heading says after its number when arc, not the lead, wrote the rework: a conflict
/// with main (ADR 0048 decision 9).
pub const CONFLICT_REWORK_SUFFIX: &str = " (conflict with main)";

/// What the heading says after its number when arc wrote the rework because the review stage
/// failed with blocking items (ADR 0048 decision 9).
pub const REVIEW_REWORK_SUFFIX: &str = " (review)";

/// What one `radian arc confirm` confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    /// The plan went from `draft` to `confirmed`.
    Plan(String),
    /// The task went from `todo.proposed` to `todo.ready`.
    Task(String),
}

impl Confirmed {
    /// The one line `radian arc confirm` prints.
    pub fn line(&self) -> String {
        match self {
            Confirmed::Plan(id) => format!("{id} confirmed"),
            Confirmed::Task(id) => format!("{id} todo.ready"),
        }
    }
}

/// What one `radian arc confirm` did, or what its `--dry-run` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// The line the confirmation prints.
    pub confirmed: Confirmed,
    /// The task ids whose `after` chain the confirmation reports: the confirmed task, or a
    /// plan's `todo.ready` tasks in the plan's order.
    pub tasks: Vec<String>,
}

/// `radian arc confirm <id>`: the owner's confirmation of a draft plan or of a proposed task
/// (ADR 0048 decisions 3, 6 and 11).
///
/// A plan is refused when it has no tasks (nothing to deliver) and when it is already
/// confirmed. A task is refused with exit 3 unless it is `todo.proposed`; confirming it makes
/// it `todo.ready` and leaves its plan's `status` and `auto` alone. A file that changed
/// outside arc is refused by the transaction, and an id that names both a plan and a task is
/// refused the way every command refuses it.
pub fn confirm(store: &Store, id: &str) -> Result<Confirmed, Error> {
    confirm_report(store, id).map(|report| report.confirmed)
}

/// [`confirm`], with the task ids whose chain the caller reports as a warning.
pub fn confirm_report(store: &Store, id: &str) -> Result<Confirmation, Error> {
    confirm_with(store, id, true)
}

/// `radian arc confirm --dry-run <id>`: every check [`confirm`] makes, in the same order,
/// with the same refusal and message, and no write at all.
pub fn confirm_dry_run(store: &Store, id: &str) -> Result<Confirmation, Error> {
    confirm_with(store, id, false)
}

/// The one confirm path: `write` decides whether the checked change is saved. The dry run
/// takes `state.lock` (so a held lock refuses it exactly as it refuses a real confirm) and
/// releases it without writing a plan or a task.
fn confirm_with(store: &Store, id: &str, write: bool) -> Result<Confirmation, Error> {
    // A confirmed plan is the owner's promise that its tasks can run, and a confirmed task is
    // the same promise for one task, so the worker's line is checked first: a wrong
    // `[worker] exec_args` entry, or a `[worker] command` program that does not resolve,
    // leaves the plan a draft or the task proposed (ADR 0048 decision 7).
    crate::worker_args::check_config(&store.config()?, store.repo())?;
    match store.resolve_kind(id)? {
        super::Kind::Plan => {
            if write {
                store.change_open_plan(id, |plan| {
                    check_plan(plan)?;
                    plan.status = "confirmed".to_owned();
                    Ok(())
                })?;
            } else {
                store.check_state_lock()?;
                let loaded = store.load_plan(id)?;
                super::require_unchanged(id, loaded.changed_outside_arc)?;
                store.refuse_cancelled(id)?;
                check_plan(&loaded.item)?;
            }
            let plan = store.load_plan(id)?.item;
            Ok(Confirmation {
                confirmed: Confirmed::Plan(id.to_owned()),
                tasks: ready_tasks(store, &plan),
            })
        }
        super::Kind::Task => {
            // A cancelled plan is final, so a proposed task of one cannot be revived.
            store.refuse_cancelled(&store.load_task(id)?.item.plan)?;
            if write {
                store.change_task(id, |task| {
                    check_task(task)?;
                    task.enter_phase("ready");
                    Ok(())
                })?;
            } else {
                store.check_state_lock()?;
                let loaded = store.load_task(id)?;
                super::require_unchanged(id, loaded.changed_outside_arc)?;
                check_task(&loaded.item)?;
            }
            Ok(Confirmation {
                confirmed: Confirmed::Task(id.to_owned()),
                tasks: vec![id.to_owned()],
            })
        }
    }
}

/// The plan checks of a confirm, in the order a refusal names them. The cancelled check comes
/// first and is [`Store::refuse_cancelled`], which both callers make before this one.
fn check_plan(plan: &model::PlanFile) -> Result<(), Error> {
    if plan.status == "confirmed" {
        return Err(Error::Refused(format!(
            "plan `{}` is already `confirmed`",
            plan.id
        )));
    }
    if plan.tasks.is_empty() {
        return Err(Error::Refused(format!(
            "plan `{}` has no tasks; add the tasks that deliver it first",
            plan.id
        )));
    }
    Ok(())
}

/// The task checks of a confirm: only a `todo.proposed` task is confirmed.
fn check_task(task: &model::TaskFile) -> Result<(), Error> {
    if task.state != TaskState::Todo || task.phase != model::PROPOSED_PHASE {
        return Err(Error::Refused(format!(
            "task `{}` is `{}.{}`; only a `todo.{}` task is confirmed",
            task.id,
            task.state.as_str(),
            task.phase,
            model::PROPOSED_PHASE
        )));
    }
    Ok(())
}

/// The plan's `todo.ready` tasks in the plan's order: the tasks a plan confirm reports. A
/// task file that does not read is left out here; `show <plan>` fails on a malformed file and
/// leaves out only a missing one.
fn ready_tasks(store: &Store, plan: &model::PlanFile) -> Vec<String> {
    let mut tasks = Vec::new();
    for id in &plan.tasks {
        if let Ok(task) = store.load_task(id)
            && task.item.state == TaskState::Todo
            && task.item.phase == "ready"
        {
            tasks.push(id.clone());
        }
    }
    tasks
}

/// `radian arc auto <plan> <level>`: the plan's auto level (decision 12). `off` is allowed
/// on any plan; a level that starts work needs a confirmed plan.
pub fn set_auto(store: &Store, plan: &str, level: &str) -> Result<(), Error> {
    if !model::AUTO_LEVELS.contains(&level) {
        return Err(Error::Usage(format!(
            "`{level}` is not an auto level; levels are {}",
            model::AUTO_LEVELS.join(", ")
        )));
    }
    store.change_open_plan(plan, |plan| {
        if level != "off" && plan.status != "confirmed" {
            return Err(Error::Refused(format!(
                "plan `{}` is `{}`; only a confirmed plan can use auto level `{level}`",
                plan.id, plan.status
            )));
        }
        plan.auto = level.to_owned();
        Ok(())
    })?;
    Ok(())
}

/// `radian arc priority <plan> <P0..P5>`: the plan's priority (decision 3).
///
/// A plan in any state can be changed, and the next scheduling pass reads the new value. A
/// value outside [`model::PLAN_PRIORITIES`] is refused with exit 3, the same class as a state
/// refusal, and the message lists every value.
pub fn set_priority(store: &Store, plan: &str, priority: &str) -> Result<(), Error> {
    if !model::PLAN_PRIORITIES.contains(&priority) {
        return Err(Error::Refused(format!(
            "`{priority}` is not a plan priority; priorities are {}",
            model::PLAN_PRIORITIES.join(", ")
        )));
    }
    store.change_open_plan(plan, |plan| {
        plan.priority = Some(priority.to_owned());
        Ok(())
    })?;
    Ok(())
}

/// `radian arc pause <plan>`: the auto level back to `off` (decision 12).
///
/// This build has no scheduling loop, so nothing is running: `pause` only writes the level.
/// A task that is already started keeps its state, and a later loop reads the level before
/// it starts anything.
pub fn pause(store: &Store, plan: &str) -> Result<(), Error> {
    store.change_plan(plan, |plan| {
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    Ok(())
}

/// `update_plan`: the lead's one way to change a plan (decision 3).
///
/// - `draft`: `title` and `request` replace the plan's; a `change_record` is refused,
///   because there is no confirmed text to record a change against.
/// - `confirmed`: `title` and `request` are refused (the request text is never rewritten),
///   and a `change_record` is required. It is appended under [`CHANGE_RECORDS_HEADING`] with
///   the UTC time, and the plan returns to `draft` with `auto` `off`.
pub fn update_plan(
    store: &Store,
    plan: &str,
    title: Option<&str>,
    request: Option<&str>,
    change_record: Option<&str>,
) -> Result<(), Error> {
    if let Some(title) = title
        && title.trim().is_empty()
    {
        return Err(Error::Usage("the title is empty".to_owned()));
    }
    if let Some(record) = change_record
        && record.trim().is_empty()
    {
        return Err(Error::Usage("the change record is empty".to_owned()));
    }
    if title.is_none() && request.is_none() && change_record.is_none() {
        return Err(Error::Usage(
            "nothing to update: give a title or a request for a draft plan, or a change \
             record for a confirmed one"
                .to_owned(),
        ));
    }
    let now = utc_now();
    store.change_open_plan(plan, |plan| {
        if plan.status != "confirmed" {
            if change_record.is_some() {
                return Err(Error::Refused(format!(
                    "plan `{}` is `draft`; a change record amends a confirmed plan",
                    plan.id
                )));
            }
            if let Some(title) = title {
                plan.title = title.trim().to_owned();
            }
            if let Some(request) = request {
                set_request(&mut plan.body, request);
            }
            return Ok(());
        }
        if title.is_some() || request.is_some() {
            return Err(Error::Refused(format!(
                "plan `{}` is `confirmed`; its title and request text are not rewritten, \
                 append a change record instead",
                plan.id
            )));
        }
        let Some(record) = change_record else {
            return Err(Error::Refused(format!(
                "plan `{}` is `confirmed`; give a change record to amend it",
                plan.id
            )));
        };
        append_change_record(&mut plan.body, &now, record);
        plan.status = "draft".to_owned();
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    Ok(())
}

/// `update_task`: the lead's one way to change a task (ADR 0048 decision 11).
///
/// Only a task in `todo` (any phase) of a plan that is not `cancelled` is changed. The phase
/// is kept: a `todo.proposed` task stays proposed and a `todo.ready` one stays ready. The
/// `title`, `after` and `extra_stages` rules are [`super::add_task`]'s, through
/// [`check_after`]; `brief` replaces only the original brief, and every rework section is
/// kept. Each successful call appends one line to the task's [`TASK_CHANGES_HEADING`].
#[allow(clippy::too_many_arguments)]
pub fn update_task(
    store: &Store,
    task: &str,
    title: Option<&str>,
    brief: Option<&str>,
    after: Option<Vec<String>>,
    allow_noop: Option<bool>,
    extra_stages: Option<Vec<String>>,
) -> Result<(), Error> {
    update_task_with_flow(
        store,
        task,
        title,
        brief,
        after,
        allow_noop,
        extra_stages,
        None,
    )
}

/// [`update_task`] with the task's `flow` (ADR 0048 decision 15). A flow changes only while
/// the task has had no attempt; after that, a new task is the way to run a different worker.
#[allow(clippy::too_many_arguments)]
pub fn update_task_with_flow(
    store: &Store,
    task: &str,
    title: Option<&str>,
    brief: Option<&str>,
    after: Option<Vec<String>>,
    allow_noop: Option<bool>,
    extra_stages: Option<Vec<String>>,
    flow: Option<&str>,
) -> Result<(), Error> {
    if title.is_none()
        && brief.is_none()
        && after.is_none()
        && allow_noop.is_none()
        && extra_stages.is_none()
        && flow.is_none()
    {
        return Err(Error::Usage(
            "nothing to update: give a title, a brief, an after list, allow_noop, \
             extra_stages or flow"
                .to_owned(),
        ));
    }
    if let Some(title) = title {
        check_title(title)?;
    }
    if let Some(brief) = brief
        && brief.trim().is_empty()
    {
        return Err(Error::Usage("the brief is empty".to_owned()));
    }
    if let Some(stages) = &extra_stages {
        check_stages(stages)?;
    }
    check_flow(store, flow)?;
    // A cancelled plan is final: its tasks take no change either. The one question every
    // command asks: the plan's `status`, or its tasks all `failed.cancelled`.
    store.refuse_cancelled(&store.load_task(task)?.item.plan)?;
    let now = utc_now();
    store.change_task(task, |task_file| {
        if task_file.state != TaskState::Todo {
            if task_file.state == TaskState::Review {
                return Err(Error::Refused(format!(
                    "task `{}` is `review.{}`; add a rework brief with `add_rework`",
                    task_file.id, task_file.phase
                )));
            }
            return Err(Error::Refused(format!(
                "task `{}` is `{}.{}`; `update_task` changes only a `todo` task",
                task_file.id,
                task_file.state.as_str(),
                task_file.phase
            )));
        }
        if let Some(after) = &after {
            check_after(store, &task_file.plan, Some(&task_file.id), after)?;
        }
        let mut changed = Vec::new();
        if let Some(title) = title {
            task_file.title = title.trim().to_owned();
            changed.push("title");
        }
        if let Some(brief) = brief {
            set_brief(&mut task_file.body, brief);
            changed.push("brief");
        }
        if let Some(after) = after {
            task_file.after = after;
            changed.push("after");
        }
        if let Some(allow_noop) = allow_noop {
            task_file.allow_noop = Some(allow_noop);
            changed.push("allow_noop");
        }
        if let Some(stages) = extra_stages {
            task_file.extra_stages = stages;
            changed.push("extra_stages");
        }
        if let Some(flow) = flow {
            let current = task_file
                .flow
                .as_deref()
                .unwrap_or(super::config::DEFAULT_FLOW);
            if current != flow {
                if task_file.attempt != 0 {
                    return Err(Error::Refused(format!(
                        "task `{}` has {} attempt(s); a task's flow is fixed after its first \
                         attempt, so add a new task instead",
                        task_file.id, task_file.attempt
                    )));
                }
                task_file.flow = Some(flow.to_owned());
                changed.push("flow");
            }
        }
        append_task_change(&mut task_file.body, &now, &changed);
        Ok(())
    })?;
    Ok(())
}

/// `add_rework`: the lead's rework brief for a task in `review` (decision 6).
///
/// The brief goes under `<REWORK_HEADING> <n>` with `n` one past the task's attempt, and the
/// task moves to `todo.ready`. New tasks are refused: a rework brief continues the task's
/// session, so it belongs to the task that session ran.
pub fn add_rework(store: &Store, task: &str, brief: &str) -> Result<(), Error> {
    add_rework_with(store, task, brief, "")
}

/// [`add_rework`] with a suffix after the heading's number. The lead's tool passes `""`; the
/// reworks arc writes itself pass [`CONFLICT_REWORK_SUFFIX`] or [`REVIEW_REWORK_SUFFIX`], so a
/// reader can tell the two apart (ADR 0048 decision 9).
pub fn add_rework_with(
    store: &Store,
    task: &str,
    brief: &str,
    heading_suffix: &str,
) -> Result<(), Error> {
    if brief.trim().is_empty() {
        return Err(Error::Usage("the rework brief is empty".to_owned()));
    }
    // A cancelled plan is final: its tasks take no rework either.
    store.refuse_cancelled(&store.load_task(task)?.item.plan)?;
    store.change_task(task, |task| {
        if task.state != TaskState::Review {
            return Err(Error::Refused(format!(
                "task `{}` is `{}.{}`; a rework brief applies only to a task in `review`",
                task.id,
                task.state.as_str(),
                task.phase
            )));
        }
        let number = task.attempt + 1;
        append_rework(&mut task.body, number, brief, heading_suffix);
        task.enter(TaskState::Todo, "ready");
        // The lead's and the review's new instructions get their own retries (ADR 0048
        // decision 9). A conflict rework continues the same line of work, so it keeps the
        // count: `max_attempts` then bounds a task whose conflicts keep returning it to
        // `todo.ready` while its worker keeps failing. Without this, a conflict rework and a
        // retry reset each other's limit forever.
        if heading_suffix != CONFLICT_REWORK_SUFFIX {
            task.retry_count = 0;
        }
        Ok(())
    })?;
    // The lead's own rework is a person stepping in, so the task gets a fresh run of automatic
    // conflict reworks too. arc's review and conflict reworks carry a suffix and keep the run.
    if heading_suffix.is_empty() {
        crate::review::reset_conflict_rework(store, task)?;
    }
    Ok(())
}

/// Replaces the request text of a plan body and keeps its change records.
fn set_request(body: &mut String, request: &str) {
    let records = change_records(body).to_owned();
    *body = model::plan_body(request.trim_end());
    if !records.trim().is_empty() {
        body.push_str(records.trim_start_matches('\n'));
    }
}

/// The text after the change-records heading, or `""` when the plan has no such section.
fn change_records(body: &str) -> &str {
    body.find(CHANGE_RECORDS_HEADING)
        .map(|at| &body[at + CHANGE_RECORDS_HEADING.len()..])
        .unwrap_or("")
}

/// Appends one change record under the plan's change-records heading.
fn append_change_record(body: &mut String, now: &str, record: &str) {
    if !body.contains(CHANGE_RECORDS_HEADING) {
        let head = body.trim_end().to_owned();
        body.clear();
        body.push_str(&head);
        body.push_str("\n\n");
        body.push_str(CHANGE_RECORDS_HEADING);
        body.push('\n');
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!("\n### {now}\n\n{}\n", record.trim_end()));
}

/// Appends one rework brief as its own section, with `heading_suffix` after its number. The
/// `## Changes` section stays terminal, so a rework goes above it (ADR 0048 decision 11).
fn append_rework(body: &mut String, number: u32, brief: &str, heading_suffix: &str) {
    let section = format!(
        "{REWORK_HEADING} {number}{heading_suffix}\n\n{}\n",
        brief.trim_end()
    );
    let head = body.trim_end().to_owned();
    body.clear();
    if let Some((start, _)) = changes_section(&head) {
        let before = head[..start].trim_end();
        if !before.is_empty() {
            body.push_str(before);
            body.push_str("\n\n");
        }
        body.push_str(&section);
        body.push('\n');
        body.push_str(&head[start..]);
        body.push('\n');
        return;
    }
    if !head.is_empty() {
        body.push_str(&head);
        body.push_str("\n\n");
    }
    body.push_str(&section);
}

/// Replaces the original brief of a task body and keeps every rework section and the change
/// records below it (ADR 0048 decision 11). The original brief is the text before the first
/// `## Rework <n>` section, or before `## Changes` when the task has no rework yet. A line that
/// begins `## Rework ` but carries no number, such as `## Rework notes`, is part of the brief,
/// not a rework section.
fn set_brief(body: &mut String, brief: &str) {
    let kept_at = [
        rework_section_start(body),
        section_start(body, TASK_CHANGES_HEADING),
    ]
    .into_iter()
    .flatten()
    .min();
    let kept = kept_at.map(|at| body[at..].to_owned()).unwrap_or_default();
    let mut new = model::brief_body(brief);
    if !kept.is_empty() {
        if !new.is_empty() {
            new.push('\n');
        }
        new.push_str(&kept);
    }
    *body = new;
}

/// Appends one `update_task` line to the body's `## Changes` section, creating the section at
/// the end of the body the first time. `fields` is the call's fields in a fixed order. A
/// `## Changes` section a rework was appended below moves back to the end first, so the records
/// stay the body's last section (ADR 0048 decision 11).
fn append_task_change(body: &mut String, now: &str, fields: &[&str]) {
    move_changes_last(body);
    let mut head = body.trim_end().to_owned();
    if section_start(&head, TASK_CHANGES_HEADING).is_none() {
        if !head.is_empty() {
            head.push_str("\n\n");
        }
        head.push_str(TASK_CHANGES_HEADING);
    }
    head.push('\n');
    head.push_str(&format!("- {now} update_task: {}\n", fields.join(", ")));
    *body = head;
}

/// The `## Changes` section: where it starts and where it ends, which is the next `## ` heading
/// or the body's end.
fn changes_section(body: &str) -> Option<(usize, usize)> {
    let start = section_start(body, TASK_CHANGES_HEADING)?;
    let after_heading = start + TASK_CHANGES_HEADING.len();
    let end = body[after_heading..]
        .find("\n## ")
        .map(|at| after_heading + at + 1)
        .unwrap_or(body.len());
    Some((start, end))
}

/// Moves the body's `## Changes` section to the end when a later section follows it, so the
/// change records are the body's last section (ADR 0048 decision 11).
fn move_changes_last(body: &mut String) {
    let Some((start, end)) = changes_section(body) else {
        return;
    };
    if end == body.len() {
        return;
    }
    let section = body[start..end].trim_end().to_owned();
    let mut rest = body[..start].trim_end().to_owned();
    let after = body[end..].trim_end();
    if !after.is_empty() {
        if !rest.is_empty() {
            rest.push_str("\n\n");
        }
        rest.push_str(after);
    }
    body.clear();
    if !rest.is_empty() {
        body.push_str(&rest);
        body.push_str("\n\n");
    }
    body.push_str(&section);
    body.push('\n');
}

/// The byte offset of the first line that is `heading` or starts with `heading` and a space.
fn section_start(body: &str, heading: &str) -> Option<usize> {
    line_start(body, |text| {
        text == heading
            || text
                .strip_prefix(heading)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// The byte offset of the first numbered rework heading, `## Rework <n>` (with an optional
/// suffix such as ` (review)`), or `None` when the body has none.
fn rework_section_start(body: &str) -> Option<usize> {
    line_start(body, |text| rework_number(text).is_some())
}

/// The number in a `## Rework <n>` heading line, or `None` when the line is not one. The number
/// is the run of digits after the heading and its space; anything after it must start with a
/// space (the suffix [`append_rework`] may add) or be the line's end.
fn rework_number(line: &str) -> Option<u32> {
    let rest = line.strip_prefix(REWORK_HEADING)?.strip_prefix(' ')?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, after) = rest.split_at(end);
    if digits.is_empty() || !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    digits.parse().ok()
}

/// The byte offset of the first line whose text, without its line ending, `matches`.
fn line_start(body: &str, matches: impl Fn(&str) -> bool) -> Option<usize> {
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if matches(line.trim_end_matches(['\n', '\r'])) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// The current time as the stored shape of an absolute UTC time,
/// `YYYY-MM-DDTHH:MM:SSZ` (decision 5).
pub fn utc_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    utc_from_unix(seconds)
}

/// `seconds` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`;
/// `chrono` is not a dependency, and the shape is the only thing arc needs).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_utc_time_has_the_stored_shape() {
        let now = utc_now();
        assert!(model::valid_utc(&now), "{now}");
        assert_eq!(utc_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day and the last second of a leap year.
        assert_eq!(utc_from_unix(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(utc_from_unix(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn a_change_record_goes_under_the_heading_and_keeps_the_request() {
        let mut body = model::plan_body("the request");
        append_change_record(&mut body, "2026-01-02T03:04:05Z", "a change\n");
        assert!(
            body.starts_with("the request\n\n## Change records\n"),
            "{body}"
        );
        assert!(
            body.contains("\n### 2026-01-02T03:04:05Z\n\na change\n"),
            "{body}"
        );
        append_change_record(&mut body, "2026-01-03T00:00:00Z", "another");
        assert_eq!(body.matches(CHANGE_RECORDS_HEADING).count(), 1, "{body}");
        assert_eq!(body.matches("### ").count(), 2, "{body}");
    }

    #[test]
    fn a_new_request_keeps_the_old_change_records() {
        let mut body = model::plan_body("the request");
        append_change_record(&mut body, "2026-01-02T03:04:05Z", "a change");
        set_request(&mut body, "a better request");
        assert!(
            body.starts_with("a better request\n\n## Change records\n"),
            "{body}"
        );
        assert!(body.contains("a change"), "{body}");
        assert!(!body.contains("the request"), "{body}");
    }

    /// The change records stay the body's last section: a rework appended after a first
    /// `update_task` goes above them, and the next record keeps the records last (ADR 0048
    /// decision 11).
    #[test]
    fn a_rework_goes_above_the_change_records() {
        let mut body = model::brief_body("the brief");
        append_task_change(&mut body, "2026-01-02T03:04:05Z", &["title"]);
        append_rework(&mut body, 1, "do it again", "");
        assert_eq!(
            body,
            "the brief\n\n## Rework 1\n\ndo it again\n\n## Changes\n\
             - 2026-01-02T03:04:05Z update_task: title\n"
        );
        append_task_change(&mut body, "2026-01-03T00:00:00Z", &["brief"]);
        assert_eq!(
            body,
            "the brief\n\n## Rework 1\n\ndo it again\n\n## Changes\n\
             - 2026-01-02T03:04:05Z update_task: title\n\
             - 2026-01-03T00:00:00Z update_task: brief\n"
        );
    }

    /// A task file whose records ended up above a later rework is repaired: the whole section
    /// moves to the end, in order, before the new line is added.
    #[test]
    fn an_update_moves_a_changes_section_a_rework_follows() {
        let mut body = "the brief\n\n## Changes\n- 2026-01-02T03:04:05Z update_task: title\n\n\
                        ## Rework 1\n\ndo it again\n"
            .to_owned();
        append_task_change(&mut body, "2026-01-03T00:00:00Z", &["after"]);
        assert_eq!(
            body,
            "the brief\n\n## Rework 1\n\ndo it again\n\n## Changes\n\
             - 2026-01-02T03:04:05Z update_task: title\n\
             - 2026-01-03T00:00:00Z update_task: after\n"
        );
    }

    /// Only a numbered heading, `## Rework <n>`, starts a rework section. An original brief
    /// that carries a heading such as `## Rework notes` is brief text, so a new brief replaces
    /// it (the tool contract's grammar).
    #[test]
    fn a_heading_that_only_starts_like_a_rework_is_part_of_the_brief() {
        let mut body = model::brief_body("the brief\n\n## Rework notes\n\nboth sides stay");
        set_brief(&mut body, "a better brief");
        assert_eq!(body, "a better brief\n");

        let mut bare = model::brief_body("the brief\n\n## Rework\n\nno number");
        set_brief(&mut bare, "a better brief");
        assert_eq!(bare, "a better brief\n");
    }

    /// The numbered heading, with or without the suffix arc writes after its number, is where
    /// the original brief ends.
    #[test]
    fn a_numbered_rework_heading_is_where_the_brief_ends() {
        let mut body = model::brief_body("the brief");
        append_rework(&mut body, 2, "do it again", REVIEW_REWORK_SUFFIX);
        set_brief(&mut body, "a better brief");
        assert_eq!(
            body,
            "a better brief\n\n## Rework 2 (review)\n\ndo it again\n"
        );
    }

    #[test]
    fn a_rework_section_is_numbered_and_keeps_the_brief() {
        let mut body = model::brief_body("the brief");
        append_rework(&mut body, 1, "do it again\n", "");
        assert_eq!(body, "the brief\n\n## Rework 1\n\ndo it again\n");
        append_rework(&mut body, 2, "and again", "");
        assert!(body.contains("## Rework 2\n\nand again\n"), "{body}");
        let mut empty = String::new();
        append_rework(&mut empty, 1, "only this", "");
        assert_eq!(empty, "## Rework 1\n\nonly this\n");
    }

    /// The conflict rework arc writes says so in its heading, so a reader can tell it apart
    /// from the lead's (ADR 0048 decision 9).
    #[test]
    fn a_conflict_rework_heading_says_arc_wrote_it() {
        let mut body = model::brief_body("the brief");
        append_rework(&mut body, 2, "the conflict", CONFLICT_REWORK_SUFFIX);
        assert!(
            body.contains("## Rework 2 (conflict with main)\n\nthe conflict\n"),
            "{body}"
        );
        append_rework(&mut body, 3, "the review", REVIEW_REWORK_SUFFIX);
        assert!(
            body.contains("## Rework 3 (review)\n\nthe review\n"),
            "{body}"
        );
    }
}

//! The plan changes the owner makes and the rework step the lead makes (ADR 0048
//! decisions 3, 6 and 12).
//!
//! Every function here is one of the store's transactions: it runs inside
//! [`Store::change_plan`] or [`Store::change_task`], so it takes `state.lock`, compares the
//! file with the ledger, refuses a file that changed outside arc, and writes the file, its
//! ledger entry and the `.last.md` copy in one step.
//!
//! - `confirm` moves a `draft` plan to `confirmed`. A plan with no tasks is not worth
//!   confirming, so it is refused.
//! - `auto` sets the plan's auto level. Only a `confirmed` plan may leave `off`.
//! - `pause` sets the level back to `off`. It does not look at the tasks: the scheduling
//!   loop that could be running one does not exist yet.
//! - `update_plan` replaces a draft plan's title or request text, and appends a change
//!   record to a confirmed one. A change record returns the plan to `draft` with `auto`
//!   `off`, and the tasks keep their state.
//! - `add_rework` appends a rework brief to a task in `review` and returns it to
//!   `todo.ready`.

use super::model::{self, TaskState};
use super::{Error, Store};

/// The heading change records are appended under.
pub const CHANGE_RECORDS_HEADING: &str = "## Change records";

/// The heading each rework brief is appended under, with its number.
pub const REWORK_HEADING: &str = "## Rework";

/// `radian arc confirm <plan>`: `draft` to `confirmed`.
///
/// Refused when the plan has no tasks (nothing to deliver) and when it is already
/// confirmed. A file that changed outside arc is refused by the transaction.
pub fn confirm(store: &Store, plan: &str) -> Result<(), Error> {
    store.change_plan(plan, |plan| {
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
        plan.status = "confirmed".to_owned();
        Ok(())
    })?;
    Ok(())
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
    store.change_plan(plan, |plan| {
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
    store.change_plan(plan, |plan| {
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

/// `add_rework`: the lead's rework brief for a task in `review` (decision 6).
///
/// The brief goes under `<REWORK_HEADING> <n>` with `n` one past the task's attempt, and the
/// task moves to `todo.ready`. New tasks are refused: a rework brief continues the task's
/// session, so it belongs to the task that session ran.
pub fn add_rework(store: &Store, task: &str, brief: &str) -> Result<(), Error> {
    if brief.trim().is_empty() {
        return Err(Error::Usage("the rework brief is empty".to_owned()));
    }
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
        append_rework(&mut task.body, number, brief);
        task.state = TaskState::Todo;
        task.phase = "ready".to_owned();
        Ok(())
    })?;
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

/// Appends one rework brief as its own section.
fn append_rework(body: &mut String, number: u32, brief: &str) {
    let head = body.trim_end().to_owned();
    body.clear();
    if !head.is_empty() {
        body.push_str(&head);
        body.push_str("\n\n");
    }
    body.push_str(&format!("{REWORK_HEADING} {number}\n\n"));
    body.push_str(brief.trim_end());
    body.push('\n');
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

    #[test]
    fn a_rework_section_is_numbered_and_keeps_the_brief() {
        let mut body = model::brief_body("the brief");
        append_rework(&mut body, 1, "do it again\n");
        assert_eq!(body, "the brief\n\n## Rework 1\n\ndo it again\n");
        append_rework(&mut body, 2, "and again");
        assert!(body.contains("## Rework 2\n\nand again\n"), "{body}");
        let mut empty = String::new();
        append_rework(&mut empty, 1, "only this");
        assert_eq!(empty, "## Rework 1\n\nonly this\n");
    }
}

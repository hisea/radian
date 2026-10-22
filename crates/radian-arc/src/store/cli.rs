//! `radian arc`'s subcommands: the argument handling, the output, and the exit code.
//!
//! The `radian` binary's parser turns its arguments into a [`Command`]; everything that
//! acts on the store and prints its result lives here, so `radian-tui`'s command file stays
//! a thin call and the same behavior is reachable from a test without a terminal.

use std::io::Read;

use serde_json::json;

use super::{Error, Exit, Item, Loaded, PlanView, Store};

/// The subcommands this build has, in the order `radian arc` lists them.
pub const SUBCOMMANDS: &[&str] = &["init", "add", "list", "show", "adopt"];

/// The subcommands ADR 0048 names that this build does not have. `work` is not here: it
/// exists as the hidden wrapper a scheduling loop runs, and a person does not run it.
pub const NOT_BUILT: &[&str] = &[
    "start",
    "merge",
    "reject",
    "auto",
    "confirm",
    "pause",
    "reset-budget",
    "cancel",
    "run",
    "mcp",
];

/// One `radian arc` invocation, without clap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `radian arc init [--track]`.
    Init { track: bool },
    /// `radian arc add plan --slug <s> --title <t>`; the request text is on stdin.
    AddPlan { slug: String, title: String },
    /// `radian arc add task --plan <id> --slug <s> --title <t> …`; the brief is on stdin.
    AddTask {
        plan: String,
        slug: String,
        title: String,
        after: Vec<String>,
        allow_noop: bool,
        stages: Vec<String>,
    },
    /// `radian arc list [--plan <id>] [--json]`.
    List { plan: Option<String>, json: bool },
    /// `radian arc show <id> [--json]`.
    Show { id: String, json: bool },
    /// `radian arc adopt <id>`.
    Adopt { id: String },
}

/// Runs one invocation and returns the process exit code. `None` is a bare `radian arc`:
/// the TUI does not exist yet, so it says so and lists the subcommands.
pub fn run(command: Option<&Command>) -> u8 {
    let result = match command {
        None => bare(),
        Some(Command::Init { track }) => init(*track).map(emit),
        Some(Command::AddPlan { slug, title }) => add_plan(slug, title).map(emit),
        Some(Command::AddTask {
            plan,
            slug,
            title,
            after,
            allow_noop,
            stages,
        }) => add_task(plan, slug, title, after, *allow_noop, stages).map(emit),
        Some(Command::List { plan, json }) => list(plan.as_deref(), *json).map(emit),
        Some(Command::Show { id, json }) => show(id, *json).map(emit),
        Some(Command::Adopt { id }) => adopt(id).map(emit),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("radian arc: {}", error.message());
            error.exit().code()
        }
    }
}

/// Prints `text` on stdout and maps the outcome to an exit code: a closed reader is a
/// normal stop (exit 0), as every other print-and-exit command of this binary treats it.
fn emit(text: String) -> u8 {
    match radian_host::write_stdout(&text) {
        Ok(()) | Err(radian_host::StdoutError::BrokenPipe) => Exit::Ok.code(),
        Err(error) => {
            eprintln!("radian arc: cannot write to stdout: {error}");
            Exit::Error.code()
        }
    }
}

fn cwd() -> Result<std::path::PathBuf, Error> {
    std::env::current_dir()
        .map_err(|error| Error::Failed(format!("cannot read the current directory: {error}")))
}

fn open() -> Result<Store, Error> {
    Store::open(&cwd()?)
}

/// A bare `radian arc`: without setup the refusal names `init`; with it, the TUI that does
/// not exist yet is a usage-class refusal that lists the subcommands.
fn bare() -> Result<u8, Error> {
    let store = open()?;
    let mut out = String::new();
    out.push_str("the arc TUI is not built yet.\n");
    out.push_str(&format!("subcommands: {}\n", SUBCOMMANDS.join(", ")));
    out.push_str(&format!(
        "in this build: `radian arc init`, `radian arc add plan|task`, `radian arc list`, \
         `radian arc show`, `radian arc adopt`\n{}",
        arc_dir_line(&store)
    ));
    let code = emit(out);
    Ok(if code == Exit::Ok.code() {
        Exit::Usage.code()
    } else {
        code
    })
}

fn arc_dir_line(store: &Store) -> String {
    format!("arc directory: {}\n", store.root().display())
}

fn stdin_text() -> Result<String, Error> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| Error::Failed(format!("cannot read stdin: {error}")))?;
    Ok(text)
}

fn init(track: bool) -> Result<String, Error> {
    let report = Store::init(&cwd()?, track)?;
    let mut out = String::from("arc init\n");
    out.push_str(&format!("  repository   {}\n", report.repository.display()));
    out.push_str(&format!(
        "  remote       {} {}\n",
        report.remote, report.remote_url
    ));
    out.push_str(&format!("  main branch  {}\n", report.main_branch));
    out.push_str(&format!(
        "  config       {} ({})\n",
        report.config_path.display(),
        if report.config_created {
            "created"
        } else {
            "kept"
        }
    ));
    out.push_str(&format!(
        "  exclude      {} ({})\n",
        report.exclude_path.display(),
        if report.exclude_added.is_empty() {
            "already current".to_owned()
        } else {
            format!("{} line(s) added", report.exclude_added.len())
        }
    ));
    out.push_str("checks\n");
    let mut warnings = Vec::new();
    for check in &report.checks {
        match &check.detail {
            Ok(detail) => out.push_str(&format!("  {:<20} ok: {detail}\n", check.name)),
            Err(detail) => {
                out.push_str(&format!("  {:<20} warning: {detail}\n", check.name));
                warnings.push(format!("{}: {detail}", check.name));
            }
        }
    }
    if !warnings.is_empty() {
        out.push_str("warnings\n");
        for warning in &warnings {
            out.push_str(&format!("  {warning}\n"));
        }
    }
    out.push_str("to register arc with a lead:\n  claude mcp add arc -- radian arc mcp\n");
    Ok(out)
}

fn add_plan(slug: &str, title: &str) -> Result<String, Error> {
    let store = open()?;
    let request = stdin_text()?;
    let id = store.add_plan(slug, title, &request)?;
    Ok(format!("{id}\n"))
}

fn add_task(
    plan: &str,
    slug: &str,
    title: &str,
    after: &[String],
    allow_noop: bool,
    stages: &[String],
) -> Result<String, Error> {
    let store = open()?;
    let brief = stdin_text()?;
    let id = store.add_task(
        plan,
        slug,
        title,
        after.to_vec(),
        allow_noop,
        stages.to_vec(),
        &brief,
    )?;
    Ok(format!("{id}\n"))
}

fn list(plan: Option<&str>, json: bool) -> Result<String, Error> {
    let store = open()?;
    let views = match plan {
        Some(id) => vec![store.plan_view(id)?],
        None => store.plans()?,
    };
    if json {
        let value = serde_json::Value::Array(
            views
                .iter()
                .map(|view| plan_json(view, false))
                .collect::<Vec<_>>(),
        );
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".to_owned())
        ));
    }
    let mut out = String::new();
    for view in &views {
        out.push_str(&format!(
            "{}  {}  auto: {}  {}{}\n",
            view.plan.item.id,
            view.plan.item.status,
            view.plan.item.auto,
            view.plan.item.title,
            changed_marker(view.plan.changed_outside_arc)
        ));
        for task in &view.tasks {
            out.push_str(&format!(
                "  {}  {}.{}  attempt {}  {}{}\n",
                task.item.id,
                task.item.state.as_str(),
                task.item.phase,
                task.item.attempt,
                task.item.title,
                changed_marker(task.changed_outside_arc)
            ));
        }
    }
    if out.is_empty() {
        out.push_str("no plans\n");
    }
    Ok(out)
}

fn changed_marker(changed: bool) -> &'static str {
    if changed {
        "  (changed outside arc)"
    } else {
        ""
    }
}

fn show(id: &str, json: bool) -> Result<String, Error> {
    let store = open()?;
    let item = store.show(id)?;
    if json {
        let value = match &item {
            Item::Plan(view) => plan_json(view, true),
            Item::Task(task) => task_json(task, true),
        };
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        ));
    }
    let mut out = String::new();
    match &item {
        Item::Plan(view) => {
            let plan = &view.plan;
            out.push_str(&format!("plan {}\n", plan.item.id));
            field(&mut out, "title", &plan.item.title);
            field(&mut out, "status", &plan.item.status);
            field(&mut out, "auto", &plan.item.auto);
            field(
                &mut out,
                "max_cost_usd",
                &optional_number(plan.item.max_cost_usd),
            );
            field(
                &mut out,
                "max_wall_time_secs",
                &optional_number(plan.item.max_wall_time_secs),
            );
            field(&mut out, "tasks", &joined(&plan.item.tasks));
            field(
                &mut out,
                "changed outside arc",
                if plan.changed_outside_arc {
                    "yes"
                } else {
                    "no"
                },
            );
            out.push('\n');
            out.push_str(&plan.item.body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        Item::Task(task) => {
            let item = &task.item;
            out.push_str(&format!("task {}\n", item.id));
            field(&mut out, "plan", &item.plan);
            field(&mut out, "title", &item.title);
            field(&mut out, "state", item.state.as_str());
            field(&mut out, "phase", &item.phase);
            field(&mut out, "attempt", &item.attempt.to_string());
            field(&mut out, "after", &joined(&item.after));
            field(
                &mut out,
                "supersedes",
                &item.supersedes.clone().unwrap_or_else(|| "-".to_owned()),
            );
            field(&mut out, "allow_noop", &item.allow_noop.to_string());
            field(&mut out, "extra_stages", &joined(&item.extra_stages));
            field(
                &mut out,
                "max_attempts",
                &optional_number(item.max_attempts),
            );
            field(
                &mut out,
                "max_cost_usd",
                &optional_number(item.max_cost_usd),
            );
            field(
                &mut out,
                "max_wall_time_secs",
                &optional_number(item.max_wall_time_secs),
            );
            field(
                &mut out,
                "stall_timeout_secs",
                &optional_number(item.stall_timeout_secs),
            );
            field(
                &mut out,
                "base_sha",
                &optional_text(item.base_sha.as_deref()),
            );
            field(
                &mut out,
                "head_sha",
                &optional_text(item.head_sha.as_deref()),
            );
            field(
                &mut out,
                "verified_sha",
                &optional_text(item.verified_sha.as_deref()),
            );
            field(
                &mut out,
                "retry_at",
                &optional_text(item.retry_at.as_deref()),
            );
            field(
                &mut out,
                "changed outside arc",
                if task.changed_outside_arc {
                    "yes"
                } else {
                    "no"
                },
            );
            out.push('\n');
            out.push_str(&item.body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Ok(out)
}

fn field(out: &mut String, name: &str, value: &str) {
    out.push_str(&format!("{name}: {value}\n"));
}

fn joined(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

fn optional_text(value: Option<&str>) -> String {
    value.unwrap_or("-").to_owned()
}

fn optional_number<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn task_json(task: &Loaded<super::model::TaskFile>, with_body: bool) -> serde_json::Value {
    let item = &task.item;
    let mut value = json!({
        "id": item.id,
        "plan": item.plan,
        "title": item.title,
        "state": item.state.as_str(),
        "phase": item.phase,
        "attempt": item.attempt,
        "after": item.after,
        "supersedes": item.supersedes,
        "allow_noop": item.allow_noop,
        "extra_stages": item.extra_stages,
        "max_attempts": item.max_attempts,
        "max_cost_usd": item.max_cost_usd,
        "max_wall_time_secs": item.max_wall_time_secs,
        "stall_timeout_secs": item.stall_timeout_secs,
        "base_sha": item.base_sha,
        "head_sha": item.head_sha,
        "verified_sha": item.verified_sha,
        "retry_at": item.retry_at,
        "changed_outside_arc": task.changed_outside_arc,
    });
    if with_body && let Some(object) = value.as_object_mut() {
        object.insert("body".to_owned(), json!(item.body));
    }
    value
}

fn plan_json(view: &PlanView, with_body: bool) -> serde_json::Value {
    let plan = &view.plan.item;
    let mut value = json!({
        "id": plan.id,
        "title": plan.title,
        "status": plan.status,
        "auto": plan.auto,
        "max_cost_usd": plan.max_cost_usd,
        "max_wall_time_secs": plan.max_wall_time_secs,
        "tasks": view
            .tasks
            .iter()
            // A task's own body is only in `show <task> --json`; the plan's JSON names the
            // tasks, it does not repeat every brief.
            .map(|task| task_json(task, false))
            .collect::<Vec<_>>(),
        "changed_outside_arc": view.plan.changed_outside_arc,
    });
    if with_body && let Some(object) = value.as_object_mut() {
        object.insert("body".to_owned(), json!(plan.body));
    }
    value
}

fn adopt(id: &str) -> Result<String, Error> {
    let store = open()?;
    let report = store.adopt(id)?;
    Ok(if report.changed {
        format!("adopted {} ({})\n", report.id, report.kind)
    } else {
        format!(
            "{} already matches arc's record ({})\n",
            report.id, report.kind
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{AUTO_LEVELS, PLAN_STATUSES};

    #[test]
    fn the_built_and_unbuilt_lists_do_not_share_a_name() {
        for name in SUBCOMMANDS {
            assert!(!NOT_BUILT.contains(name), "{name} is in both lists");
        }
        assert!(SUBCOMMANDS.contains(&"init"));
        assert!(NOT_BUILT.contains(&"mcp"));
    }

    #[test]
    fn every_plan_value_set_has_the_expected_members() {
        assert_eq!(PLAN_STATUSES, &["draft", "confirmed"]);
        assert_eq!(AUTO_LEVELS, &["off", "run", "merge"]);
    }
}

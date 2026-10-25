//! `radian arc`'s subcommands: the argument handling, the output, and the exit code.
//!
//! The `radian` binary's parser turns its arguments into a [`Command`]; everything that
//! acts on the store and prints its result lives here, so `radian-tui`'s command file stays
//! a thin call and the same behavior is reachable from a test without a terminal.

use std::io::Read;

use serde_json::json;

use super::{Error, Exit, Item, Loaded, PlanView, Store};

/// The subcommands this build has, in the order `radian arc` lists them.
pub const SUBCOMMANDS: &[&str] = &[
    "init",
    "add",
    "list",
    "show",
    "adopt",
    "confirm",
    "auto",
    "priority",
    "pause",
    "start",
    "reverify",
    "cancel",
    "reset-budget",
    "merge",
    "reject",
    "run",
    "serve",
    "mcp",
];

/// The subcommands ADR 0048 names that this build does not have. Every subcommand exists now;
/// a bare `radian arc` opens the TUI, which lives in `radian-tui` and calls this module's
/// subcommands. `work` is not here: it exists as the hidden wrapper a scheduling loop runs,
/// and a person does not run it.
pub const NOT_BUILT: &[&str] = &[];

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
    /// `radian arc list [--plan <id>] [--all] [--json]`. `--all` does not change the JSON.
    List {
        plan: Option<String>,
        all: bool,
        json: bool,
    },
    /// `radian arc show <id> [--json]`.
    Show { id: String, json: bool },
    /// `radian arc adopt <id>`.
    Adopt { id: String },
    /// `radian arc confirm <id>`: the owner confirms a draft plan, or a task arc or the
    /// lead proposed (ADR 0048 decisions 3, 6 and 11). `--dry-run` runs the same checks and
    /// prints what it would do without writing anything.
    Confirm { id: String, dry_run: bool },
    /// `radian arc auto <plan> <level>`.
    Auto { plan: String, level: String },
    /// `radian arc priority <plan> <priority>`: the plan's `P0`..`P5`.
    Priority { plan: String, priority: String },
    /// `radian arc pause <plan>`.
    Pause { plan: String },
    /// `radian arc start <task>`.
    Start { task: String },
    /// `radian arc reverify <task>`: re-run the gate for the same commit without a worker.
    Reverify { task: String },
    /// `radian arc cancel <id>`: a task, or every open task of a plan.
    Cancel { id: String },
    /// `radian arc reset-budget <id>`: a task or a plan.
    ResetBudget { id: String },
    /// `radian arc merge <task>`: the owner's approval and the merge into main.
    Merge { task: String },
    /// `radian arc reject <task> --note <text>`: send a task back to rework.
    Reject { task: String, note: String },
    /// `radian arc run`: one pass, then exit. A pass only schedules: a worker and a gate run
    /// outside it and a later pass collects them.
    Run,
    /// `radian arc serve [--interval <seconds>]`: the scheduling loop, a pass repeated until
    /// SIGINT or SIGTERM.
    Serve { interval: u64 },
    /// `radian arc mcp [--claude-experimental]`: the lead's MCP server on standard input and
    /// output. The flag pushes task transitions to the lead over Claude Code's channel
    /// extension (ADR 0048 decision 11).
    Mcp { claude_experimental: bool },
}

/// Runs one invocation and returns the process exit code.
pub fn run(command: &Command) -> u8 {
    let result = match command {
        Command::Init { track } => init(*track).map(emit),
        Command::AddPlan { slug, title } => add_plan(slug, title).map(emit),
        Command::AddTask {
            plan,
            slug,
            title,
            after,
            allow_noop,
            stages,
        } => add_task(plan, slug, title, after, *allow_noop, stages).map(emit),
        Command::List { plan, all, json } => list(plan.as_deref(), *all, *json).map(emit),
        Command::Show { id, json } => show(id, *json).map(emit),
        Command::Adopt { id } => adopt(id).map(emit),
        Command::Confirm { id, dry_run } => confirm(id, *dry_run),
        Command::Auto { plan, level } => set_auto(plan, level).map(emit),
        Command::Priority { plan, priority } => set_priority(plan, priority).map(emit),
        Command::Pause { plan } => pause(plan).map(emit),
        Command::Start { task } => start(task),
        Command::Reverify { task } => reverify(task),
        Command::Cancel { id } => cancel(id),
        Command::ResetBudget { id } => reset_budget(id),
        Command::Merge { task } => merge(task),
        Command::Reject { task, note } => reject(task, note),
        Command::Run => run_loop(),
        Command::Serve { interval } => serve_loop(*interval),
        Command::Mcp {
            claude_experimental,
        } => mcp(*claude_experimental),
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
        "  worker brief {} ({})\n",
        report.worker_brief_path.display(),
        if report.worker_brief_created {
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

fn list(plan: Option<&str>, all: bool, json: bool) -> Result<String, Error> {
    let store = open()?;
    if json {
        // `--json` follows the text filter for cancelled plans (by status or by their tasks):
        // `--all` adds them. A plan named with `--plan` is always shown. Scripts and the MCP
        // `list` tool read it.
        let value = list_json(&store, plan, all)?;
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".to_owned())
        ));
    }
    match plan {
        Some(id) => Ok(list_plan_text(&store.plan_view(id)?, all)),
        None => {
            // Claim order, the order `run` serves tasks in: the plan's priority, then its id.
            let mut views = store.plans()?;
            views.sort_by(|one, other| super::model::claim_order(&one.plan.item, &other.plan.item));
            Ok(list_text(&views, all))
        }
    }
}

/// The text `radian arc list` prints (ADR 0048 decision 1, `list`). Unless `all`, a task in
/// `done.merged`, a finished plan, and a cancelled plan are left out, and the last line counts
/// what was left out. A plan with no tasks is shown too. Every other state, `failed.*`
/// included, stays visible.
pub fn list_text(views: &[PlanView], all: bool) -> String {
    render(views, all, false)
}

/// The text `radian arc list --plan <id>` prints: the plan the user named is always shown,
/// with an empty task list when it is finished, so the user sees that it exists. Its merged
/// tasks follow the default filter.
pub fn list_plan_text(view: &PlanView, all: bool) -> String {
    render(std::slice::from_ref(view), all, true)
}

/// `named` keeps a finished or cancelled plan's header line: `--plan` asked for that plan by
/// name.
fn render(views: &[PlanView], all: bool, named: bool) -> String {
    let mut out = String::new();
    let mut hidden_plans = 0usize;
    let mut hidden_cancelled = 0usize;
    let mut hidden_tasks = 0usize;
    for view in views {
        let (merged, total) = view.progress();
        let finished = view.is_finished();
        if !all && !named && view.is_cancelled() {
            hidden_cancelled += 1;
            hidden_tasks += merged;
            continue;
        }
        if finished && !all && !named {
            hidden_plans += 1;
            hidden_tasks += merged;
            continue;
        }
        out.push_str(&format!(
            "{}  {}  auto: {}  priority: {}  {}/{} merged  total_cost_usd: {}  {}{}\n",
            view.plan.item.id,
            view.plan.item.status,
            view.plan.item.auto,
            view.plan.item.priority(),
            merged,
            total,
            cost_text(view.total_cost_usd()),
            view.plan.item.title,
            changed_marker(view.plan.changed_outside_arc)
        ));
        for task in &view.tasks {
            if is_merged(task) && !all {
                hidden_tasks += 1;
                continue;
            }
            let retry = task
                .item
                .retry_at
                .as_deref()
                .map(|at| format!("  retry at {at}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {}  {}.{}  attempt {}  {}{}{}\n",
                task.item.id,
                task.item.state.as_str(),
                task.item.phase,
                task.item.attempt,
                task.item.title,
                retry,
                changed_marker(task.changed_outside_arc)
            ));
        }
    }
    if hidden_plans > 0 || hidden_cancelled > 0 || hidden_tasks > 0 {
        out.push_str(&hidden_line(hidden_plans, hidden_cancelled, hidden_tasks));
    }
    if out.is_empty() {
        out.push_str("no plans\n");
    }
    out
}

/// Whether a task is `done.merged`: the one state the default `list` hides.
fn is_merged(task: &Loaded<crate::store::model::TaskFile>) -> bool {
    task.item.is_merged()
}

/// The line that ends a filtered `list`: how many plans and tasks were left out. A zero part
/// is omitted, and a count of one is singular.
fn hidden_line(plans: usize, cancelled: usize, tasks: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            one.to_owned()
        } else {
            many.to_owned()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if plans > 0 {
        parts.push(format!(
            "{plans} {}",
            plural(plans, "finished plan", "finished plans")
        ));
    }
    if cancelled > 0 {
        parts.push(format!(
            "{cancelled} {}",
            plural(cancelled, "cancelled plan", "cancelled plans")
        ));
    }
    if tasks > 0 {
        parts.push(format!(
            "{tasks} {}",
            plural(tasks, "merged task", "merged tasks")
        ));
    }
    let list = if parts.len() <= 1 {
        parts.join("")
    } else {
        let last = parts.pop().unwrap_or_default();
        format!("{} and {last}", parts.join(", "))
    };
    format!("{list} hidden (--all to show)\n")
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
    if json {
        let value = show_json(&store, id)?;
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        ));
    }
    show_text(&store, id)
}

/// `radian arc show <id>`'s text: every field, the `after` chain, and the body.
pub fn show_text(store: &Store, id: &str) -> Result<String, Error> {
    let item = store.show(id)?;
    let mut out = String::new();
    match &item {
        Item::Plan(view) => {
            let plan = &view.plan;
            out.push_str(&format!("plan {}\n", plan.item.id));
            field(&mut out, "title", &plan.item.title);
            field(&mut out, "status", &plan.item.status);
            field(&mut out, "auto", &plan.item.auto);
            field(&mut out, "priority", plan.item.priority());
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
            field(&mut out, "cost_usd", &cost_text(view.cost_usd()));
            field(
                &mut out,
                "total_cost_usd",
                &cost_text(view.total_cost_usd()),
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
            field(
                &mut out,
                "attempt_id",
                &optional_text(item.attempt_id.as_deref()),
            );
            field(
                &mut out,
                "session_id",
                &optional_text(
                    super::attempt_ops::attempt_session_id(store, &item.id, item.attempt)
                        .as_deref(),
                ),
            );
            for note in super::attempt_ops::attempt_notes(store, &item.id, item.attempt) {
                field(&mut out, "attempt_note", &note);
            }
            field(&mut out, "token", &optional_text(item.token.as_deref()));
            field(&mut out, "cost_usd", &optional_cost(item.cost_usd));
            field(
                &mut out,
                "total_cost_usd",
                &optional_cost(item.total_cost()),
            );
            field(&mut out, "auto_rework", &item.auto_rework.to_string());
            field(&mut out, "retry_count", &item.retry_count.to_string());
            field(&mut out, "after", &joined(&item.after));
            let chain = super::chain::chain(store, &item.id)?;
            field(&mut out, "waits_on", &chain_entries(&chain.waits_on));
            field(&mut out, "blocks", &chain_entries(&chain.blocks));
            if let Some(entry) = &chain.never_runs {
                field(&mut out, "never_runs", &entry.text());
            }
            if let Some(cycle) = &chain.cycle {
                field(&mut out, "cycle", &cycle.join(" -> "));
            }
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
                "verdict_set_digest",
                &optional_text(item.verdict_set_digest.as_deref()),
            );
            field(
                &mut out,
                "retry_at",
                &optional_text(item.retry_at.as_deref()),
            );
            if let Some(reason) = crate::review::rework_reason(store, item) {
                field(&mut out, "rework_reason", &reason);
            }
            if let Some((path, verdict)) = crate::review::latest_review_verdict(store, &item.id) {
                field(&mut out, "review_verdict", &path.display().to_string());
                if let Some(review) = &verdict.review {
                    field(&mut out, "review_summary", &review.summary);
                    field(&mut out, "review_blocking", &joined_items(&review.blocking));
                    field(
                        &mut out,
                        "review_suggestions",
                        &joined_items(&review.suggestions),
                    );
                    field(&mut out, "review_verified", &joined(&review.verified));
                }
                field(
                    &mut out,
                    "review_session_ids",
                    &joined(&verdict.review_session_ids),
                );
                field(
                    &mut out,
                    "review_cost_micros",
                    &optional_number(verdict.review_cost_micros),
                );
                if let Some(settings) = &verdict.review_settings {
                    field(&mut out, "review_round", &settings.round);
                    field(&mut out, "review_exec_args", &settings.exec_args);
                    field(
                        &mut out,
                        "review_timeout_secs",
                        &settings.timeout_secs.to_string(),
                    );
                    field(&mut out, "review_timeout", &settings.timeout);
                }
            }
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

/// A chain as `<id> <state>.<phase>` entries separated by `, `, or `-` when it is empty.
fn chain_entries(entries: &[super::chain::Entry]) -> String {
    if entries.is_empty() {
        "-".to_owned()
    } else {
        entries
            .iter()
            .map(super::chain::Entry::text)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The review items as `[level] path:line text`, separated by `; `.
fn joined_items(items: &[crate::gate::ReviewItem]) -> String {
    if items.is_empty() {
        "-".to_owned()
    } else {
        items
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
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

/// A task's cost field, printed the way a summed cost is printed: `-` when the file records
/// none, else [`cost_text`]'s rounding.
fn optional_cost(value: Option<f64>) -> String {
    value.map(cost_text).unwrap_or_else(|| "-".to_owned())
}

/// A summed cost as the text forms print it: up to six decimals, with the trailing zeros
/// dropped. Costs are kept in micros of a US dollar, so the rounding hides the float's own
/// noise and nothing a session reported.
fn cost_text(cost: f64) -> String {
    if cost == 0.0 {
        return "0".to_owned();
    }
    let mut text = format!("{cost:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
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
        "attempt_id": item.attempt_id,
        "token": item.token,
        "cost_usd": item.cost_usd,
        "total_cost_usd": item.total_cost(),
        "auto_rework": item.auto_rework,
        "retry_count": item.retry_count,
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
        "verdict_set_digest": item.verdict_set_digest,
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
        "priority": plan.priority(),
        "max_cost_usd": plan.max_cost_usd,
        "max_wall_time_secs": plan.max_wall_time_secs,
        "cost_usd": view.cost_usd(),
        "total_cost_usd": view.total_cost_usd(),
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

/// `radian arc confirm <id>`: the owner confirms a draft plan, or a task arc or the lead
/// proposed (ADR 0048 decisions 3, 6 and 11).
///
/// `--dry-run` runs every check the real command runs and prints what it would do, without
/// writing a file. The real command prints the `never runs:` and `cycle:` warnings on stderr
/// after its line; they warn, and the confirm still happens.
fn confirm(id: &str, dry_run: bool) -> Result<u8, Error> {
    let store = open()?;
    if dry_run {
        let report = super::plan_ops::confirm_dry_run(&store, id)?;
        return Ok(emit(dry_run_text(&store, &report)?));
    }
    let report = super::plan_ops::confirm_report(&store, id)?;
    let code = emit(format!("{}\n", report.confirmed.line()));
    for line in warning_lines(&store, &report.tasks)? {
        eprintln!("{line}");
    }
    Ok(code)
}

/// What `confirm --dry-run` prints: the line the real confirm would print, the chain of every
/// task it would confirm, the chains that cannot finish, and each task's place among the
/// `todo.ready` tasks at this moment.
fn dry_run_text(store: &Store, report: &super::plan_ops::Confirmation) -> Result<String, Error> {
    let mut out = format!("would: {}\n", report.confirmed.line());
    let mut never = Vec::new();
    let mut cycles = Vec::new();
    for id in &report.tasks {
        let chain = super::chain::chain(store, id)?;
        let waits = if chain.waits_on.is_empty() {
            "nothing".to_owned()
        } else {
            chain_entries(&chain.waits_on)
        };
        out.push_str(&format!("{id} waits on: {waits}\n"));
        if let Some(entry) = &chain.never_runs {
            never.push(format!("never runs: {}", entry.text()));
        }
        if let Some(cycle) = &chain.cycle {
            cycles.push(format!("cycle: {}", cycle.join(" -> ")));
        }
    }
    for line in never {
        out.push_str(&line);
        out.push('\n');
    }
    for line in cycles {
        out.push_str(&line);
        out.push('\n');
    }
    for id in &report.tasks {
        let (position, total) = super::chain::queue_position(store, id)?;
        out.push_str(&format!("{id} queue: {position} of {total} todo.ready"));
        if let Some(reason) = queue_reason(store, id)? {
            out.push_str(&format!(" ({reason})"));
        }
        out.push('\n');
    }
    Ok(out)
}

/// The `never runs:` and `cycle:` warnings of the tasks a confirm would make ready.
fn warning_lines(store: &Store, tasks: &[String]) -> Result<Vec<String>, Error> {
    let mut lines = Vec::new();
    for id in tasks {
        let chain = super::chain::chain(store, id)?;
        if let Some(entry) = &chain.never_runs {
            lines.push(format!("never runs: {}", entry.text()));
        }
        if let Some(cycle) = &chain.cycle {
            lines.push(format!("cycle: {}", cycle.join(" -> ")));
        }
    }
    Ok(lines)
}

/// Why the next scheduling pass would not claim a task that is `todo.ready` now, in the words
/// `radian arc run`'s summary uses: its plan's `auto` is `off`, an `after` task has not
/// merged, or every concurrency slot is taken. `None` when nothing stands in the way.
fn queue_reason(store: &Store, id: &str) -> Result<Option<String>, Error> {
    let task = store.load_task(id)?.item;
    let plan = store.load_plan(&task.plan)?.item;
    // The confirmation is what makes the task `todo.ready`, so the reason is the one a ready
    // task gets, not the one it has as `todo.proposed` now.
    let mut ready = task.clone();
    ready.phase = "ready".to_owned();
    let blocked = task.after.iter().any(|dependency| {
        !store
            .load_task(dependency)
            .is_ok_and(|loaded| loaded.item.is_merged())
    });
    if plan.auto == "off" || blocked || !crate::run::free_concurrency_slot(store)? {
        Ok(Some(crate::run::waiting_reason(store, &plan, &ready)?))
    } else {
        Ok(None)
    }
}

/// `radian arc auto <plan> <level>`: the owner sets the plan's auto level (decision 12).
fn set_auto(plan: &str, level: &str) -> Result<String, Error> {
    let store = open()?;
    super::plan_ops::set_auto(&store, plan, level)?;
    Ok(format!("{plan} auto {level}\n"))
}

/// `radian arc priority <plan> <P0..P5>`: the plan's priority (decision 3). A plan in any
/// state can be changed, and the next scheduling pass reads it.
fn set_priority(plan: &str, priority: &str) -> Result<String, Error> {
    let store = open()?;
    super::plan_ops::set_priority(&store, plan, priority)?;
    Ok(format!("{plan} priority {priority}\n"))
}

/// `radian arc pause <plan>`: the auto level back to `off`.
fn pause(plan: &str) -> Result<String, Error> {
    let store = open()?;
    super::plan_ops::pause(&store, plan)?;
    Ok(format!("{plan} auto off\n"))
}

/// `radian arc start <task>`: claim the task, run one attempt, apply the result, and
/// continue into the gate when the attempt was pushed.
fn start(task: &str) -> Result<u8, Error> {
    let store = open()?;
    let started = crate::attempt::start(&store, task)?;
    let text = format!("{}\n", crate::attempt::lines(&started).join("\n"));
    let code = emit(text);
    if code != Exit::Ok.code() {
        return Ok(code);
    }
    // The attempt's own line is printed even when verification refuses; the refusal follows
    // on stderr with its own exit code, and the task is left in `review.verifying`.
    if let Some(Err(error)) = &started.verification {
        eprintln!("radian arc: {}", error.message());
        return Ok(error.exit().code());
    }
    Ok(
        if started
            .applied
            .as_ref()
            .is_none_or(|applied| applied.left_doing())
        {
            Exit::Ok.code()
        } else {
            Exit::Error.code()
        },
    )
}

/// `radian arc reverify <task>`: claim a new attempt without a worker and re-run the gate.
/// The claim's line is printed even when verification refuses; the refusal follows on stderr
/// with its own exit code, and the task is left in `review.verifying`.
fn reverify(task: &str) -> Result<u8, Error> {
    let store = open()?;
    let reverified = crate::review::reverify(&store, task)?;
    let text = format!("{}\n", reverified.lines().join("\n"));
    let code = emit(text);
    if code != Exit::Ok.code() {
        return Ok(code);
    }
    if let Err(error) = &reverified.verification {
        eprintln!("radian arc: {}", error.message());
        return Ok(error.exit().code());
    }
    Ok(Exit::Ok.code())
}

/// `radian arc cancel <id>`: stop a task, or end every open task of a plan and the plan.
fn cancel(id: &str) -> Result<u8, Error> {
    let store = open()?;
    let canceled = crate::attempt::cancel_any(&store, id)?;
    Ok(emit(format!("{}\n", canceled.lines().join("\n"))))
}

/// `radian arc reset-budget <id>`: a paused task, or every paused task of a plan, back to
/// `todo.ready`, with the counted cost reset to zero.
fn reset_budget(id: &str) -> Result<u8, Error> {
    let store = open()?;
    let reset = super::attempt_ops::reset_budget(&store, id)?;
    Ok(emit(format!("{id}: {} task(s) ready\n", reset.tasks.len())))
}

/// `radian arc merge <task>`: the owner's approval, integration, and the push to main.
fn merge(task: &str) -> Result<u8, Error> {
    let store = open()?;
    let merged = crate::review::merge(&store, task)?;
    let mut text = format!("{}\n", merged.line());
    if let Some(warning) = &merged.warning {
        text.push_str(&format!("warning: {warning}\n"));
    }
    Ok(emit(text))
}

/// `radian arc reject <task> --note <text>`: the owner's note, and back to rework.
fn reject(task: &str, note: &str) -> Result<u8, Error> {
    let store = open()?;
    let rejected = crate::review::reject(&store, task, note)?;
    Ok(emit(format!("{}\n", rejected.line())))
}

/// `radian arc run`: one pass of the scheduling loop. A pass makes every transition it can
/// make now, prints the summary, and exits when nothing more can happen now.
///
/// A transition line is written and flushed when the loop makes it; the summary when the pass
/// ends. A closed stdout stops the printing and is a normal stop (exit `0`), as every other
/// print-and-exit command of this binary treats it.
fn run_loop() -> Result<u8, Error> {
    let store = open()?;
    let failed = std::cell::Cell::new(false);
    let mut emit = |text: &str| {
        if let Err(error) = radian_host::write_stdout(text)
            && !matches!(error, radian_host::StdoutError::BrokenPipe)
        {
            eprintln!("radian arc: cannot write to stdout: {error}");
            failed.set(true);
        }
    };
    crate::run::run(&store, &mut emit)?;
    Ok(if failed.get() {
        Exit::Error.code()
    } else {
        Exit::Ok.code()
    })
}

/// `radian arc serve [--interval <seconds>]`: the scheduling loop, until SIGINT or SIGTERM
/// (ADR 0048 §13). A transition line is written and flushed when the loop makes it; the
/// summary only when it changed. A closed stdout stops the printing and is a normal stop
/// (exit `0`), as every other print-and-exit command of this binary treats it.
fn serve_loop(interval: u64) -> Result<u8, Error> {
    let store = open()?;
    let failed = std::cell::Cell::new(false);
    let mut emit = |text: &str| match radian_host::write_stdout(text) {
        Ok(()) => {}
        // The reader of a foreground loop is gone (`... | head`): stop now, as a closed pipe
        // stops every other print-and-exit command with exit `0`.
        Err(radian_host::StdoutError::BrokenPipe) => std::process::exit(Exit::Ok.code().into()),
        Err(error) => {
            eprintln!("radian arc: cannot write to stdout: {error}");
            failed.set(true);
        }
    };
    crate::run::serve(&store, interval, &mut emit)?;
    Ok(if failed.get() {
        Exit::Error.code()
    } else {
        Exit::Ok.code()
    })
}

/// `radian arc mcp`: the lead's MCP server, until its client closes the connection. Without
/// setup `open` refuses with the same line and exit code as every other subcommand.
fn mcp(claude_experimental: bool) -> Result<u8, Error> {
    let store = open()?;
    crate::mcp::run(store, claude_experimental).map_err(Error::Failed)?;
    Ok(Exit::Ok.code())
}

/// `radian arc list --json`'s document, for the CLI and the MCP `list` tool. Unless `all`, a
/// cancelled plan is left out (by its `status` or by all its tasks being `failed.cancelled`);
/// a plan named with `plan` is always included.
pub fn list_json(store: &Store, plan: Option<&str>, all: bool) -> Result<serde_json::Value, Error> {
    let views = match plan {
        Some(id) => vec![store.plan_view(id)?],
        None => {
            // The same order and the same cancelled-plan filter the text form uses.
            let mut views = store.plans()?;
            if !all {
                views.retain(|view| !view.is_cancelled());
            }
            views.sort_by(|one, other| super::model::claim_order(&one.plan.item, &other.plan.item));
            views
        }
    };
    Ok(serde_json::Value::Array(
        views.iter().map(|view| plan_json(view, false)).collect(),
    ))
}

/// `radian arc show --json`'s document, for the CLI and the MCP `show` tool.
pub fn show_json(store: &Store, id: &str) -> Result<serde_json::Value, Error> {
    Ok(match store.show(id)? {
        Item::Plan(view) => plan_json(&view, true),
        Item::Task(task) => {
            let mut value = task_json(&task, true);
            let chain = super::chain::chain(store, &task.item.id)?;
            let review = crate::review::latest_review_verdict(store, &task.item.id);
            if let Some(object) = value.as_object_mut() {
                // The `after` chain, both ways (ADR 0048 decision 3): `show` and the MCP
                // `show` tool report the same walk.
                object.insert(
                    "waits_on".to_owned(),
                    json!(
                        chain
                            .waits_on
                            .iter()
                            .map(super::chain::Entry::json)
                            .collect::<Vec<_>>()
                    ),
                );
                object.insert(
                    "blocks".to_owned(),
                    json!(
                        chain
                            .blocks
                            .iter()
                            .map(super::chain::Entry::json)
                            .collect::<Vec<_>>()
                    ),
                );
                object.insert(
                    "never_runs".to_owned(),
                    json!(chain.never_runs.as_ref().map(super::chain::Entry::json)),
                );
                object.insert("cycle".to_owned(), json!(chain.cycle));
                object.insert(
                    "rework_reason".to_owned(),
                    json!(crate::review::rework_reason(store, &task.item)),
                );
                // The newest review verdict's findings (ADR 0048 decision 10): `review` is
                // `null` until a review ran, and stays `null` when its output was unusable.
                let (path, verdict) = match review {
                    Some((path, verdict)) => (Some(path), Some(verdict)),
                    None => (None, None),
                };
                object.insert(
                    "review_verdict".to_owned(),
                    json!(path.map(|path| path.display().to_string())),
                );
                object.insert(
                    "review".to_owned(),
                    json!(verdict.as_ref().and_then(|verdict| verdict.review.as_ref())),
                );
                object.insert(
                    "review_session_ids".to_owned(),
                    json!(
                        verdict
                            .as_ref()
                            .map(|verdict| verdict.review_session_ids.clone())
                            .unwrap_or_default()
                    ),
                );
                object.insert(
                    "review_cost_micros".to_owned(),
                    json!(
                        verdict
                            .as_ref()
                            .and_then(|verdict| verdict.review_cost_micros)
                    ),
                );
                object.insert(
                    "review_settings".to_owned(),
                    json!(
                        verdict
                            .as_ref()
                            .and_then(|verdict| verdict.review_settings.clone())
                    ),
                );
                // The current attempt's worker session (ADR 0048 decision 4): `attach` and the
                // arc TUI open the worker's journal by this id. `null` when the attempt's
                // wrapper wrote no record.
                object.insert(
                    "session_id".to_owned(),
                    json!(super::attempt_ops::attempt_session_id(
                        store,
                        &task.item.id,
                        task.item.attempt
                    )),
                );
                object.insert(
                    "attempt_notes".to_owned(),
                    json!(super::attempt_ops::attempt_notes(
                        store,
                        &task.item.id,
                        task.item.attempt
                    )),
                );
            }
            value
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{AUTO_LEVELS, PLAN_PRIORITIES, PLAN_STATUSES};

    #[test]
    fn the_built_and_unbuilt_lists_do_not_share_a_name() {
        for name in SUBCOMMANDS {
            assert!(!NOT_BUILT.contains(name), "{name} is in both lists");
        }
        assert!(SUBCOMMANDS.contains(&"init"));
        assert!(SUBCOMMANDS.contains(&"mcp"));
    }

    #[test]
    fn every_plan_value_set_has_the_expected_members() {
        assert_eq!(PLAN_STATUSES, &["draft", "confirmed", "cancelled"]);
        assert_eq!(AUTO_LEVELS, &["off", "run", "merge"]);
        assert_eq!(PLAN_PRIORITIES, &["P0", "P1", "P2", "P3", "P4", "P5"]);
    }
}

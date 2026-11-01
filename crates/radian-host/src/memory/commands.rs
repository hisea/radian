//! The `radian memory` / `/memory` command set, shared by the TUI and the headless command
//! (ADR 0021 D7).
//!
//! One clap grammar ([`MemoryCommand`]) and one executor ([`execute`]) so the CLI and the TUI
//! cannot drift in what a command means or what it prints. [`execute`] neither prints nor reads
//! stdin: it returns the text to show, an optional confirmation prompt, an optional diagnostics
//! note, and an optional path a host may open in the user's editor.

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::Parser;
use radian_memory::{
    Frontmatter, MemoryError, ProjectId, RECORD_TYPES, Record, validate_record_name,
};

use super::service::{
    MemoryService, MemoryServiceError, ProjectForWrite, ProjectState, ScopeSelection, ScopedEntry,
    WriteScope,
};

/// The exit code for a usage or refusal error, matching the CLI's `USAGE_ERROR` and clap.
pub const USAGE_ERROR: i32 = 2;

/// The current command names, in documentation order. The removed-command error and the TUI's
/// completion table both read this list.
pub const COMMAND_NAMES: &[&str] = &[
    "list", "show", "stale", "add", "edit", "forget", "open", "project", "relink", "split", "merge",
];

/// The `--type` values, in documentation order. The parser and the manual's drift test both
/// read this list.
pub const TYPE_NAMES: &[&str] = RECORD_TYPES;

/// The `--type` grammar (clap `value_parser`).
pub fn parse_type(raw: &str) -> Result<String, String> {
    if TYPE_NAMES.contains(&raw) {
        Ok(raw.to_owned())
    } else {
        Err(format!(
            "unknown type {raw:?}; expected one of {}",
            TYPE_NAMES.join(", ")
        ))
    }
}

/// `radian memory <command>` / `/memory <command>`: the user commands for durable memory.
/// `forget`, `split` and `merge` need confirmation.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum MemoryCommand {
    /// List one line per file. Project first, then Global.
    List {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one file's frontmatter and body as stored.
    Show {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        #[arg(long)]
        json: bool,
        name: String,
    },
    /// List memory files not read for a while: `last_used` older than --days, or never read
    /// and `created` older than --days. Reads the file store only.
    Stale {
        /// Days after which a file is stale. An integer of 1 or more; the default is 30.
        #[arg(long, default_value_t = DEFAULT_STALE_DAYS)]
        days: u64,
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a memory file and open it in the editor.
    Add {
        #[arg(long)]
        global: bool,
        #[arg(long = "type", value_parser = parse_type, default_value = "user")]
        record_type: String,
        name: String,
    },
    /// Open an existing memory file in the editor.
    Edit {
        #[arg(long)]
        global: bool,
        name: String,
    },
    /// Delete a memory file and its index line (needs confirmation).
    Forget {
        #[arg(long)]
        global: bool,
        #[arg(long)]
        yes: bool,
        name: String,
    },
    /// Print the scope directory, or open it in the editor.
    Open {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
    },
    /// Show this workspace's identity key and project state, or `project new` to register it.
    Project {
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<ProjectCommand>,
    },
    /// Attach this workspace's identity key to an existing project.
    Relink { project_id: String },
    /// Move files and their index lines between projects (needs confirmation).
    Split {
        #[arg(long)]
        yes: bool,
        from: String,
        to: String,
        #[arg(required = true, num_args = 1.., value_name = "NAME")]
        names: Vec<String>,
    },
    /// Merge one project into another, renaming a name present in both (needs confirmation).
    Merge {
        #[arg(long)]
        yes: bool,
        into: String,
        from: String,
    },
    // The removed commands parse only so their refusal can name the current set. They are
    // hidden from `--help` and from the TUI's completion table.
    #[command(hide = true)]
    Search {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Archive {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Restore {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Purge {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Confirm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Export {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Import {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ProjectCommand {
    /// Register a new project for this workspace's identity key.
    New,
}

impl MemoryCommand {
    /// The command's own `--json` flag (list/show/stale/project). `execute` takes `json`
    /// explicitly so a host can force text; both hosts pass this value.
    pub fn json(&self) -> bool {
        match self {
            MemoryCommand::List { json, .. }
            | MemoryCommand::Show { json, .. }
            | MemoryCommand::Stale { json, .. }
            | MemoryCommand::Project { json, .. } => *json,
            _ => false,
        }
    }

    /// The command's own `--yes` flag (forget/split/merge).
    pub fn yes(&self) -> bool {
        match self {
            MemoryCommand::Forget { yes, .. }
            | MemoryCommand::Split { yes, .. }
            | MemoryCommand::Merge { yes, .. } => *yes,
            _ => false,
        }
    }
}

/// A `memory` command failure: a message and the exit code the CLI uses (2 for a usage or
/// refusal error, 1 for a missing file or an I/O failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub code: i32,
    pub message: String,
}

impl CommandError {
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: USAGE_ERROR,
            message: message.into(),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<MemoryServiceError> for CommandError {
    fn from(error: MemoryServiceError) -> Self {
        CommandError::failure(error.to_string())
    }
}

/// What one command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// The text to show (already newline-terminated the way the CLI printed it).
    pub text: String,
    /// `Some(prompt)` when the command writes and `confirmed` was false: nothing was written.
    pub needs_confirmation: Option<String>,
    /// A diagnostics line for the host's stderr/notice channel (the unlinked-scope hint).
    pub note: Option<String>,
    /// A path the host may open: a file after `add`/`edit`, the scope directory after `open`.
    /// The CLI opens it only on a terminal; the TUI always opens it.
    pub open: Option<PathBuf>,
}

/// The `stale` command's default age, in days.
pub const DEFAULT_STALE_DAYS: u64 = 30;

/// The smallest accepted `--days`: zero would mean "everything", which is not a staleness
/// question and would list every file.
pub const MIN_STALE_DAYS: u64 = 1;

fn selection(global: bool, project: bool) -> ScopeSelection {
    if global {
        ScopeSelection::GlobalOnly
    } else if project {
        ScopeSelection::ProjectOnly
    } else {
        ScopeSelection::Default
    }
}

fn write_scope(global: bool) -> WriteScope {
    if global {
        WriteScope::Global
    } else {
        WriteScope::Project
    }
}

/// Runs one command against `service`. Never prints and never reads stdin.
pub fn execute(
    service: &MemoryService,
    command: MemoryCommand,
    confirmed: bool,
    json: bool,
) -> Result<CommandOutput, CommandError> {
    let mut output = CommandOutput::default();
    match command {
        MemoryCommand::List {
            global, project, ..
        } => {
            output.note = unlinked_note(service, global, project)?;
            let entries = service.list(selection(global, project))?;
            output.text = if json {
                json_line(&list_json(&entries))?
            } else {
                entries.iter().map(list_row).collect()
            };
        }
        MemoryCommand::Show {
            global,
            project,
            name,
            ..
        } => {
            let scope = locate(service, global, project, &name)?;
            let record = service.read(&scope, &name)?;
            output.text = if json {
                json_line(&show_json(&record))?
            } else {
                service.read_text(&scope, &name)?
            };
        }
        MemoryCommand::Stale {
            days,
            global,
            project,
            ..
        } => {
            output.note = unlinked_note(service, global, project)?;
            let entries = stale_entries(service, selection(global, project), days)?;
            output.text = if json {
                stale_json(&entries)?
            } else {
                entries.iter().map(stale_row).collect()
            };
        }
        MemoryCommand::Add {
            global,
            record_type,
            name,
        } => {
            // Validate before resolving the project: an unlinked git workspace would otherwise
            // register a project (and write projects.jsonl) for a name that is refused.
            if let Err(reason) = validate_record_name(&name) {
                return Err(add_error(MemoryServiceError::Storage(
                    MemoryError::InvalidName {
                        name: name.clone(),
                        reason,
                    },
                )));
            }
            let scope = write_scope(global);
            if scope == WriteScope::Project
                && let ProjectForWrite::Created(created) = service.project_for_write()?
            {
                output.note = Some(format!("[memory] {}", created.notice()));
            }
            let (scope, record) = service.add(scope, &name, &record_type).map_err(add_error)?;
            let path = service.record_path(&scope, &record.frontmatter.name);
            output.text = format!("added {name} ({})\n", path.display());
            output.open = Some(path);
        }
        MemoryCommand::Edit { global, name } => {
            let scope = locate(service, global, false, &name)?;
            let path = service.record_path(&scope, &name);
            output.text = format!("{}\n", path.display());
            output.open = Some(path);
        }
        MemoryCommand::Forget { global, yes, name } => {
            let prompt = format!("forget {name}");
            match confirm(confirmed || yes, prompt) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let scope = locate(service, global, false, &name)?;
                    service.remove(&scope, &name)?;
                    output.text = format!("forgot {name}\n");
                }
            }
        }
        MemoryCommand::Open { global, project } => {
            output.note = unlinked_note(service, global, project)?;
            let scopes = service.read_scopes(selection(global, project))?;
            let scope = scopes
                .first()
                .ok_or_else(|| CommandError::failure("memory has no selected scope"))?;
            let dir = service.scope_dir(scope);
            output.text = format!("{}\n", dir.display());
            output.open = Some(dir);
        }
        MemoryCommand::Project { command, .. } => match command {
            Some(ProjectCommand::New) => {
                let project = service.create_project()?;
                output.text = format!("project {project} now owns {}\n", service.identity_key());
            }
            None => output.text = project_text(service, json)?,
        },
        MemoryCommand::Relink { project_id } => {
            service.relink(&ProjectId(project_id.clone()))?;
            output.text = format!("relinked {} to {project_id}\n", service.identity_key());
        }
        MemoryCommand::Split {
            yes,
            from,
            to,
            names,
        } => {
            let prompt = format!(
                "move {} file(s) from {from} to {to}: {}",
                names.len(),
                names.join(", ")
            );
            match confirm(confirmed || yes, prompt) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let report = service
                        .split(&ProjectId(from), &ProjectId(to), &names)
                        .map_err(add_error)?;
                    output.text = move_report_text(&report);
                }
            }
        }
        MemoryCommand::Merge { yes, into, from } => {
            let prompt = format!("merge {from} into {into}");
            match confirm(confirmed || yes, prompt) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let report = service.merge(&ProjectId(into), &ProjectId(from))?;
                    output.text = move_report_text(&report);
                }
            }
        }
        // The old commands still parse (hidden) only so the refusal names the current set.
        MemoryCommand::Search { .. }
        | MemoryCommand::Archive { .. }
        | MemoryCommand::Restore { .. }
        | MemoryCommand::Purge { .. }
        | MemoryCommand::Confirm { .. }
        | MemoryCommand::Export { .. }
        | MemoryCommand::Import { .. } => {
            return Err(CommandError::usage(removed_message()));
        }
    }
    Ok(output)
}

/// The usage error a removed command gets, naming the current set.
pub fn removed_message() -> String {
    format!(
        "memory commands are: {}; archive, restore, confirm, purge, export, import and search \
         were removed",
        COMMAND_NAMES.join(", ")
    )
}

fn confirm(confirmed: bool, prompt: impl Into<String>) -> Option<String> {
    if confirmed { None } else { Some(prompt.into()) }
}

/// Maps the name errors `add` and `split` report to a usage error; anything else keeps its code.
fn add_error(error: MemoryServiceError) -> CommandError {
    match error {
        MemoryServiceError::AlreadyExists { .. }
        | MemoryServiceError::Storage(
            MemoryError::InvalidName { .. } | MemoryError::InvalidRecord { .. },
        ) => CommandError::usage(error.to_string()),
        other => other.into(),
    }
}

/// Finds the first selected scope holding `name`; a missing name is a runtime failure.
fn locate(
    service: &MemoryService,
    global: bool,
    project: bool,
    name: &str,
) -> Result<radian_memory::Scope, CommandError> {
    service
        .locate_name(selection(global, project), name)?
        .ok_or_else(|| CommandError::failure(format!("no memory file named {name:?}")))
}

fn unlinked_note(
    service: &MemoryService,
    global: bool,
    project: bool,
) -> Result<Option<String>, CommandError> {
    if global || project {
        return Ok(None);
    }
    if let ProjectState::Unlinked {
        identity_key,
        candidates,
    } = service.state()?
    {
        return Ok(Some(format!(
            "[memory] {}",
            MemoryServiceError::Unlinked {
                identity_key,
                candidates,
            }
        )));
    }
    Ok(None)
}

fn json_line<T: serde::Serialize>(value: &T) -> Result<String, CommandError> {
    let text = serde_json::to_string(value)
        .map_err(|e| CommandError::failure(format!("cannot render JSON: {e}")))?;
    Ok(format!("{text}\n"))
}

fn scope_label(scope: &radian_memory::Scope) -> &'static str {
    match scope {
        radian_memory::Scope::Global => "global",
        radian_memory::Scope::Project(_) => "project",
    }
}

/// The files `stale` lists: every record whose `last_used` (or, when it was never read,
/// `created`) is older than `days`, followed by every file that could not be parsed. A zero
/// age is a usage error naming the minimum.
fn stale_entries(
    service: &MemoryService,
    selection: ScopeSelection,
    days: u64,
) -> Result<Vec<ScopedEntry>, CommandError> {
    if days < MIN_STALE_DAYS {
        return Err(CommandError::usage(format!(
            "--days must be at least {MIN_STALE_DAYS}"
        )));
    }
    // An absurd age makes the cutoff earlier than any record; the arithmetic never panics.
    let days = i64::try_from(days).unwrap_or(i64::MAX);
    let cutoff = chrono::Duration::try_days(days)
        .and_then(|span| Utc::now().checked_sub_signed(span))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let entries = service.list(selection)?;
    Ok(entries
        .into_iter()
        .filter(|entry| match entry {
            ScopedEntry::Record { frontmatter, .. } => is_stale(frontmatter, cutoff),
            ScopedEntry::Unreadable { .. } => true,
        })
        .collect())
}

/// Older than the cutoff. A record never read is judged by `created`. The store has already
/// validated both timestamps; a value that somehow does not parse is listed rather than dropped.
fn is_stale(frontmatter: &Frontmatter, cutoff: DateTime<Utc>) -> bool {
    let timestamp = frontmatter
        .last_used
        .as_deref()
        .unwrap_or(&frontmatter.created);
    match DateTime::parse_from_rfc3339(timestamp) {
        Ok(time) => time.with_timezone(&Utc) < cutoff,
        Err(_) => true,
    }
}

/// `scope  name  last_used-or-never  description`.
fn stale_row(entry: &ScopedEntry) -> String {
    match entry {
        ScopedEntry::Record { scope, frontmatter } => format!(
            "{}  {}  {}  {}\n",
            scope_label(scope),
            frontmatter.name,
            frontmatter.last_used.as_deref().unwrap_or("never"),
            one_line(&frontmatter.description),
        ),
        ScopedEntry::Unreadable {
            scope,
            path,
            reason,
        } => format!(
            "{}  {}  unreadable  {}\n",
            scope_label(scope),
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            one_line(reason),
        ),
    }
}

/// The frontmatter fields of every listed file, with `last_used` null when never read. A file
/// that could not be read has no frontmatter: its object carries the path and the reason.
fn stale_json(entries: &[ScopedEntry]) -> Result<String, CommandError> {
    let values: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| match entry {
            ScopedEntry::Record { scope, frontmatter } => serde_json::json!({
                "scope": scope_label(scope),
                "name": frontmatter.name,
                "description": frontmatter.description,
                "type": frontmatter.record_type,
                "source": frontmatter.source,
                "created": frontmatter.created,
                "modified": frontmatter.modified,
                "last_used": frontmatter.last_used,
                "provenance": frontmatter.provenance.as_ref().map(|provenance| {
                    serde_json::json!({
                        "session_id": provenance.session_id,
                        "turn_id": provenance.turn_id,
                    })
                }),
            }),
            ScopedEntry::Unreadable {
                scope,
                path,
                reason,
            } => serde_json::json!({
                "scope": scope_label(scope),
                "path": path.to_string_lossy(),
                "unreadable": reason,
            }),
        })
        .collect();
    json_line(&values)
}

/// A field as one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn list_row(entry: &ScopedEntry) -> String {
    match entry {
        ScopedEntry::Record { scope, frontmatter } => format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            scope_label(scope),
            frontmatter.name,
            frontmatter.record_type,
            frontmatter.source,
            frontmatter.modified,
            one_line(&frontmatter.description),
        ),
        ScopedEntry::Unreadable {
            scope,
            path,
            reason,
        } => {
            let name = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{}\t{}\tunreadable\t{}\n", scope_label(scope), name, reason)
        }
    }
}

fn frontmatter_json(frontmatter: &Frontmatter) -> serde_json::Value {
    serde_json::json!({
        "name": frontmatter.name,
        "description": frontmatter.description,
        "type": frontmatter.record_type,
        "source": frontmatter.source,
        "created": frontmatter.created,
        "modified": frontmatter.modified,
        "last_used": frontmatter.last_used,
        "provenance": frontmatter.provenance.as_ref().map(|provenance| serde_json::json!({
            "session_id": provenance.session_id,
            "turn_id": provenance.turn_id,
        })),
    })
}

fn list_json(entries: &[ScopedEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|entry| match entry {
            ScopedEntry::Record { scope, frontmatter } => {
                let mut value = frontmatter_json(frontmatter);
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "scope".to_owned(),
                        serde_json::Value::String(scope_label(scope).to_owned()),
                    );
                }
                value
            }
            ScopedEntry::Unreadable {
                scope,
                path,
                reason,
            } => serde_json::json!({
                "scope": scope_label(scope),
                "unreadable": true,
                "path": path.to_string_lossy(),
                "reason": reason,
            }),
        })
        .collect()
}

fn show_json(record: &Record) -> serde_json::Value {
    serde_json::json!({
        "frontmatter": frontmatter_json(&record.frontmatter),
        "body": record.body,
    })
}

fn project_text(service: &MemoryService, json: bool) -> Result<String, CommandError> {
    let state = service.state()?;
    if json {
        let (label, project, candidates) = match &state {
            ProjectState::Known(project) => ("linked", Some(project.as_str()), Vec::new()),
            ProjectState::Unlinked { candidates, .. } => (
                "unlinked",
                None,
                candidates.iter().map(ProjectId::as_str).collect(),
            ),
        };
        return json_line(&serde_json::json!({
            "workspace": service.workspace().to_string_lossy(),
            "identity_key": service.identity_key(),
            "state": label,
            "project_id": project,
            "candidates": candidates,
        }));
    }
    let mut out = String::new();
    out.push_str(&format!("workspace: {}\n", service.workspace().display()));
    out.push_str(&format!("identity: {}\n", service.identity_key()));
    match &state {
        ProjectState::Known(project) => out.push_str(&format!("state: linked to {project}\n")),
        ProjectState::Unlinked { candidates, .. } => {
            out.push_str("state: not linked\n");
            if candidates.is_empty() {
                out.push_str("candidates: (none)\n");
            } else {
                let list: Vec<String> = candidates.iter().map(ProjectId::to_string).collect();
                out.push_str(&format!("candidates: {}\n", list.join(" ")));
            }
        }
    }
    Ok(out)
}

fn move_report_text(report: &super::service::MoveReport) -> String {
    let renamed_from: std::collections::BTreeSet<&str> = report
        .renamed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let mut out = String::new();
    for name in &report.moved {
        out.push_str(&format!("moved {name}\n"));
    }
    for (name, new_name) in &report.renamed {
        out.push_str(&format!(
            "conflict: {name} exists in the target; renamed it to {new_name}\n"
        ));
    }
    for name in &report.conflicts {
        if renamed_from.contains(name.as_str()) {
            continue;
        }
        out.push_str(&format!(
            "conflict: {name} exists in the target; it was not moved\n"
        ));
    }
    for name in &report.missing {
        out.push_str(&format!("missing {name}\n"));
    }
    if report.moved.is_empty()
        && report.renamed.is_empty()
        && report.conflicts.is_empty()
        && report.missing.is_empty()
    {
        out.push_str("nothing to do\n");
    }
    out
}

/// The clap parser for the TUI's `/memory <args>`.
#[derive(clap::Parser, Debug)]
#[command(name = "memory", no_binary_name = true)]
struct Invocation {
    #[command(subcommand)]
    command: MemoryCommand,
}

/// Parses `/memory` arguments (without the `/memory` token) into a command.
pub fn parse_args(args: &[String]) -> Result<MemoryCommand, clap::Error> {
    Invocation::try_parse_from(args).map(|invocation| invocation.command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_memory::Scope;

    fn scratch(label: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::with_label("radian-memory-cmd", label)
    }

    fn service(label: &str) -> (crate::test_support::TempDir, MemoryService) {
        let dir = scratch(label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let service = MemoryService::open(&ws, dir.join("memory"));
        (dir, service)
    }

    #[test]
    fn an_unconfirmed_forget_writes_nothing() {
        let (dir, service) = service("confirm");
        service.create_project().unwrap();
        service.add(WriteScope::Project, "note", "user").unwrap();

        let output = execute(
            &service,
            MemoryCommand::Forget {
                global: false,
                yes: false,
                name: "note".into(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.needs_confirmation.is_some(), "{output:?}");
        assert!(
            service
                .locate_name(ScopeSelection::Default, "note")
                .unwrap()
                .is_some()
        );

        let output = execute(
            &service,
            MemoryCommand::Forget {
                global: false,
                yes: false,
                name: "note".into(),
            },
            true,
            false,
        )
        .unwrap();
        assert!(output.text.contains("forgot"), "{output:?}");
        assert!(
            service
                .locate_name(ScopeSelection::Default, "note")
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_with_an_invalid_name_is_a_usage_error_and_writes_nothing() {
        let (dir, service) = service("bad-name");
        service.create_project().unwrap();
        let error = execute(
            &service,
            MemoryCommand::Add {
                global: false,
                record_type: "user".into(),
                name: "Bad Name".into(),
            },
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, USAGE_ERROR, "{error}");
        assert!(
            !service
                .files()
                .scope_dir(&radian_memory::Scope::Project(
                    service.project_id().unwrap()
                ))
                .exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_with_an_existing_name_is_a_usage_error_and_writes_nothing() {
        let (dir, service) = service("exists");
        service.create_project().unwrap();
        service.add(WriteScope::Project, "note", "user").unwrap();
        let error = execute(
            &service,
            MemoryCommand::Add {
                global: false,
                record_type: "user".into(),
                name: "note".into(),
            },
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, USAGE_ERROR, "{error}");
        assert_eq!(service.list(ScopeSelection::ProjectOnly).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `split` name outside the record rule is the same usage error `add` reports (exit 2),
    /// and a valid name earlier in the list does not move.
    #[test]
    fn split_with_an_invalid_name_is_a_usage_error_and_moves_nothing() {
        let (dir, service) = service("split-bad-name");
        let project = service.create_project().unwrap();
        service.add(WriteScope::Project, "note", "user").unwrap();

        let error = execute(
            &service,
            MemoryCommand::Split {
                yes: true,
                from: project.0.clone(),
                to: "target-project".into(),
                names: vec!["note".into(), "SESSIONS".into()],
            },
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, USAGE_ERROR, "{error}");
        assert!(
            error.message.contains("invalid memory record name"),
            "the same error add gives: {error}"
        );
        let source = Scope::Project(project);
        assert!(
            service.files().exists(&source, "note").unwrap(),
            "a refused split moves nothing"
        );
        assert!(
            std::fs::read_to_string(service.files().index_path(&source))
                .unwrap()
                .contains("note.md"),
            "the source index line stayed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_command_names_the_current_set_and_exits_two() {
        for command in [
            MemoryCommand::Purge { rest: Vec::new() },
            MemoryCommand::Export { rest: Vec::new() },
            MemoryCommand::Confirm { rest: Vec::new() },
        ] {
            let (dir, service) = service("removed");
            let error = execute(&service, command, true, false).unwrap_err();
            assert_eq!(error.code, USAGE_ERROR, "{error}");
            for name in COMMAND_NAMES {
                assert!(
                    error.message.contains(name),
                    "{} lacks {name}",
                    error.message
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn show_of_a_broken_file_names_the_line_and_list_still_lists_the_others() {
        let (dir, service) = service("broken");
        let project = service.create_project().unwrap();
        service.add(WriteScope::Project, "good", "user").unwrap();
        let scope = radian_memory::Scope::Project(project);
        let path = service.record_path(&scope, "bad");
        std::fs::write(
            &path,
            "---\nname: bad\ndescription: x\ntype: nope\nsource: user\ncreated: 2026-10-31T12:00:00.000000Z\nmodified: 2026-10-31T12:00:00.000000Z\n---\nbody\n",
        )
        .unwrap();
        let error = execute(
            &service,
            MemoryCommand::Show {
                global: false,
                project: true,
                json: false,
                name: "bad".into(),
            },
            false,
            false,
        )
        .unwrap_err();
        assert!(error.message.contains("line 4"), "{error}");
        let output = execute(
            &service,
            MemoryCommand::List {
                global: false,
                project: false,
                json: false,
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.text.contains("good"), "{}", output.text);
        assert!(output.text.contains("unreadable"), "{}", output.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `parse_args` accepts a quoted argument and rejects an unknown subcommand with clap's
    /// rendered error.
    #[test]
    fn parse_args_uses_the_shared_grammar() {
        let command = parse_args(&["add".into(), "hello world".into()]).unwrap();
        assert!(matches!(command, MemoryCommand::Add { name, .. } if name == "hello world"));
        let error = parse_args(&["nope".into()]).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }

    /// Writes a record file by hand, with the timestamps the test needs (the store's own
    /// writer always sets `created` and `modified` to now).
    fn write_record(
        service: &MemoryService,
        scope: &Scope,
        name: &str,
        last_used: Option<&str>,
        created: &str,
        description: &str,
    ) {
        let dir = service.files().scope_dir(scope);
        std::fs::create_dir_all(&dir).unwrap();
        let last = last_used
            .map(|value| format!("last_used: {value}\n"))
            .unwrap_or_default();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: {description}\ntype: project\nsource: model\n\
                 created: {created}\nmodified: {created}\n{last}---\nthe body\n"
            ),
        )
        .unwrap();
    }

    fn days_ago(days: i64) -> String {
        (Utc::now() - chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    fn stale(days: u64) -> MemoryCommand {
        MemoryCommand::Stale {
            days,
            global: true,
            project: false,
            json: false,
        }
    }

    /// Edge 5: `--days 7` lists a file read 8 days ago, not one read yesterday.
    #[test]
    fn stale_lists_a_file_read_eight_days_ago_and_not_one_read_yesterday() {
        let (dir, service) = service("stale-days");
        let old = days_ago(8);
        let recent = days_ago(1);
        write_record(
            &service,
            &Scope::Global,
            "old",
            Some(&old),
            &old,
            "the old note",
        );
        write_record(
            &service,
            &Scope::Global,
            "recent",
            Some(&recent),
            &recent,
            "the recent note",
        );

        let output = execute(&service, stale(7), false, false).unwrap();
        assert!(
            output.text.contains("old") && output.text.contains(&old),
            "{output:?}"
        );
        assert!(
            !output.text.contains("the recent note"),
            "a file read yesterday is not stale: {output:?}"
        );
        assert!(
            output.text.contains("global  old  "),
            "one line per file: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge 6: a file never read is judged by `created`; the default is 30 days.
    #[test]
    fn stale_lists_a_never_read_file_by_its_created_date() {
        let (dir, service) = service("stale-never");
        let old = days_ago(31);
        let recent = days_ago(1);
        write_record(
            &service,
            &Scope::Global,
            "unread-old",
            None,
            &old,
            "created long ago",
        );
        write_record(
            &service,
            &Scope::Global,
            "unread-new",
            None,
            &recent,
            "created yesterday",
        );

        let output = execute(&service, stale(DEFAULT_STALE_DAYS), false, false).unwrap();
        assert!(output.text.contains("unread-old"), "{output:?}");
        assert!(
            output.text.contains("never"),
            "a never-read file says so: {output:?}"
        );
        assert!(
            !output.text.contains("created yesterday"),
            "a file created yesterday is not stale: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge 7: `--days 0` is a usage error naming the minimum.
    #[test]
    fn stale_refuses_zero_days_as_a_usage_error() {
        let (dir, service) = service("stale-zero");
        let error = execute(&service, stale(0), false, false).unwrap_err();
        assert_eq!(error.code, 2, "{error:?}");
        assert_eq!(error.code, USAGE_ERROR, "{error:?}");
        assert!(
            error.message.contains("at least 1"),
            "the message names the minimum: {error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge 8: `--json` carries the frontmatter fields, `last_used` null when never read.
    #[test]
    fn stale_json_carries_the_frontmatter_fields() {
        let (dir, service) = service("stale-json");
        let created = days_ago(31);
        write_record(
            &service,
            &Scope::Global,
            "unread",
            None,
            &created,
            "never read",
        );

        let command = MemoryCommand::Stale {
            days: DEFAULT_STALE_DAYS,
            global: true,
            project: false,
            json: true,
        };
        let output = execute(&service, command, false, true).unwrap();
        let value: serde_json::Value = serde_json::from_str(output.text.trim()).unwrap();
        let record = &value.as_array().unwrap()[0];
        assert_eq!(record["name"], "unread");
        assert_eq!(record["scope"], "global");
        assert_eq!(record["last_used"], serde_json::Value::Null);
        assert_eq!(record["created"], created);
        assert_eq!(record["description"], "never read");
        assert_eq!(record["type"], "project");
        assert_eq!(record["source"], "model");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge 9: a malformed `last_used` is reported as unreadable, and the other files list.
    #[test]
    fn stale_reports_an_unreadable_file_and_lists_the_others() {
        let (dir, service) = service("stale-broken");
        let old = days_ago(31);
        write_record(
            &service,
            &Scope::Global,
            "good",
            None,
            &old,
            "the readable note",
        );
        write_record(
            &service,
            &Scope::Global,
            "broken",
            Some("not-a-timestamp"),
            &old,
            "the broken note",
        );

        let output = execute(&service, stale(DEFAULT_STALE_DAYS), false, false).unwrap();
        assert!(output.text.contains("the readable note"), "{output:?}");
        assert!(output.text.contains("broken.md"), "{output:?}");
        assert!(output.text.contains("unreadable"), "{output:?}");
        assert!(
            output.text.contains("last_used"),
            "the reason names the field: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

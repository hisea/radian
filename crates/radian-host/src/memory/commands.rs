//! The `radian memory` / `/memory` command set, shared by the TUI and the headless
//! command (ADR 0021 D7).
//!
//! One clap grammar ([`MemoryCommand`]) and one executor ([`execute`]) so the CLI and the TUI
//! cannot drift in what a command means or what it prints. [`execute`] neither prints nor
//! reads stdin: it returns the text to show, an optional confirmation prompt, and an optional
//! diagnostics note, and the host decides how to show them and how to confirm.
//!
//! **External input is resolved before `execute`.** At clap-parse time `add`/`edit`'s content
//! and `import`'s source are the argument as typed (a literal, `-`, or a path); the host reads
//! `-`/a file and passes the text on. `execute` then treats both as literal text.

use std::fmt;

use clap::Parser;

use radian_memory::{
    ImportReport, Kind, MemoryError, MemoryId, MemoryRecord, MoveReport, ProjectId, Scope,
    SearchQuery,
};

use super::service::{
    MemoryService, MemoryServiceError, ProjectForWrite, ProjectState, ScopeSelection, WriteScope,
};

/// The exit code for a usage or refusal error, matching the CLI's `USAGE_ERROR` and clap.
pub const USAGE_ERROR: i32 = 2;

/// `radian memory <command>` / `/memory <command>`: the user commands for
/// durable
/// memory. Every one is `Source::User`, and none of them needs a model, a key or a session
/// file. `forget`, `purge`, `split` and `merge` need confirmation.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum MemoryCommand {
    /// List records (Active only unless --all). Project first, then Global.
    List {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        /// Include archived records.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search records by keyword (every word must match).
    Search {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        #[arg(long)]
        json: bool,
        #[arg(required = true, num_args = 1.., value_name = "WORD")]
        words: Vec<String>,
    },
    /// Show one record by id.
    Show {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        #[arg(long)]
        json: bool,
        id: String,
    },
    /// Add a record (Project by default).
    Add {
        #[arg(long)]
        global: bool,
        #[arg(long, value_parser = parse_kind, default_value = "fact")]
        kind: Kind,
        #[arg(long)]
        key: Option<String>,
        /// The content, or `-` to read it from stdin.
        content: String,
    },
    /// Change a record's content (needs the current revision).
    Edit {
        #[arg(long)]
        global: bool,
        #[arg(long, required = true)]
        revision: u64,
        id: String,
        content: String,
    },
    /// Archive a record: still stored, no longer injected or searched by default.
    Archive {
        #[arg(long)]
        global: bool,
        #[arg(long, required = true)]
        revision: u64,
        id: String,
    },
    /// Restore an archived record.
    Restore {
        #[arg(long)]
        global: bool,
        #[arg(long, required = true)]
        revision: u64,
        id: String,
    },
    /// Forget a record: gone from every view and never resurrected by a rebuild (the bytes
    /// remain until a purge).
    Forget {
        #[arg(long)]
        global: bool,
        #[arg(long)]
        yes: bool,
        id: String,
    },
    /// Physically rewrite the log, removing forgotten content (needs confirmation).
    Purge {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Confirm a record (keeps its original source).
    Confirm {
        #[arg(long)]
        global: bool,
        #[arg(long, required = true)]
        revision: u64,
        id: String,
    },
    /// Print one scope as editable Markdown.
    Export {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
    },
    /// Import an edited Markdown export. `-` reads it from stdin.
    Import {
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long, conflicts_with = "global")]
        project: bool,
        source: String,
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
    /// Move records between projects (needs confirmation).
    Split {
        #[arg(long)]
        yes: bool,
        from: String,
        to: String,
        #[arg(required = true, num_args = 1.., value_name = "ID")]
        ids: Vec<String>,
    },
    /// Merge one project into another, reporting key conflicts (needs confirmation).
    Merge {
        #[arg(long)]
        yes: bool,
        into: String,
        from: String,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ProjectCommand {
    /// Register a new project for this workspace's identity key.
    New,
}

impl MemoryCommand {
    /// The command's own `--json` flag (list/search/show/project). `execute` takes `json`
    /// explicitly so a host can force text; both hosts pass this value.
    pub fn json(&self) -> bool {
        match self {
            MemoryCommand::List { json, .. }
            | MemoryCommand::Search { json, .. }
            | MemoryCommand::Show { json, .. }
            | MemoryCommand::Project { json, .. } => *json,
            _ => false,
        }
    }

    /// The command's own `--yes` flag (forget/purge/split/merge). `execute` takes `confirmed`
    /// explicitly so a host can ask first; the CLI passes this value.
    pub fn yes(&self) -> bool {
        match self {
            MemoryCommand::Forget { yes, .. }
            | MemoryCommand::Purge { yes, .. }
            | MemoryCommand::Split { yes, .. }
            | MemoryCommand::Merge { yes, .. } => *yes,
            _ => false,
        }
    }
}

/// A `memory` command failure: a message and the exit code the CLI uses (2 for a usage or
/// refusal error, 1 for a conflict or a missing record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub code: i32,
    pub message: String,
}

impl CommandError {
    /// A runtime failure (conflict, missing record, unlinked workspace, I/O): exit 1.
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    /// A usage or refusal error: exit 2.
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
    /// `Some(prompt)` when the command writes and `confirmed` was false: nothing was written,
    /// and the host must confirm with this prompt and call `execute` again with `confirmed`.
    pub needs_confirmation: Option<String>,
    /// A diagnostics line for the host's stderr/notice channel (the unlinked-scope hint).
    pub note: Option<String>,
}

/// The `--kind` grammar (clap `value_parser`). A value this version does not know is a usage
/// error, not a silently stored `Unknown` (ADR 0021 D4).
pub fn parse_kind(raw: &str) -> Result<Kind, String> {
    match raw {
        "preference" => Ok(Kind::Preference),
        "decision" => Ok(Kind::Decision),
        "fact" => Ok(Kind::Fact),
        "lesson" => Ok(Kind::Lesson),
        other => Err(format!(
            "unknown kind {other:?}; expected preference|decision|fact|lesson"
        )),
    }
}

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

/// The not-found error for an id command whose auto-detected scope is asked for the record:
/// ids are unique across scopes, so "not found" means no scope this workspace can see holds
/// it (ADR 0021 D1). The wording is the store's own (`MemoryError::NotFound`), so one message
/// covers every place an id is unknown.
fn unknown_id(id: &str) -> CommandError {
    MemoryServiceError::Storage(MemoryError::NotFound {
        id: MemoryId(id.to_owned()),
    })
    .into()
}

/// The read scope for a single-record read (`show`). An explicit `--global`/`--project`
/// keeps today's meaning (that scope only); otherwise the record's own scope is found, so an
/// unlinked workspace still reads a Global record by id (ADR 0021 D1).
fn resolve_read_scope(
    service: &MemoryService,
    global: bool,
    project: bool,
    id: &str,
) -> Result<ScopeSelection, CommandError> {
    if global {
        return Ok(ScopeSelection::GlobalOnly);
    }
    if project {
        return Ok(ScopeSelection::ProjectOnly);
    }
    match service.locate(&MemoryId(id.to_owned()))? {
        Some(Scope::Global) => Ok(ScopeSelection::GlobalOnly),
        Some(Scope::Project(_)) => Ok(ScopeSelection::ProjectOnly),
        None => Err(unknown_id(id)),
    }
}

/// The write scope for a single-record write (`edit`/`archive`/`restore`/`confirm`/`forget`).
/// An explicit `--global` keeps today's meaning (Global only); otherwise the record's own
/// scope is found, so an unlinked workspace still edits, archives, restores, confirms or
/// forgets a Global record by id (ADR 0021 D1).
fn resolve_write_scope(
    service: &MemoryService,
    global: bool,
    id: &str,
) -> Result<WriteScope, CommandError> {
    if global {
        return Ok(WriteScope::Global);
    }
    match service.locate(&MemoryId(id.to_owned()))? {
        Some(Scope::Global) => Ok(WriteScope::Global),
        Some(Scope::Project(_)) => Ok(WriteScope::Project),
        None => Err(unknown_id(id)),
    }
}

/// Runs one command against `service`. Never prints and never reads stdin. `confirmed` is the
/// user's consent for a write command; `json` selects machine-readable output where offered.
pub fn execute(
    service: &MemoryService,
    command: MemoryCommand,
    confirmed: bool,
    json: bool,
) -> Result<CommandOutput, CommandError> {
    let mut output = CommandOutput::default();
    match command {
        MemoryCommand::List {
            global,
            project,
            all,
            ..
        } => {
            output.note = unlinked_note(service, global, project)?;
            let records = service.list(selection(global, project), all)?;
            output.text = if json {
                json_line(&records)?
            } else {
                records.iter().map(list_row).collect()
            };
        }
        MemoryCommand::Search {
            global,
            project,
            words,
            ..
        } => {
            output.note = unlinked_note(service, global, project)?;
            let query = SearchQuery {
                text: Some(words.join(" ")),
                key: None,
                include_archived: false,
            };
            let records = service.search(selection(global, project), &query)?;
            output.text = if json {
                json_line(&records)?
            } else {
                records.iter().map(list_row).collect()
            };
        }
        MemoryCommand::Show {
            global,
            project,
            id,
            ..
        } => {
            let selection = resolve_read_scope(service, global, project, &id)?;
            let record = service.get(selection, &MemoryId(id))?;
            output.text = if json {
                json_line(&record)?
            } else {
                record_text(&record)
            };
        }
        MemoryCommand::Add {
            global,
            kind,
            key,
            content,
        } => {
            let scope = write_scope(global);
            // ADR 0021: a user's default-scope `add` registers a git workspace and
            // shows the notice through the host's note channel. Reads (`list`/`search`/`show`)
            // never create anything.
            if scope == WriteScope::Project
                && let ProjectForWrite::Created(created) = service.project_for_write()?
            {
                // The `[memory]` prefix matches `unlinked_note`: a host prints a note as it is.
                output.note = Some(format!("[memory] {}", created.notice()));
            }
            let record = service.add(scope, kind, key, trim_trailing_newlines(content))?;
            output.text = format!("added {} (revision {})\n", record.id, record.revision);
        }
        MemoryCommand::Edit {
            global,
            revision,
            id,
            content,
        } => {
            let scope = resolve_write_scope(service, global, &id)?;
            let record = service.edit(
                scope,
                &MemoryId(id),
                revision,
                trim_trailing_newlines(content),
            )?;
            output.text = format!("revised {} (revision {})\n", record.id, record.revision);
        }
        MemoryCommand::Archive {
            global,
            revision,
            id,
        } => {
            let scope = resolve_write_scope(service, global, &id)?;
            let record = service.archive(scope, &MemoryId(id), revision)?;
            output.text = format!("archived {} (revision {})\n", record.id, record.revision);
        }
        MemoryCommand::Restore {
            global,
            revision,
            id,
        } => {
            let scope = resolve_write_scope(service, global, &id)?;
            let record = service.restore(scope, &MemoryId(id), revision)?;
            output.text = format!("restored {} (revision {})\n", record.id, record.revision);
        }
        MemoryCommand::Forget { global, id, .. } => {
            match confirm(confirmed, format!("forget {id}")) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let scope = resolve_write_scope(service, global, &id)?;
                    service.forget(scope, &MemoryId(id.clone()))?;
                    output.text = format!("forgot {id}\n");
                }
            }
        }
        MemoryCommand::Purge { global, .. } => {
            let prompt = "purge this scope: rewrite the log and remove forgotten content";
            match confirm(confirmed, prompt) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let removed = service.purge(write_scope(global))?;
                    output.text = format!("purged {removed} record(s)\n");
                }
            }
        }
        MemoryCommand::Confirm {
            global,
            revision,
            id,
        } => {
            let scope = resolve_write_scope(service, global, &id)?;
            let record = service.confirm(scope, &MemoryId(id), revision)?;
            output.text = format!("confirmed {} (revision {})\n", record.id, record.revision);
        }
        MemoryCommand::Export { global, .. } => {
            output.text = service.export(write_scope(global))?;
        }
        MemoryCommand::Import { global, source, .. } => {
            let report = service.import(write_scope(global), &source)?;
            output.text = import_report_text(&report);
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
        MemoryCommand::Split { from, to, ids, .. } => {
            // The prompt must name the records that actually exist: `ids` is what the user
            // typed, and `split` would silently drop the ones it does not find. Read the
            // `from` project first (no write) so the confirmation is a promise about what
            // will move (ADR 0021 D7).
            let present: Vec<String> = service
                .store()
                .records(&Scope::Project(ProjectId(from.clone())))
                .map_err(|error| CommandError::failure(error.to_string()))?
                .into_iter()
                .map(|record| record.id.0)
                .collect();
            let found: Vec<&String> = ids.iter().filter(|id| present.contains(id)).collect();
            let missing: Vec<&String> = ids.iter().filter(|id| !present.contains(id)).collect();
            if found.is_empty() {
                return Err(CommandError::failure(format!(
                    "none of the {} record(s) exist in {from}: {}",
                    ids.len(),
                    missing
                        .iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let mut prompt = format!(
                "move {} of {} record(s) from {from} to {to}",
                found.len(),
                ids.len()
            );
            if !missing.is_empty() {
                prompt.push_str(&format!(
                    " (not found: {})",
                    missing
                        .iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            match confirm(confirmed, prompt) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let ids: Vec<MemoryId> = ids.into_iter().map(MemoryId).collect();
                    let report = service.split(&ProjectId(from), &ids, &ProjectId(to))?;
                    output.text = move_report_text(&report);
                }
            }
        }
        MemoryCommand::Merge { into, from, .. } => {
            let prompt = format!("merge {from} into {into}");
            match confirm(confirmed, prompt) {
                Some(prompt) => output.needs_confirmation = Some(prompt),
                None => {
                    let report = service.merge(&ProjectId(into), &ProjectId(from))?;
                    output.text = move_report_text(&report);
                }
            }
        }
    }
    Ok(output)
}

/// `Some(prompt)` when the write must be confirmed first; `None` when it may run.
fn confirm(confirmed: bool, prompt: impl Into<String>) -> Option<String> {
    if confirmed { None } else { Some(prompt.into()) }
}

/// A default-scope `list`/`search` on an unlinked workspace reads only Global; the host shows
/// this line so the dropped Project scope is visible. An explicit `--global`/`--project` does
/// not produce it.
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

fn trim_trailing_newlines(mut text: String) -> String {
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    text
}

fn json_line<T: serde::Serialize>(value: &T) -> Result<String, CommandError> {
    let text = serde_json::to_string(value)
        .map_err(|e| CommandError::failure(format!("cannot render JSON: {e}")))?;
    Ok(format!("{text}\n"))
}

fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Global => "global".to_owned(),
        Scope::Project(project) => format!("project:{project}"),
    }
}

/// A field as one line (the session `--search` snippet's rule).
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One record as a tab-separated row: id, revision, scope, kind, key, status, verification,
/// one-line content. No header, like the session `--list`.
fn list_row(record: &MemoryRecord) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        record.id,
        record.revision,
        scope_label(&record.scope),
        record.kind.as_str(),
        record.key.as_deref().unwrap_or("-"),
        record.status.as_str(),
        if record.verification.is_confirmed() {
            "confirmed"
        } else {
            "unverified"
        },
        one_line(&record.content),
    )
}

fn record_text(record: &MemoryRecord) -> String {
    let mut out = String::new();
    out.push_str(&format!("id: {}\n", record.id));
    out.push_str(&format!("scope: {}\n", scope_label(&record.scope)));
    out.push_str(&format!("kind: {}\n", record.kind.as_str()));
    out.push_str(&format!("key: {}\n", record.key.as_deref().unwrap_or("-")));
    out.push_str(&format!("source: {}\n", record.source.as_str()));
    out.push_str(&format!(
        "verification: {}\n",
        if record.verification.is_confirmed() {
            "confirmed"
        } else {
            "unverified"
        }
    ));
    out.push_str(&format!("status: {}\n", record.status.as_str()));
    out.push_str(&format!("created: {}\n", record.created_at));
    out.push_str(&format!("updated: {}\n", record.updated_at));
    out.push_str(&format!("revision: {}\n", record.revision));
    out.push_str("---\n");
    out.push_str(&format!("{}\n", record.content));
    out
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

fn move_report_text(report: &MoveReport) -> String {
    let mut out = String::new();
    for id in &report.moved {
        out.push_str(&format!("moved {id}\n"));
    }
    for conflict in &report.conflicts {
        out.push_str(&format!(
            "conflict: {} (key {:?}) is held in the target by {} at revision {}\n",
            conflict.id, conflict.key, conflict.held_by, conflict.held_revision
        ));
    }
    for id in &report.missing {
        out.push_str(&format!("missing {id}\n"));
    }
    if report.moved.is_empty() && report.conflicts.is_empty() && report.missing.is_empty() {
        out.push_str("nothing to do\n");
    }
    out
}

fn import_report_text(report: &ImportReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "created {} revised {} conflicts {} rejections {}\n",
        report.created.len(),
        report.revised.len(),
        report.conflicts.len(),
        report.rejections.len(),
    ));
    for conflict in &report.conflicts {
        out.push_str(&format!(
            "conflict {}: expected revision {} but found {}\n",
            conflict.id, conflict.expected, conflict.found
        ));
    }
    for rejection in &report.rejections {
        out.push_str(&format!(
            "rejected {}: {} changed from {:?} to {:?}\n",
            rejection.id, rejection.field, rejection.stored, rejection.imported
        ));
    }
    out
}

/// The clap parser for the TUI's `/memory <args>`: `no_binary_name` so the first token is the
/// subcommand, and `try_parse_from` returns a clap error whose rendered text is shown as-is.
#[derive(clap::Parser, Debug)]
#[command(name = "memory", no_binary_name = true)]
struct Invocation {
    #[command(subcommand)]
    command: MemoryCommand,
}

/// Parses `/memory` arguments (without the `/memory` token) into a command. The error is the
/// clap error (a usage error or `--help`), its `to_string()` ready to show.
pub fn parse_args(args: &[String]) -> Result<MemoryCommand, clap::Error> {
    Invocation::try_parse_from(args).map(|invocation| invocation.command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_memory::Source;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-memory-cmd-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(label: &str) -> (PathBuf, MemoryService) {
        let dir = scratch(label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let service = MemoryService::open(&ws, dir.join("memory"));
        (dir, service)
    }

    /// `forget` with `confirmed = false` returns the prompt and writes nothing.
    #[test]
    fn an_unconfirmed_forget_writes_nothing() {
        let (dir, service) = service("confirm");
        service.create_project().unwrap();
        let record = service
            .add(WriteScope::Project, Kind::Fact, None, "keep".into())
            .unwrap();
        let log = service.store().log_path(&record.scope);
        let before = std::fs::read_to_string(&log).unwrap();

        let output = execute(
            &service,
            MemoryCommand::Forget {
                global: false,
                yes: false,
                id: record.id.to_string(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.needs_confirmation.is_some(), "{output:?}");
        assert_eq!(output.text, "", "no success text before confirmation");
        assert_eq!(before, std::fs::read_to_string(&log).unwrap());

        // Confirmed: it runs.
        let output = execute(
            &service,
            MemoryCommand::Forget {
                global: false,
                yes: false,
                id: record.id.to_string(),
            },
            true,
            false,
        )
        .unwrap();
        assert!(output.text.contains("forgot"), "{output:?}");
        assert!(
            service
                .list(ScopeSelection::ProjectOnly, false)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unlinked workspace still finds a Global record by id: `show` needs no `--global`
    /// (ADR 0021 D1; record ids are unique across scopes).
    #[test]
    fn show_finds_a_global_record_in_an_unlinked_workspace() {
        let (dir, service) = service("show-global");
        let record = service
            .add(WriteScope::Global, Kind::Fact, None, "global body".into())
            .unwrap();
        let output = execute(
            &service,
            MemoryCommand::Show {
                global: false,
                project: false,
                json: false,
                id: record.id.to_string(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.text.contains("global body"), "{output:?}");
        assert!(output.text.contains("scope: global"), "{output:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unlinked workspace still revises a Global record by id: `edit` needs no `--global`.
    #[test]
    fn edit_finds_a_global_record_in_an_unlinked_workspace() {
        let (dir, service) = service("edit-global");
        let record = service
            .add(WriteScope::Global, Kind::Fact, None, "v1".into())
            .unwrap();
        let output = execute(
            &service,
            MemoryCommand::Edit {
                global: false,
                revision: record.revision,
                id: record.id.to_string(),
                content: "v2".into(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.text.contains("revised"), "{output:?}");
        let found = service.get(ScopeSelection::GlobalOnly, &record.id).unwrap();
        assert_eq!(found.content, "v2");
        assert!(found.scope.is_global());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unlinked workspace still archives and restores a Global record by id.
    #[test]
    fn archive_and_restore_find_a_global_record_in_an_unlinked_workspace() {
        let (dir, service) = service("archive-global");
        let record = service
            .add(WriteScope::Global, Kind::Fact, None, "body".into())
            .unwrap();
        let archived = execute(
            &service,
            MemoryCommand::Archive {
                global: false,
                revision: record.revision,
                id: record.id.to_string(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(archived.text.contains("archived"), "{archived:?}");
        assert!(
            service
                .list(ScopeSelection::GlobalOnly, false)
                .unwrap()
                .is_empty()
        );

        let restored = execute(
            &service,
            MemoryCommand::Restore {
                global: false,
                revision: record.revision + 1,
                id: record.id.to_string(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(restored.text.contains("restored"), "{restored:?}");
        assert_eq!(
            service
                .list(ScopeSelection::GlobalOnly, false)
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unlinked workspace still confirms a Global record by id, and the source is kept:
    /// an old `Source::Agent` record confirms to `Confirmed` without becoming `Source::User`.
    #[test]
    fn confirm_finds_a_global_record_in_an_unlinked_workspace() {
        let (dir, service) = service("confirm-global");
        let record = service
            .store()
            .create(
                &Scope::Global,
                Source::Agent,
                Kind::Fact,
                None,
                "body",
                radian_memory::Provenance::default(),
            )
            .unwrap();
        assert!(!record.verification.is_confirmed());
        let output = execute(
            &service,
            MemoryCommand::Confirm {
                global: false,
                revision: record.revision,
                id: record.id.to_string(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.text.contains("confirmed"), "{output:?}");
        let found = service.get(ScopeSelection::GlobalOnly, &record.id).unwrap();
        assert!(found.verification.is_confirmed());
        assert_eq!(found.source, Source::Agent, "confirm keeps the source");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Confirming a user record keeps `Source::User` (user records are confirmed on write).
    #[test]
    fn confirm_keeps_the_user_source() {
        let (dir, service) = service("confirm-user");
        let record = service
            .add(WriteScope::Global, Kind::Fact, None, "body".into())
            .unwrap();
        assert!(record.verification.is_confirmed());
        let output = execute(
            &service,
            MemoryCommand::Confirm {
                global: false,
                revision: record.revision,
                id: record.id.to_string(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.text.contains("confirmed"), "{output:?}");
        let found = service.get(ScopeSelection::GlobalOnly, &record.id).unwrap();
        assert!(found.verification.is_confirmed());
        assert_eq!(found.source, Source::User, "confirm keeps the source");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unlinked workspace still forgets a Global record by id: `forget` needs no `--global`.
    #[test]
    fn forget_finds_a_global_record_in_an_unlinked_workspace() {
        let (dir, service) = service("forget-global");
        let record = service
            .add(WriteScope::Global, Kind::Fact, None, "gone".into())
            .unwrap();
        let output = execute(
            &service,
            MemoryCommand::Forget {
                global: false,
                yes: false,
                id: record.id.to_string(),
            },
            true,
            false,
        )
        .unwrap();
        assert!(output.text.contains("forgot"), "{output:?}");
        assert!(
            service
                .list(ScopeSelection::GlobalOnly, true)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A linked workspace acts on the Project record the id names, without `--project`.
    #[test]
    fn a_linked_workspace_acts_on_the_project_record() {
        let (dir, service) = service("linked-project");
        service.create_project().unwrap();
        let record = service
            .add(WriteScope::Project, Kind::Fact, None, "project body".into())
            .unwrap();
        let output = execute(
            &service,
            MemoryCommand::Edit {
                global: false,
                revision: record.revision,
                id: record.id.to_string(),
                content: "edited".into(),
            },
            false,
            false,
        )
        .unwrap();
        assert!(output.text.contains("revised"), "{output:?}");
        let found = service
            .get(ScopeSelection::ProjectOnly, &record.id)
            .unwrap();
        assert_eq!(found.content, "edited");
        assert!(!found.scope.is_global());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An explicit scope keeps today's meaning: it never reaches the other scope, even by id.
    #[test]
    fn an_explicit_scope_never_crosses_to_the_other_scope() {
        let (dir, service) = service("explicit");
        service.create_project().unwrap();
        let global = service
            .add(WriteScope::Global, Kind::Fact, None, "global body".into())
            .unwrap();
        let project = service
            .add(WriteScope::Project, Kind::Fact, None, "project body".into())
            .unwrap();

        // `show --project` does not see the Global record.
        let error = execute(
            &service,
            MemoryCommand::Show {
                global: false,
                project: true,
                json: false,
                id: global.id.to_string(),
            },
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, 1, "{error:?}");
        assert!(error.message.contains(&global.id.to_string()), "{error:?}");

        // `edit --global` does not see the Project record.
        let error = execute(
            &service,
            MemoryCommand::Edit {
                global: true,
                revision: project.revision,
                id: project.id.to_string(),
                content: "nope".into(),
            },
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, 1, "{error:?}");
        assert!(error.message.contains(&project.id.to_string()), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An id in no visible scope is a clear error, never the unlinked-workspace refusal.
    #[test]
    fn an_unknown_id_is_a_clear_error() {
        let (dir, service) = service("unknown-id");
        let missing = "mem-00000000000000000000000000000000";
        let error = execute(
            &service,
            MemoryCommand::Show {
                global: false,
                project: false,
                json: false,
                id: missing.into(),
            },
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, 1, "{error:?}");
        assert_eq!(error.message, format!("no memory record {missing}"));

        let error = execute(
            &service,
            MemoryCommand::Forget {
                global: false,
                yes: false,
                id: missing.into(),
            },
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, 1, "{error:?}");
        assert_eq!(error.message, format!("no memory record {missing}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `split`'s confirmation counts the records that actually exist in `from`, not the ids
    /// the user typed (dogfooding D22), and names the ones that do not.
    #[test]
    fn the_split_prompt_counts_the_records_that_exist() {
        let (dir, service) = service("split-prompt");
        let project = service.create_project().unwrap();
        let first = service
            .add(WriteScope::Project, Kind::Fact, None, "one".into())
            .unwrap();
        let second = service
            .add(WriteScope::Project, Kind::Fact, None, "two".into())
            .unwrap();

        let output = execute(
            &service,
            MemoryCommand::Split {
                yes: false,
                from: project.to_string(),
                to: "other".into(),
                ids: vec![
                    first.id.to_string(),
                    "missing".into(),
                    second.id.to_string(),
                ],
            },
            false,
            false,
        )
        .unwrap();
        let prompt = output
            .needs_confirmation
            .expect("an unconfirmed split asks");
        assert!(prompt.contains("move 2 of 3 record(s)"), "{prompt}");
        assert!(prompt.contains("not found: missing"), "{prompt}");

        // No id exists: a failure (exit 1), not a confirmation.
        let error = execute(
            &service,
            MemoryCommand::Split {
                yes: false,
                from: project.to_string(),
                to: "other".into(),
                ids: vec!["nope".into()],
            },
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, 1, "{error:?}");
        assert!(
            error.message.contains("none of the 1 record(s) exist"),
            "{error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `parse_args` accepts a quoted argument and rejects an unknown subcommand with clap's
    /// rendered error.
    #[test]
    fn parse_args_uses_the_shared_grammar() {
        let command = parse_args(&["add".into(), "hello world".into()]).unwrap();
        assert!(matches!(command, MemoryCommand::Add { content, .. } if content == "hello world"));
        let error = parse_args(&["nope".into()]).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }
}

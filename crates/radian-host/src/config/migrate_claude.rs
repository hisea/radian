//! Claude Code → Radian one-time import (ADR 0046).
//!
//! Reads a Claude **home directory the user named explicitly** (`--claude-home <dir>`, the
//! `~/.claude/` tree) and/or a **project directory the user named explicitly**
//! (`--project <dir>`), and drafts a Radian configuration from the categories that have a
//! faithful target today. Nothing is written unless the caller asks for it, and the draft is
//! only ever a **best-effort translation**: Radian does not promise compatibility with
//! Claude's file format (ADR 0029 §5's principle, applied to Claude).
//!
//! What maps where (user level lands in user-level files, project level in project-level
//! files):
//!
//! - `<claude-home>/commands/*.md` → `~/.agents/skills/<name>/SKILL.md` (frontmatter gains
//!   `name`, the body keeps `$ARGUMENTS` verbatim).
//! - `<claude-home>/skills` → one `[skills] extra_paths` entry pointing at it (never copied).
//! - `<claude-home>/settings.json` `hooks` → the `[hooks]` subset (ADR 0045): only
//!   `PreToolUse`/`PostToolUse`/`Stop`, only `type = "command"`, no `async`/`if`/`http`/
//!   `prompt`/`agent`/`context: fork`. Claude tool names are translated once, here
//!   (`Bash`→`shell`, `Edit`→`patch`, `Write`→`write`, `Read`→`read`); scripts that read
//!   `.tool_input.file_path` must use `.path` (see the report line).
//! - `<claude-home>/CLAUDE.md` → `~/.radian/AGENTS.md` (verbatim).
//! - `<project>/.claude/commands/*.md` → `<project>/.agents/skills/<name>/SKILL.md`.
//! - `<project>/.claude/skills/<name>/SKILL.md` → `<project>/.agents/skills/<name>/SKILL.md`
//!   (verbatim copy).
//!
//! What is reported as **not imported**, with the reason:
//!
//! - `<claude-home>/.claude.json` `mcpServers`: not imported (there is no `[mcp]` section
//!   yet); reported per server.
//! - `<project>/.claude/settings.json` `hooks`: not imported (project hooks are not loaded
//!   yet); reported per item.
//! - `<project>/.mcp.json`: never translated; reported as not read by the runtime.
//!
//! Every other dropped item (`if`, `async`, http hooks, `context: fork`, agents, plugins,
//! LSP, `.claude/rules`, `CLAUDE.local.md`, memory, sessions) gets one report line each.
//!
//! Safety rules this module is built around (each one has a test):
//!
//! - **Credential files are never read.** `auth.json` and `.credentials.json` are not
//!   opened, not even `stat`ed, wherever they might live.
//! - **No secret value is ever copied or printed.** A literal secret becomes an ADR 0008 §3
//!   environment reference with a suggested name; the note never shows the value.
//! - **No network, no command execution.** The only inputs are files under the two named
//!   directories.
//! - The generated config draft is validated with the real reader ([`super::parse`]) and
//!   every generated skill is validated with the skill rules before anything is printed or
//!   written. `radian config import-claude --write` never overwrites a file that exists:
//!   the configuration is merged into the existing file, format-preserving, through
//!   [`super::preserve::edit_preserving`] (ADR 0008 §5: comments, order and unknown keys
//!   survive; `extra_paths` gains only missing entries; `[hooks]` gains only new entries;
//!   anything that would have to change to land is left alone with one report line naming
//!   the key). `AGENTS.md` and `SKILL.md` targets are created with `create_new`: an
//!   existing target is skipped with one report line, and the other targets are still
//!   written.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use toml::Value as Toml;
use toml_edit::{Array, ArrayOfTables, Item, Table, value};

use super::preserve::PreserveError;

/// One generated skill file: `<skills-dir>/<name>/SKILL.md` with this text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSkill {
    /// The skill name (the source file stem, or the source skill directory name).
    pub name: String,
    /// The full `SKILL.md` text, frontmatter included.
    pub content: String,
}

/// A drafted Claude import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeDraft {
    /// The TOML text: `schema_version = 1` plus only the keys the Radian reader implements
    /// (`[skills] extra_paths`, `[hooks]`).
    pub config_toml: String,
    /// The user-level `CLAUDE.md` body, when present (goes to `~/.radian/AGENTS.md` verbatim).
    pub user_agents_md: Option<String>,
    /// Skills from `<claude-home>/commands/*.md` (go to `~/.agents/skills/<name>/SKILL.md`).
    pub user_skills: Vec<ClaudeSkill>,
    /// Skills from `<project>/.claude/commands/*.md` plus `<project>/.claude/skills/*/`.
    pub project_skills: Vec<ClaudeSkill>,
    /// What was mapped, what was skipped and why. Printed to stderr. Lines name keys, never
    /// secret values.
    pub notes: Vec<String>,
    /// Every file actually read, as given (never credential files). Tests assert the
    /// credential names never appear here.
    pub files_read: Vec<PathBuf>,
}

/// What `write_claude_draft` did. Nothing that existed was changed: the configuration was
/// merged, and every other target was created new or skipped with one line each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeWriteReport {
    /// Files this run created.
    pub written: Vec<PathBuf>,
    /// One line per target not written (an existing file) and per merge part not landed
    /// (a conflict naming the key). For stderr; never a secret value.
    pub skipped: Vec<String>,
}

/// Where `write_claude_draft` puts the draft. The CLI resolves these from the environment;
/// tests pass scratch directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTargets {
    /// The Radian configuration file (the `--config`/`RADIAN_CONFIG` file, or
    /// `~/.radian/config/config.toml`).
    pub config_path: PathBuf,
    /// `~/.agents/skills/` (user-level skill root).
    pub user_skills_dir: PathBuf,
    /// `~/.radian/AGENTS.md`.
    pub user_agents_path: PathBuf,
    /// `<project>/.agents/skills/` (`None` when no `--project` was given).
    pub project_skills_dir: Option<PathBuf>,
}

/// Why the import stopped.
#[derive(Debug)]
pub enum ClaudeMigrateError {
    /// Neither `--claude-home` nor `--project` was given.
    MissingSource,
    /// The named directory does not exist or is not a directory.
    NotADirectory { path: PathBuf },
    /// A file could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A JSON file did not parse.
    Parse { path: PathBuf, message: String },
    /// The generated draft is not accepted by the reader: an internal error, never written.
    InvalidDraft { message: String },
    /// The configuration file is newer than this build supports: nothing was written.
    ReadOnly { path: PathBuf, supported: u64 },
    /// The write itself failed.
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ClaudeMigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeMigrateError::MissingSource => {
                write!(
                    f,
                    "nothing to import: pass --claude-home <dir> and/or --project <dir>"
                )
            }
            ClaudeMigrateError::NotADirectory { path } => {
                write!(f, "{}: not a directory", path.display())
            }
            ClaudeMigrateError::Read { path, source } => {
                write!(f, "{}: cannot read: {source}", path.display())
            }
            ClaudeMigrateError::Parse { path, message } => {
                write!(f, "{}: invalid JSON: {message}", path.display())
            }
            ClaudeMigrateError::InvalidDraft { message } => {
                write!(
                    f,
                    "the generated draft is not a valid Radian configuration: {message}"
                )
            }
            ClaudeMigrateError::ReadOnly { path, supported } => write!(
                f,
                "{}: the configuration file is newer than the supported v{supported}; \
                 nothing was written",
                path.display()
            ),
            ClaudeMigrateError::Write { path, source } => {
                write!(f, "{}: cannot write: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ClaudeMigrateError {}

impl From<super::preserve::PreserveError> for ClaudeMigrateError {
    fn from(error: super::preserve::PreserveError) -> Self {
        match error {
            super::preserve::PreserveError::ReadOnly { path, supported } => {
                ClaudeMigrateError::ReadOnly { path, supported }
            }
            super::preserve::PreserveError::Read { path, source } => {
                ClaudeMigrateError::Read { path, source }
            }
            super::preserve::PreserveError::Parse { path, message } => {
                ClaudeMigrateError::Parse { path, message }
            }
            // The pre-pass keeps the two closures in sync, so a verify failure is an
            // internal error, never a user-facing conflict.
            super::preserve::PreserveError::Verify { path, keys } => {
                ClaudeMigrateError::InvalidDraft {
                    message: format!("{}: refusing a write that changes {keys}", path.display()),
                }
            }
            super::preserve::PreserveError::Write { path, source } => {
                ClaudeMigrateError::Write { path, source }
            }
        }
    }
}

/// Reads the named Claude directories and returns the draft. `None` means "not given": that
/// tree is never touched. At least one must be `Some`.
pub fn import_claude(
    claude_home: Option<&Path>,
    project: Option<&Path>,
) -> Result<ClaudeDraft, ClaudeMigrateError> {
    if claude_home.is_none() && project.is_none() {
        return Err(ClaudeMigrateError::MissingSource);
    }
    if let Some(dir) = claude_home {
        require_dir(dir)?;
    }
    if let Some(dir) = project {
        require_dir(dir)?;
    }

    let mut draft = Draft::default();
    // The credential files are deliberately never opened — not even to check whether they
    // exist. Saved credentials are never migrated.
    draft.note(
        "credentials are not migrated: `auth.json` and `.credentials.json` are never read. \
         Add entries to `~/.radian/credentials/auth.json` yourself.",
    );

    if let Some(home) = claude_home {
        map_user_commands(home, &mut draft)?;
        map_user_skills_ref(home, &mut draft);
        map_user_claude_json(home, &mut draft)?;
        map_user_settings(home, &mut draft)?;
        map_user_claude_md(home, &mut draft)?;
        map_user_dropped(home, &mut draft);
    }
    if let Some(dir) = project {
        map_project_commands(dir, &mut draft)?;
        map_project_skills(dir, &mut draft)?;
        map_project_mcp_json(dir, &mut draft);
        map_project_settings(dir, &mut draft)?;
        map_project_dropped(dir, &mut draft);
    }

    let config_toml = draft.render()?;
    let built = ClaudeDraft {
        config_toml,
        user_agents_md: draft.user_agents_md,
        user_skills: draft.user_skills,
        project_skills: draft.project_skills,
        notes: draft.notes,
        files_read: draft.files_read,
    };
    // Every generated skill must be loadable by the skill rules, or it never leaves here.
    for skill in built.user_skills.iter().chain(built.project_skills.iter()) {
        validate_skill(&skill.name, &skill.content).map_err(|message| {
            ClaudeMigrateError::InvalidDraft {
                message: format!("skill `{}`: {message}", skill.name),
            }
        })?;
    }
    Ok(built)
}

fn require_dir(dir: &Path) -> Result<(), ClaudeMigrateError> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(ClaudeMigrateError::NotADirectory {
            path: dir.to_path_buf(),
        }),
        Err(source) => Err(ClaudeMigrateError::Read {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Writes the draft to `targets`, creating parent directories. Nothing that exists is
/// ever changed or removed:
///
/// - the configuration is merged into the existing file through
///   [`super::preserve::edit_preserving`] (ADR 0008 §5): comments, order and unknown keys
///   survive byte-for-byte; `extra_paths` gains only entries that are not already listed;
///   `[hooks]` gains only matcher entries that are not already present; anything the
///   import would have to change to land is left alone with one report line naming the
///   key. A file the reader refuses, or one read read-only (a newer `schema_version`),
///   stops the whole write with an error and nothing is written.
/// - `AGENTS.md` and every `SKILL.md` target is created with `create_new`: an existing
///   file is skipped with one report line (`<path>: exists, not written; the draft is in
///   the dry-run output`), and the other targets are still written.
///
/// Project-level skill files are trust-gate resources (ADR 0031 T1); the CLI prints that
/// notice before calling this.
pub fn write_claude_draft(
    draft: &ClaudeDraft,
    targets: &ClaudeTargets,
) -> Result<ClaudeWriteReport, ClaudeMigrateError> {
    // Validate first: what lands on disk must be the accepted document, so a bad draft
    // writes nothing.
    let parsed = super::parse(Path::new("<draft>"), &draft.config_toml).map_err(|error| {
        ClaudeMigrateError::InvalidDraft {
            message: error.to_string(),
        }
    })?;
    for skill in draft.user_skills.iter().chain(draft.project_skills.iter()) {
        validate_skill(&skill.name, &skill.content).map_err(|message| {
            ClaudeMigrateError::InvalidDraft {
                message: format!("skill `{}`: {message}", skill.name),
            }
        })?;
    }

    let mut report = ClaudeWriteReport::default();
    merge_config(
        &targets.config_path,
        &parsed.skills.extra_paths,
        &parsed.hooks,
        &mut report,
    )?;
    if let Some(body) = &draft.user_agents_md {
        write_one(&targets.user_agents_path, body, true, &mut report)?;
    }
    for skill in &draft.user_skills {
        let path = targets.user_skills_dir.join(&skill.name).join("SKILL.md");
        write_one(&path, &skill.content, false, &mut report)?;
    }
    if let Some(dir) = &targets.project_skills_dir {
        for skill in &draft.project_skills {
            let path = dir.join(&skill.name).join("SKILL.md");
            write_one(&path, &skill.content, false, &mut report)?;
        }
    }
    // Read back what landed through the same reader: the file on disk must be accepted.
    let text = std::fs::read_to_string(&targets.config_path).map_err(|source| {
        ClaudeMigrateError::Read {
            path: targets.config_path.clone(),
            source,
        }
    })?;
    super::parse(&targets.config_path, &text).map_err(|error| {
        ClaudeMigrateError::InvalidDraft {
            message: error.to_string(),
        }
    })?;
    Ok(report)
}

/// The keys one merge may touch, for the verify error.
const MERGE_KEYS: &str = "`[skills] extra_paths`/`[hooks]`";

/// One event's imported matcher entries to append.
struct PlannedHooks {
    event: &'static str,
    entries: Vec<PlannedEntry>,
}

/// One matcher entry in merge-plan shape: the matcher and every hook's `(command, timeout)`.
type PlannedEntry = (Option<String>, Vec<(String, Option<u64>)>);

/// What [`plan_merge`] decided: additions plus conflict lines.
#[derive(Default)]
struct PlannedMerge {
    add_paths: Vec<String>,
    add_hooks: Vec<PlannedHooks>,
    conflicts: Vec<String>,
}

/// Merges the imported paths and hooks into the configuration at `path` (created when
/// absent), format-preserving. Additions that are already present are skipped; shapes the
/// merge cannot extend are left alone with one conflict line each in `report`.
fn merge_config(
    path: &Path,
    add_paths: &[String],
    add_hooks: &super::HooksConfig,
    report: &mut ClaudeWriteReport,
) -> Result<(), ClaudeMigrateError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ClaudeMigrateError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if let Some(text) = &existing {
        let baseline = super::parse(path, text).map_err(|error| ClaudeMigrateError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if baseline.read_only {
            return Err(ClaudeMigrateError::ReadOnly {
                path: path.to_path_buf(),
                supported: super::SUPPORTED_SCHEMA_VERSION,
            });
        }
    }
    let plan = plan_merge(existing.as_deref(), path, add_paths, add_hooks)?;
    report.skipped.extend(plan.conflicts);

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let _ = crate::home::ensure_private_dir(parent);
    }
    super::preserve::edit_preserving(
        path,
        false,
        MERGE_KEYS,
        |doc| {
            if !plan.add_paths.is_empty() {
                let skills = doc.entry("skills").or_insert(Item::Table(Table::new()));
                let table = skills
                    .as_table_like_mut()
                    .ok_or_else(|| PreserveError::Verify {
                        path: path.to_path_buf(),
                        keys: MERGE_KEYS,
                    })?;
                let array = table
                    .entry("extra_paths")
                    .or_insert(Item::Value(toml_edit::Value::Array(Array::new())));
                let array = array.as_array_mut().ok_or_else(|| PreserveError::Verify {
                    path: path.to_path_buf(),
                    keys: MERGE_KEYS,
                })?;
                for entry in &plan.add_paths {
                    array.push(entry.as_str());
                }
            }
            for event in &plan.add_hooks {
                if event.entries.is_empty() {
                    continue;
                }
                // A missing event starts as an inline array under an inline `hooks`
                // parent (so the parent stays inline), `[[...]]` sections otherwise.
                let inline_parent = matches!(
                    doc.get("hooks"),
                    Some(Item::Value(toml_edit::Value::InlineTable(_)))
                );
                let root = doc.entry("hooks").or_insert(Item::Table(Table::new()));
                let table = root
                    .as_table_like_mut()
                    .ok_or_else(|| PreserveError::Verify {
                        path: path.to_path_buf(),
                        keys: MERGE_KEYS,
                    })?;
                let list = table.entry(event.event).or_insert_with(|| {
                    if inline_parent {
                        Item::Value(toml_edit::Value::Array(Array::new()))
                    } else {
                        Item::ArrayOfTables(ArrayOfTables::new())
                    }
                });
                // The event list keeps its form: tables append to `[[...]]`, inline tables
                // to `[...]`. Anything else was already a conflict in the plan.
                if let Some(tables) = list.as_array_of_tables_mut() {
                    for (matcher, commands) in &event.entries {
                        tables.push(hook_table(matcher, commands));
                    }
                } else if let Some(array) = list.as_array_mut() {
                    for (matcher, commands) in &event.entries {
                        array.push(toml_edit::Value::InlineTable(inline_hook_table(
                            matcher, commands,
                        )));
                    }
                } else {
                    return Err(PreserveError::Verify {
                        path: path.to_path_buf(),
                        keys: MERGE_KEYS,
                    });
                }
            }
            Ok(())
        },
        |expected| {
            for entry in &plan.add_paths {
                expected.skills.extra_paths.push(entry.clone());
            }
            for event in &plan.add_hooks {
                for (matcher, commands) in &event.entries {
                    let matcher_entry = super::HookMatcher {
                        matcher: matcher.clone(),
                        hooks: commands
                            .iter()
                            .map(|(command, timeout)| super::HookCommand {
                                command: command.clone(),
                                timeout_secs: *timeout,
                            })
                            .collect(),
                    };
                    match event.event {
                        "PreToolUse" => expected.hooks.pre_tool_use.push(matcher_entry),
                        "PostToolUse" => expected.hooks.post_tool_use.push(matcher_entry),
                        _ => expected.hooks.stop.push(matcher_entry),
                    }
                }
            }
        },
    )?;
    Ok(())
}

/// Works out what the merge may append. Shapes the reader accepts but the merge cannot
/// extend (a non-table `hooks`, a non-list event) become conflict lines; shapes the reader
/// refuses never reach here (the baseline parse above already failed on them).
fn plan_merge(
    existing: Option<&str>,
    path: &Path,
    add_paths: &[String],
    add_hooks: &super::HooksConfig,
) -> Result<PlannedMerge, ClaudeMigrateError> {
    let mut plan = PlannedMerge::default();
    let Some(text) = existing else {
        plan.add_paths = add_paths.to_owned();
        plan.add_hooks = planned_hooks(add_hooks);
        return Ok(plan);
    };
    let doc =
        text.parse::<toml_edit::DocumentMut>()
            .map_err(|error| ClaudeMigrateError::Parse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
    let present_paths: Vec<String> = doc
        .get("skills")
        .and_then(|skills| skills.as_table_like())
        .and_then(|skills| skills.get("extra_paths"))
        .and_then(|paths| paths.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    for entry in add_paths {
        if !present_paths.contains(entry) && !plan.add_paths.contains(entry) {
            plan.add_paths.push(entry.clone());
        }
    }
    let hooks = doc.get("hooks");
    if hooks.is_none() {
        plan.add_hooks = planned_hooks(add_hooks);
        return Ok(plan);
    }
    let Some(table) = hooks.and_then(|hooks| hooks.as_table_like()) else {
        plan.conflicts
            .push("`hooks`: is not a table, hooks left unchanged".to_owned());
        return Ok(plan);
    };
    for (event, entries) in [
        ("PreToolUse", &add_hooks.pre_tool_use),
        ("PostToolUse", &add_hooks.post_tool_use),
        ("Stop", &add_hooks.stop),
    ] {
        let imported: Vec<PlannedEntry> = entries
            .iter()
            .map(|entry| {
                (
                    entry.matcher.clone(),
                    entry
                        .hooks
                        .iter()
                        .map(|hook| (hook.command.clone(), hook.timeout_secs))
                        .collect(),
                )
            })
            .collect();
        if imported.is_empty() {
            continue;
        }
        let present = match table.get(event) {
            None => Vec::new(),
            Some(list) => match existing_hook_entries(list) {
                Some(entries) => entries,
                None => {
                    plan.conflicts.push(format!(
                        "`hooks.{event}`: is not a hook list, left unchanged"
                    ));
                    continue;
                }
            },
        };
        let mut planned = PlannedHooks {
            event,
            entries: Vec::new(),
        };
        for entry in imported {
            if !present.contains(&entry) && !planned.entries.contains(&entry) {
                planned.entries.push(entry);
            }
        }
        plan.add_hooks.push(planned);
    }
    Ok(plan)
}

/// The imported hooks in merge-plan shape (every entry is new: there is no file yet).
fn planned_hooks(hooks: &super::HooksConfig) -> Vec<PlannedHooks> {
    [
        ("PreToolUse", &hooks.pre_tool_use),
        ("PostToolUse", &hooks.post_tool_use),
        ("Stop", &hooks.stop),
    ]
    .into_iter()
    .map(|(event, entries)| PlannedHooks {
        event,
        entries: entries
            .iter()
            .map(|entry| {
                (
                    entry.matcher.clone(),
                    entry
                        .hooks
                        .iter()
                        .map(|hook| (hook.command.clone(), hook.timeout_secs))
                        .collect(),
                )
            })
            .collect(),
    })
    .collect()
}

/// The canonical entries of an existing `hooks.<Event>` list: `None` when the value is not
/// a list of tables (a conflict the merge leaves alone).
fn existing_hook_entries(list: &Item) -> Option<Vec<PlannedEntry>> {
    if let Some(tables) = list.as_array_of_tables() {
        return Some(tables.iter().map(table_hook_entry).collect());
    }
    if let Some(array) = list.as_array() {
        let mut entries = Vec::new();
        for value in array {
            let table = value.as_inline_table()?;
            entries.push(inline_hook_entry(table));
        }
        return Some(entries);
    }
    None
}

/// One matcher entry as a `[[hooks.<Event>]]` table.
fn hook_table(matcher: &Option<String>, commands: &[(String, Option<u64>)]) -> Table {
    let mut entry = Table::new();
    if let Some(matcher) = matcher {
        entry["matcher"] = value(matcher.as_str());
    }
    let mut hooks = ArrayOfTables::new();
    for (command, timeout) in commands {
        let mut hook = Table::new();
        hook["type"] = value("command");
        hook["command"] = value(command.as_str());
        if let Some(secs) = timeout {
            hook["timeout"] = value(*secs as i64);
        }
        hooks.push(hook);
    }
    entry["hooks"] = Item::ArrayOfTables(hooks);
    entry
}

/// One matcher entry as an inline table (for `hooks.<Event> = [{ ... }]` lists: the event
/// list keeps its form).
fn inline_hook_table(
    matcher: &Option<String>,
    commands: &[(String, Option<u64>)],
) -> toml_edit::InlineTable {
    let mut entry = toml_edit::InlineTable::new();
    if let Some(matcher) = matcher {
        entry.insert("matcher", toml_edit::Value::from(matcher.as_str()));
    }
    let mut hooks = Array::new();
    for (command, timeout) in commands {
        let mut hook = toml_edit::InlineTable::new();
        hook.insert("type", toml_edit::Value::from("command"));
        hook.insert("command", toml_edit::Value::from(command.as_str()));
        if let Some(secs) = timeout {
            hook.insert("timeout", toml_edit::Value::from(*secs as i64));
        }
        hooks.push(toml_edit::Value::InlineTable(hook));
    }
    entry.insert("hooks", toml_edit::Value::Array(hooks));
    entry
}

/// The canonical shape of one existing matcher entry: the matcher and every hook's
/// `(command, timeout)`.
fn table_hook_entry(table: &Table) -> (Option<String>, Vec<(String, Option<u64>)>) {
    let matcher = table
        .get("matcher")
        .and_then(|matcher| matcher.as_str())
        .map(str::to_owned);
    let hooks = table
        .get("hooks")
        .and_then(|hooks| hooks.as_array_of_tables())
        .map(|hooks| hooks.iter().map(hook_command_entry).collect())
        .unwrap_or_default();
    (matcher, hooks)
}

/// The same shape for an inline-table entry (`hooks.<Event> = [{ ... }]`).
fn inline_hook_entry(
    table: &toml_edit::InlineTable,
) -> (Option<String>, Vec<(String, Option<u64>)>) {
    let matcher = table
        .get("matcher")
        .and_then(|matcher| matcher.as_str())
        .map(str::to_owned);
    let hooks = table
        .get("hooks")
        .and_then(|hooks| hooks.as_array())
        .map(|hooks| {
            hooks
                .iter()
                .filter_map(|hook| hook.as_inline_table())
                .map(|hook| {
                    (
                        hook.get("command")
                            .and_then(|command| command.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        hook.get("timeout")
                            .and_then(|timeout| timeout.as_integer())
                            .and_then(|secs| u64::try_from(secs).ok()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (matcher, hooks)
}

/// One existing hook's `(command, timeout)` in canonical shape.
fn hook_command_entry(table: &Table) -> (String, Option<u64>) {
    (
        table
            .get("command")
            .and_then(|command| command.as_str())
            .unwrap_or_default()
            .to_owned(),
        table
            .get("timeout")
            .and_then(|timeout| timeout.as_integer())
            .and_then(|secs| u64::try_from(secs).ok()),
    )
}

/// Creates `path` with mode `0600` (unix) and writes `text`, or records one skip line when
/// the target exists. `create_new` means an existing file is never truncated, replaced, or
/// removed. The other targets are unaffected by one skip.
fn write_one(
    path: &Path,
    text: &str,
    private_parent: bool,
    report: &mut ClaudeWriteReport,
) -> Result<(), ClaudeMigrateError> {
    if std::fs::symlink_metadata(path).is_ok() {
        report.skipped.push(format!(
            "`{}`: exists, not written; the draft is in the dry-run output",
            path.display()
        ));
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if private_parent {
            let _ = crate::home::ensure_private_dir(parent);
        } else {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    write_private_new(path, text)?;
    report.written.push(path.to_path_buf());
    Ok(())
}

/// Creates `path` with mode `0600` (unix) and writes `text`. `create_new` means an existing
/// file is never truncated: callers skip existing targets instead of replacing them.
fn write_private_new(path: &Path, text: &str) -> Result<(), ClaudeMigrateError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|source| ClaudeMigrateError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    let result = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(source) = result {
        let _ = std::fs::remove_file(path);
        return Err(ClaudeMigrateError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

// ---------- reading helpers ----------

/// Reads one JSON file, or notes that it is missing. Anything other than "not found" is a
/// real failure. The path is recorded in `files_read` only when it was actually opened.
fn read_json(dir: &Path, rel: &str, draft: &mut Draft) -> Result<Option<Json>, ClaudeMigrateError> {
    let path = dir.join(rel);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => {
            draft.files_read.push(path.clone());
            text
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            draft.note(format!("`{rel}`: not found, skipped"));
            return Ok(None);
        }
        Err(source) => return Err(ClaudeMigrateError::Read { path, source }),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| ClaudeMigrateError::Parse {
            path,
            message: error.to_string(),
        })
}

// ---------- user level ----------

/// `<claude-home>/commands/*.md` → user skills. The body keeps `$ARGUMENTS` verbatim; the
/// frontmatter gains `name` (the file stem) and keeps `description` when it is usable.
fn map_user_commands(home: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let dir = home.join("commands");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            draft.note("`commands/`: not found, skipped");
            return Ok(());
        }
        Err(source) => {
            return Err(ClaudeMigrateError::Read { path: dir, source });
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ClaudeMigrateError::Read {
            path: dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let text = std::fs::read_to_string(&path).map_err(|source| ClaudeMigrateError::Read {
            path: path.clone(),
            source,
        })?;
        draft.files_read.push(path.clone());
        let name = skill_name(&stem);
        let (description, body) = command_skill_body(&stem, &text);
        if name != stem {
            draft.note(format!(
                "`commands/{stem}.md`: the name was sanitized to `{name}`"
            ));
        }
        if description_is_placeholder(&text) {
            draft.note(format!(
                "`commands/{stem}.md`: no usable `description`, so the skill uses \
                 `Imported from Claude command {name}.`; edit it"
            ));
        }
        draft.user_skills.push(ClaudeSkill {
            name: name.clone(),
            content: render_skill(&name, &description, &body),
        });
        names.push(name);
    }
    names.sort();
    if names.is_empty() {
        draft.note("`commands/`: no `*.md` files, nothing to import");
    } else {
        draft.note(format!(
            "`commands/`: imported {} command(s) as user skills: {}",
            names.len(),
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// `<claude-home>/skills` → one `[skills] extra_paths` entry. The directory is never copied.
fn map_user_skills_ref(home: &Path, draft: &mut Draft) {
    let dir = home.join("skills");
    match std::fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => {
            draft.extra_paths.push(dir.display().to_string());
            draft.note(format!(
                "`skills/`: not copied; `[skills] extra_paths` points at `{}` (only read \
                 because it is listed)",
                dir.display()
            ));
        }
        _ => draft.note("`skills/`: not found, skipped"),
    }
}

/// `<claude-home>/.claude.json` `mcpServers`: not imported. Literal secrets become env
/// references with suggested names; values are never shown.
fn map_user_claude_json(home: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let Some(json) = read_json(home, ".claude.json", draft)? else {
        return Ok(());
    };
    let Some(servers) = json.get("mcpServers").and_then(Json::as_object) else {
        draft.note("`.claude.json`: no `mcpServers`, nothing to report");
        return Ok(());
    };
    if servers.is_empty() {
        draft.note("`.claude.json`: `mcpServers` is empty, nothing to import");
        return Ok(());
    }
    let mut names: Vec<String> = servers.keys().cloned().collect();
    names.sort();
    for name in &names {
        draft.note(format!(
            "`.claude.json` server `{name}`: not imported: Radian reads MCP servers from \
             `--mcp-config` only; copy it to a `mcpServers` file yourself"
        ));
        if let Some(Json::Object(entry)) = servers.get(name)
            && let Some(Json::Object(env)) = entry.get("env")
        {
            for (key, value) in env {
                if let Some(text) = value.as_str()
                    && !text.is_empty()
                    && looks_like_secret_value(key, text)
                {
                    let suggested = suggested_env(&format!("{name}_{key}"));
                    draft.note(format!(
                        "`.claude.json` server `{name}` env `{key}`: a literal value was not \
                         migrated (its value is never shown); use an environment reference \
                         instead: `{{ from = \"env\", name = \"{suggested}\" }}`"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `<claude-home>/settings.json` `hooks` → `[hooks]`. Only the ADR 0045 subset survives;
/// everything else gets one report line each.
fn map_user_settings(home: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let Some(json) = read_json(home, "settings.json", draft)? else {
        return Ok(());
    };
    let Some(hooks) = json.get("hooks").and_then(Json::as_object) else {
        draft.note("`settings.json`: no `hooks`, nothing to import");
        return Ok(());
    };
    let mut any_command = false;
    for (event, entries) in hooks {
        let target = match event.as_str() {
            "PreToolUse" => Some(HookEvent::Pre),
            "PostToolUse" => Some(HookEvent::Post),
            "Stop" => Some(HookEvent::Stop),
            _ => {
                draft.note(format!(
                    "`settings.json` hook event `{event}`: not imported (only \
                     `PreToolUse`/`PostToolUse`/`Stop` are supported)"
                ));
                continue;
            }
        };
        let Some(target) = target else { continue };
        let Some(items) = entries.as_array() else {
            draft.note(format!(
                "`settings.json` hook event `{event}`: not an array, skipped"
            ));
            continue;
        };
        for (index, item) in items.iter().enumerate() {
            let what = format!("`settings.json` `hooks.{event}[{index}]`");
            let Some(entry) = item.as_object() else {
                draft.note(format!("{what}: not an object, skipped"));
                continue;
            };
            if let Some(reason) = entry_drop_reason(entry, "entry") {
                draft.note(format!("{what}: not imported ({reason})"));
                continue;
            }
            let matcher = match entry.get("matcher") {
                None => None,
                Some(Json::String(matcher)) if matcher.is_empty() => None,
                Some(Json::String(matcher)) => translate_matcher(matcher, &what, draft),
                Some(_) => {
                    draft.note(format!("{what}: `matcher` is not a string, skipped"));
                    continue;
                }
            };
            let hooks = match entry.get("hooks").and_then(Json::as_array) {
                Some(hooks) => hooks,
                None => {
                    draft.note(format!("{what}: no `hooks` array, skipped"));
                    continue;
                }
            };
            if hooks.is_empty() {
                draft.note(format!("{what}: no hooks, skipped"));
                continue;
            }
            let mut commands: Vec<(String, Option<u64>)> = Vec::new();
            for hook in hooks {
                let Some(hook) = hook.as_object() else {
                    draft.note(format!("{what}: a hook is not an object, skipped"));
                    continue;
                };
                match hook_command(hook) {
                    Ok((command, timeout)) => {
                        any_command = true;
                        commands.push((command, timeout));
                    }
                    Err(reason) => {
                        draft.note(format!("{what}: a hook was not imported ({reason})"))
                    }
                }
            }
            if commands.is_empty() {
                continue;
            }
            draft.hooks.push((target, matcher, commands));
        }
    }
    if any_command {
        draft.note(
            "hook scripts that read `.tool_input.file_path` must use `.path` instead \
             (`old_string`→`.old`, `new_string`→`.new`); matchers name Radian tools \
             (`shell`, `patch`, `write`, `read`, …)",
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookEvent {
    Pre,
    Post,
    Stop,
}

/// Why an entry (the outer `{ matcher, hooks }` object) is dropped, if it is.
fn entry_drop_reason(entry: &serde_json::Map<String, Json>, _kind: &str) -> Option<String> {
    for key in ["async", "if", "http", "prompt", "agent"] {
        if entry.contains_key(key) {
            return Some(format!("`{key}` hooks are not supported"));
        }
    }
    if let Some(context) = entry.get("context").and_then(Json::as_str)
        && context == "fork"
    {
        return Some("`context: fork` hooks are not supported".into());
    }
    None
}

/// Reads one inner `{ type, command, timeout }` hook. `Err` is the one-line drop reason.
fn hook_command(hook: &serde_json::Map<String, Json>) -> Result<(String, Option<u64>), String> {
    if let Some(kind) = hook.get("type").and_then(Json::as_str) {
        if kind != "command" {
            return Err(format!("`type: \"{kind}\"` hooks are not supported"));
        }
    } else {
        return Err("a hook without `type: \"command\"` is not supported".into());
    }
    for key in ["async", "if", "http", "prompt", "agent"] {
        if hook.contains_key(key) {
            return Err(format!("`{key}` hooks are not supported"));
        }
    }
    if let Some(context) = hook.get("context").and_then(Json::as_str)
        && context == "fork"
    {
        return Err("`context: fork` hooks are not supported".into());
    }
    let Some(command) = hook.get("command").and_then(Json::as_str) else {
        return Err("a hook without a `command` string is not supported".into());
    };
    if command.is_empty() {
        return Err("a hook with an empty `command` is not supported".into());
    }
    let timeout = match hook.get("timeout") {
        None => None,
        Some(Json::Number(timeout)) => timeout.as_u64().map(|secs| {
            if secs == 0 {
                super::DEFAULT_HOOK_TIMEOUT_SECS
            } else {
                secs
            }
        }),
        Some(_) => return Err("a hook with a bad `timeout` is not supported".into()),
    };
    Ok((command.to_owned(), timeout))
}

/// Translates one Claude matcher to Radian tool names. Alternatives are split on `|` and
/// mapped one by one; unknown names are kept verbatim (they simply never match) and named
/// once in the report.
fn translate_matcher(raw: &str, what: &str, draft: &mut Draft) -> Option<String> {
    if raw == "*" {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for part in raw.split('|') {
        let mapped = match part {
            "Bash" => "shell".to_owned(),
            "Edit" => "patch".to_owned(),
            "Write" => "write".to_owned(),
            "Read" => "read".to_owned(),
            "Glob" => "glob".to_owned(),
            "Grep" => "grep".to_owned(),
            _ => {
                draft.note(format!(
                    "{what}: matcher `{part}` names no Radian tool, kept verbatim (it never \
                     matches until renamed)"
                ));
                part.to_owned()
            }
        };
        if !out.contains(&mapped) {
            out.push(mapped);
        }
    }
    Some(out.join("|"))
}

/// `<claude-home>/CLAUDE.md` → user `AGENTS.md` body (verbatim).
fn map_user_claude_md(home: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let path = home.join("CLAUDE.md");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            draft.files_read.push(path);
            draft.user_agents_md = Some(text);
            draft.note("`CLAUDE.md`: imported as `~/.radian/AGENTS.md` (verbatim)");
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            draft.note("`CLAUDE.md`: not found, skipped");
            Ok(())
        }
        Err(source) => Err(ClaudeMigrateError::Read { path, source }),
    }
}

/// User-level items that are never portable: one report line each.
fn map_user_dropped(home: &Path, draft: &mut Draft) {
    // Agents and plugins have no Radian equivalent.
    for (rel, label) in [
        ("agents", "agents"),
        (".claude/agents", "agents"),
        ("plugins", "plugins"),
    ] {
        if home.join(rel).exists() {
            draft.note(format!(
                "`{rel}/`: {label} are not imported (no equivalent)"
            ));
        } else {
            draft.note(format!("`{rel}/`: not present, nothing to import"));
        }
    }
    // `.claude/rules` is a project path; at user level the sibling is `rules/`.
    if home.join("rules").exists() {
        draft.note("`rules/`: rules are not imported (no equivalent)");
    } else {
        draft.note("`rules/`: not present, nothing to import");
    }
    if home.join("CLAUDE.local.md").exists() {
        draft.note("`CLAUDE.local.md`: local-only instructions are not imported");
    } else {
        draft.note("`CLAUDE.local.md`: not present, nothing to import");
    }
    // Memory and sessions are deliberately out of scope (ADR 0021, journal semantics).
    for rel in ["MEMORY.md", "projects", "todos", "history.jsonl"] {
        if home.join(rel).exists() {
            draft.note(format!("`{rel}`: memory/sessions are not imported"));
        } else {
            draft.note(format!("`{rel}`: not present, nothing to import"));
        }
    }
    // LSP settings have no equivalent.
    draft.note("LSP settings: not imported (no equivalent)");
}

// ---------- project level ----------

/// `<project>/.claude/commands/*.md` → project skills (same shaping as user commands).
fn map_project_commands(dir: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let commands = dir.join(".claude/commands");
    let entries = match std::fs::read_dir(&commands) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            draft.note("`.claude/commands/`: not found, skipped");
            return Ok(());
        }
        Err(source) => {
            return Err(ClaudeMigrateError::Read {
                path: commands,
                source,
            });
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ClaudeMigrateError::Read {
            path: commands.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let text = std::fs::read_to_string(&path).map_err(|source| ClaudeMigrateError::Read {
            path: path.clone(),
            source,
        })?;
        draft.files_read.push(path);
        let name = skill_name(&stem);
        let (description, body) = command_skill_body(&stem, &text);
        draft.project_skills.push(ClaudeSkill {
            name: name.clone(),
            content: render_skill(&name, &description, &body),
        });
        names.push(name);
    }
    names.sort();
    if names.is_empty() {
        draft.note("`.claude/commands/`: no `*.md` files, nothing to import");
    } else {
        draft.note(format!(
            "`.claude/commands/`: imported {} command(s) as project skills: {}",
            names.len(),
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// `<project>/.claude/skills/<name>/SKILL.md` → project skills (verbatim when valid).
fn map_project_skills(dir: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let skills = dir.join(".claude/skills");
    let entries = match std::fs::read_dir(&skills) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            draft.note("`.claude/skills/`: not found, skipped");
            return Ok(());
        }
        Err(source) => {
            return Err(ClaudeMigrateError::Read {
                path: skills,
                source,
            });
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ClaudeMigrateError::Read {
            path: skills.clone(),
            source,
        })?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let file = entry.path().join("SKILL.md");
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                draft.files_read.push(file);
                let (fixed_name, content) = project_skill_file(&name, &text, draft);
                // A same-named command already won: keep the first discovery.
                if draft
                    .project_skills
                    .iter()
                    .any(|skill| skill.name == fixed_name)
                {
                    draft.note(format!(
                        "`.claude/skills/{name}/`: `{fixed_name}` already imported from \
                         `.claude/commands/`, skipped"
                    ));
                    continue;
                }
                draft.project_skills.push(ClaudeSkill {
                    name: fixed_name.clone(),
                    content,
                });
                names.push(fixed_name);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                draft.note(format!("`.claude/skills/{name}/`: no `SKILL.md`, skipped"));
            }
            Err(source) => {
                return Err(ClaudeMigrateError::Read { path: file, source });
            }
        }
    }
    names.sort();
    if names.is_empty() {
        draft.note("`.claude/skills/`: no importable skills, nothing to import");
    } else {
        draft.note(format!(
            "`.claude/skills/`: copied {} skill(s) as project skills: {}",
            names.len(),
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// `<project>/.mcp.json`: never translated. The runtime does not read it; MCP servers
/// come from `--mcp-config` only.
fn map_project_mcp_json(dir: &Path, draft: &mut Draft) {
    let path = dir.join(".mcp.json");
    match std::fs::metadata(&path) {
        Ok(_) => {
            draft.note(
                "`.mcp.json`: not translated: the runtime does not read `.mcp.json`; MCP \
                 servers come from `--mcp-config` only",
            );
            // The file is deliberately not opened: its env values may hold secrets, and
            // there is no target to translate them to yet.
        }
        Err(_) => draft.note("`.mcp.json`: not found, nothing to report"),
    }
}

/// `<project>/.claude/settings.json` hooks: read for the drop report, never translated.
fn map_project_settings(dir: &Path, draft: &mut Draft) -> Result<(), ClaudeMigrateError> {
    let rel = ".claude/settings.json";
    let Some(json) = read_json(dir, rel, draft)? else {
        return Ok(());
    };
    let Some(hooks) = json.get("hooks").and_then(Json::as_object) else {
        draft.note("`.claude/settings.json`: no `hooks`, nothing to report");
        return Ok(());
    };
    let mut count = 0usize;
    for (event, entries) in hooks {
        let Some(items) = entries.as_array() else {
            continue;
        };
        for (index, item) in items.iter().enumerate() {
            count += 1;
            let what = format!("`.claude/settings.json` `hooks.{event}[{index}]`");
            let reason = item
                .as_object()
                .and_then(|entry| entry_drop_reason(entry, "entry"))
                .unwrap_or_else(|| {
                    "not imported: Radian does not load project hooks \
                     (`.radian/hooks.json`)"
                        .to_owned()
                });
            draft.note(format!("{what}: not imported ({reason})"));
        }
    }
    if count == 0 {
        draft.note("`.claude/settings.json`: `hooks` is empty, nothing to report");
    }
    Ok(())
}

/// Project-level items that are never portable: one report line each.
fn map_project_dropped(dir: &Path, draft: &mut Draft) {
    for (rel, label) in [
        (".claude/agents", "agents"),
        (".claude/plugins", "plugins"),
        (".claude/rules", "rules"),
    ] {
        if dir.join(rel).exists() {
            draft.note(format!(
                "`{rel}/`: {label} are not imported (no equivalent)"
            ));
        } else {
            draft.note(format!("`{rel}/`: not present, nothing to import"));
        }
    }
    if dir.join("CLAUDE.local.md").exists() {
        draft.note("`CLAUDE.local.md`: local-only instructions are not imported");
    } else {
        draft.note("`CLAUDE.local.md`: not present, nothing to import");
    }
    for rel in [
        "MEMORY.md",
        ".claude/projects",
        ".claude/todos",
        ".claude/history.jsonl",
    ] {
        if dir.join(rel).exists() {
            draft.note(format!("`{rel}`: memory/sessions are not imported"));
        } else {
            draft.note(format!("`{rel}`: not present, nothing to import"));
        }
    }
    draft.note("LSP settings: not imported (no equivalent)");
}

// ---------- skill shaping ----------

/// The skill name for a command file stem: valid names pass through, anything else is
/// sanitized (and reported by the caller).
fn skill_name(stem: &str) -> String {
    if crate::skills::valid_name(stem) {
        return stem.to_owned();
    }
    let mut out = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    let cut: String = trimmed.chars().take(64).collect();
    if crate::skills::valid_name(&cut) {
        cut
    } else {
        "imported-command".to_owned()
    }
}

/// Splits a Claude command file into the skill description and body. The frontmatter
/// `description` is kept when usable; otherwise a placeholder names the source command.
fn command_skill_body(stem: &str, text: &str) -> (String, String) {
    let (front, body) = split_frontmatter(text);
    let description = front
        .get("description")
        .map(|value| value.trim().to_owned())
        .filter(|value| crate::skills::valid_description(value))
        .unwrap_or_else(|| format!("Imported from Claude command {stem}."));
    (description, body.trim().to_owned() + "\n")
}

fn description_is_placeholder(source: &str) -> bool {
    let (front, _) = split_frontmatter(source);
    !front
        .get("description")
        .map(|value| value.trim())
        .is_some_and(crate::skills::valid_description)
}

/// A project skill file: verbatim when its frontmatter already names a valid skill,
/// re-frontmatter'd otherwise (the body is always kept).
fn project_skill_file(dir_name: &str, text: &str, draft: &mut Draft) -> (String, String) {
    let (front, body) = split_frontmatter(text);
    let name = front
        .get("name")
        .map(|value| value.trim().to_owned())
        .filter(|value| crate::skills::valid_name(value))
        .unwrap_or_else(|| skill_name(dir_name));
    let description = front
        .get("description")
        .map(|value| value.trim().to_owned())
        .filter(|value| crate::skills::valid_description(value))
        .unwrap_or_else(|| format!("Imported from Claude skill {dir_name}."));
    if front.get("name").is_none_or(|value| value.trim() != name)
        || front
            .get("description")
            .is_none_or(|value| value.trim() != description)
    {
        draft.note(format!(
            "`.claude/skills/{dir_name}/`: frontmatter normalized to `name: {name}`"
        ));
    }
    (name.clone(), render_skill(&name, &description, body.trim()))
}

fn render_skill(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}")
}

/// Splits `---` frontmatter from the body. Returns the scalar frontmatter keys and the body
/// after the closing fence (or empty frontmatter and the whole text when unfenced).
fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut map = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next().is_some_and(|first| first.trim_end() == "---") {
        let mut body_start: Option<usize> = None;
        let mut front_lines: Vec<&str> = Vec::new();
        let mut offset = 0usize;
        // Byte offset of the first line after the opening fence.
        offset += text.lines().next().map(|line| line.len() + 1).unwrap_or(0);
        for line in lines.by_ref() {
            let len = line.len() + 1;
            if line.trim_end() == "---" {
                body_start = Some(offset + len);
                break;
            }
            front_lines.push(line);
            offset += len;
        }
        if let Some(start) = body_start {
            for line in front_lines {
                if let Some((key, value)) = line.split_once(':') {
                    let key = key.trim().to_owned();
                    let mut value = value.trim().to_owned();
                    let quoted = value.len() >= 2
                        && ((value.starts_with('"') && value.ends_with('"'))
                            || (value.starts_with('\'') && value.ends_with('\'')));
                    if quoted {
                        value = value[1..value.len() - 1].to_owned();
                    }
                    if !key.is_empty() && !key.contains(' ') && !value.is_empty() {
                        map.insert(key, value);
                    }
                }
            }
            return (map, text[start.min(text.len())..].to_owned());
        }
    }
    (map, text.to_owned())
}

/// Whether an mcp env value must be treated as a secret: a sensitive key name, or a
/// secret-looking value. The value itself is never copied into the report.
fn looks_like_secret_value(key: &str, value: &str) -> bool {
    const SENSITIVE: [&str; 7] = [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
    ];
    if SENSITIVE.contains(&key.to_ascii_lowercase().as_str()) {
        return true;
    }
    const PREFIXES: [&str; 6] = ["sk-", "sk_", "xoxb-", "ghp_", "AKIA", "Bearer "];
    if PREFIXES.iter().any(|prefix| value.starts_with(prefix)) {
        return true;
    }
    value.len() >= 40
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The environment variable a literal secret should move to, e.g. `fs_API_KEY` →
/// `FS_API_KEY`.
fn suggested_env(base: &str) -> String {
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "CLAUDE_API_KEY".to_owned()
    } else if sanitized.ends_with("_KEY")
        || sanitized.ends_with("_TOKEN")
        || sanitized.ends_with("_SECRET")
    {
        sanitized.to_owned()
    } else {
        format!("{sanitized}_KEY")
    }
}

/// Validates one generated skill with the skill rules (name, description, closed
/// frontmatter). `Ok` means the skill discovery would load it.
fn validate_skill(name: &str, content: &str) -> Result<(), String> {
    if !crate::skills::valid_name(name) {
        return Err(format!("invalid skill name {name:?}"));
    }
    let (front, _) = split_frontmatter(content);
    match front.get("description") {
        Some(description) if crate::skills::valid_description(description.trim()) => Ok(()),
        _ => Err("missing or invalid `description`".into()),
    }
}

// ---------- the config draft ----------

/// One translated hook entry: the event, the optional Radian matcher, and the commands.
type HookEntry = (HookEvent, Option<String>, Vec<HookCommandDraft>);

/// One translated hook command with its optional timeout in seconds.
type HookCommandDraft = (String, Option<u64>);

#[derive(Default)]
struct Draft {
    extra_paths: Vec<String>,
    hooks: Vec<HookEntry>,
    user_agents_md: Option<String>,
    user_skills: Vec<ClaudeSkill>,
    project_skills: Vec<ClaudeSkill>,
    notes: Vec<String>,
    files_read: Vec<PathBuf>,
}

impl Draft {
    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

impl Draft {
    /// The config draft as TOML. Built through the `toml` serializer so every value is
    /// escaped correctly, and only keys the reader implements are emitted. Validated with
    /// the real reader before it is returned.
    fn render(&self) -> Result<String, ClaudeMigrateError> {
        let mut root = toml::map::Map::new();
        root.insert(
            "schema_version".into(),
            Toml::Integer(super::SUPPORTED_SCHEMA_VERSION as i64),
        );
        if !self.extra_paths.is_empty() {
            let mut skills = toml::map::Map::new();
            skills.insert(
                "extra_paths".into(),
                Toml::Array(self.extra_paths.iter().cloned().map(Toml::String).collect()),
            );
            root.insert("skills".into(), Toml::Table(skills));
        }
        if !self.hooks.is_empty() {
            let mut hooks = toml::map::Map::new();
            for (event, matcher, commands) in &self.hooks {
                let key = match event {
                    HookEvent::Pre => "PreToolUse",
                    HookEvent::Post => "PostToolUse",
                    HookEvent::Stop => "Stop",
                };
                let mut entry = toml::map::Map::new();
                if let Some(matcher) = matcher {
                    entry.insert("matcher".into(), Toml::String(matcher.clone()));
                }
                let mut list = Vec::new();
                for (command, timeout) in commands {
                    let mut hook = toml::map::Map::new();
                    hook.insert("type".into(), Toml::String("command".into()));
                    hook.insert("command".into(), Toml::String(command.clone()));
                    if let Some(secs) = timeout {
                        hook.insert("timeout".into(), Toml::Integer(*secs as i64));
                    }
                    list.push(Toml::Table(hook));
                }
                entry.insert("hooks".into(), Toml::Array(list));
                hooks
                    .entry(key.to_owned())
                    .or_insert_with(|| Toml::Array(Vec::new()))
                    .as_array_mut()
                    .expect("hooks event is an array")
                    .push(Toml::Table(entry));
            }
            root.insert("hooks".into(), Toml::Table(hooks));
        }
        let body = toml::to_string(&Toml::Table(root)).expect("the draft is serializable");
        let text = format!(
            "# A Radian configuration drafted from Claude Code files by\n\
             # `radian config import-claude`. Radian does not promise compatibility with \
             Claude's\n\
             # file format: review this file, and read the notes the command printed on \
             stderr for\n\
             # everything that could not be mapped.\n\
             {body}"
        );
        super::parse(Path::new("<draft>"), &text).map_err(|error| {
            ClaudeMigrateError::InvalidDraft {
                message: error.to_string(),
            }
        })?;
        Ok(text)
    }
}

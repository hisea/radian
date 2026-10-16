//! Agent definitions (ADR 0030 O11, slice 7 part a): the markdown file, its frontmatter parser,
//! the lookup and the snapshot a `spawn_task { agent }` call resolves.
//!
//! An agent definition is one markdown file, frontmatter plus body: the body is ADR 0036's
//! layer 1 (the role prompt), and the frontmatter maps to `SessionSetup`:
//!
//! ```markdown
//! ---
//! name: writer
//! description: writes docs; never touches code
//! model: anthropic/claude-sonnet-5-5
//! thinking: low
//! profile: safe
//! tools: [read, list, patch, git_status]
//! mcp: []
//! isolation: worktree
//! skills: [changelog]
//! spawn: false
//! ---
//! You are ...
//! ```
//!
//! **Where definitions are read (revision 2 item 4, part a):** only the user level,
//! `~/.radian/agents/<name>.md` (`$RADIAN_HOME/agents/<name>.md`). The project level
//! (`<workspace>/.radian/agents/<name>.md`) is an ADR 0031 gated resource, and `trust.rs` has
//! **no** per-resource gate for agent files yet, so project-level definitions stay unread in
//! this slice rather than inventing a trust rule (see the slice's "to decide" list). Because a
//! task never reads its own worktree's project resources, the lead's workspace and the task's
//! worktree are both read the same way: not at all.
//!
//! **Built-ins.** `default`, `task` and `lead` are compiled in (ADR 0036); a file of
//! the same name overrides the built-in (its body, frontmatter and all). `load` falls back to
//! the built-in when no file exists; an explicit `agent` name with neither a file nor a built-in
//! is an error.
//!
//! The parser is deliberately small and strict: flat `key: value` lines, `[a, b]` inline lists,
//! `true`/`false`. The workspace has no YAML dependency, and ADR 0030 O11's shape is exactly this
//! flat frontmatter, so a YAML parser would be a dependency for no gain. Unknown keys are ignored
//! with a warning ([`AgentDef::unknown_fields`]); a malformed line is an error naming the file and
//! the line.

use std::path::{Path, PathBuf};

use radian_permissions::{ApprovalPolicy, Preset};
use radian_runtime::ModelRoute;

use crate::task::Isolation;

/// Where an [`AgentDef`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    /// A user-level file: `~/.radian/agents/<name>.md`.
    File(PathBuf),
    /// A compiled-in role prompt (`default`, `task`, `lead`).
    Builtin,
}

/// One agent definition: the frontmatter fields plus the body (ADR 0036 layer 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// The definition name; it must equal the file stem.
    pub name: String,
    pub description: Option<String>,
    /// The `provider/model` route as written (ADR 0026); resolved by the host that builds the
    /// session. `None` means the host's own default.
    pub model: Option<String>,
    /// The ADR 0026 D11 thinking level as written (`off`, `low`, `medium`, `high`, …).
    pub thinking: Option<String>,
    /// The tier name as written (`safe`, `workspace`, `full`).
    pub profile: Option<String>,
    /// The built-in tool allow-list (ADR 0030 O11); `None` means every mounted built-in.
    pub tools: Option<Vec<String>>,
    /// The MCP server names the definition opts into (ADR 0030 O8/O11); empty means none.
    pub mcp: Vec<String>,
    /// `worktree` or `shared` (ADR 0030 O3); `None` means the task default.
    pub isolation: Option<String>,
    /// The skill allow-list (ADR 0025 / ADR 0030 O11). `None` loads every discovered skill
    /// (today's behaviour); `Some` keeps only the named ones, and a name discovery did not find
    /// is a spawn-time warning, never a failure.
    pub skills: Option<Vec<String>>,
    /// `true` for a lead definition (ADR 0030 O13). A task ignores it (depth limit 2).
    pub spawn: bool,
    /// `approval: on-ask` (ADR 0030 O2 layer 3); `None` means the default.
    pub approval: Option<String>,
    /// The body: ADR 0036 layer 1, the definition's system prompt.
    pub body: String,
    pub source: AgentSource,
    /// Frontmatter keys this slice does not know; ignored, with a warning (ADR 0030 O11).
    pub unknown_fields: Vec<String>,
}

impl AgentDef {
    /// The file this definition was read from, when it was a file.
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            AgentSource::File(path) => Some(path),
            AgentSource::Builtin => None,
        }
    }

    /// A warning line for every unknown field, or an empty list.
    pub fn warnings(&self) -> Vec<String> {
        self.unknown_fields
            .iter()
            .map(|key| match self.path() {
                Some(path) => format!("{}: unknown agent field `{key}` (ignored)", path.display()),
                None => format!("unknown agent field `{key}` (ignored)"),
            })
            .collect()
    }
}

/// Why a definition could not be read or resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    pub path: Option<PathBuf>,
    pub line: Option<usize>,
    pub message: String,
}

impl AgentError {
    fn new(path: Option<&Path>, line: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            path: path.map(Path::to_path_buf),
            line,
            message: message.into(),
        }
    }

    /// Attaches a file path when the error does not already carry one (a field value like a
    /// bad `model:` is validated in [`parse`], so it already names the file and line; a route
    /// re-parsed in [`resolve`] does not).
    fn or_path(mut self, path: Option<&Path>) -> Self {
        if self.path.is_none() {
            self.path = path.map(Path::to_path_buf);
        }
        self
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.path, self.line) {
            (Some(path), Some(line)) => write!(f, "{}:{line}: {}", path.display(), self.message),
            (Some(path), None) => write!(f, "{}: {}", path.display(), self.message),
            _ => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for AgentError {}

/// The keys ADR 0030 O11 names, plus `approval` (ADR 0030 O2 layer 3).
const KNOWN_KEYS: [&str; 11] = [
    "name",
    "description",
    "model",
    "thinking",
    "profile",
    "tools",
    "mcp",
    "isolation",
    "skills",
    "spawn",
    "approval",
];

/// Parses `text` as an agent definition. `path` names the file for errors, and `expected_name`
/// (the file stem) must match a frontmatter `name:` when one is present.
pub fn parse(path: &Path, expected_name: &str, text: &str) -> Result<AgentDef, AgentError> {
    // Split on '\n' (stripping a trailing '\r' per line), so line numbers are 1-based and the
    // body can be reconstructed with '\n'. This normalizes CRLF to LF.
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    let (frontmatter, body) = if lines.first().is_some_and(|line| line.trim_end() == "---") {
        let close = lines
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, line)| line.trim_end() == "---")
            .map(|(index, _)| index)
            .ok_or_else(|| {
                AgentError::new(
                    Some(path),
                    Some(1),
                    "unterminated frontmatter: no closing `---`",
                )
            })?;
        (&lines[1..close], lines[close + 1..].join("\n"))
    } else {
        (&lines[..0], text.to_owned())
    };

    let mut name: Option<String> = None;
    let mut description = None;
    let mut model = None;
    let mut thinking = None;
    let mut profile = None;
    let mut tools = None;
    let mut mcp: Vec<String> = Vec::new();
    let mut isolation = None;
    let mut skills: Option<Vec<String>> = None;
    let mut approval = None;
    let mut spawn = false;
    let mut unknown_fields: Vec<String> = Vec::new();

    for (offset, raw) in frontmatter.iter().enumerate() {
        let line = offset + 2; // 1-based, and the opening `---` is line 1.
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(AgentError::new(
                Some(path),
                Some(line),
                format!("expected `key: value`, found `{trimmed}`"),
            ));
        };
        let key = key.trim();
        let value = value.trim();
        if !KNOWN_KEYS.contains(&key) {
            unknown_fields.push(key.to_owned());
            continue;
        }
        match key {
            "name" => {
                if value != expected_name {
                    return Err(AgentError::new(
                        Some(path),
                        Some(line),
                        format!("name `{value}` does not match file name `{expected_name}`"),
                    ));
                }
                name = Some(value.to_owned());
            }
            "description" => description = Some(value.to_owned()),
            "thinking" => thinking = Some(value.to_owned()),
            "spawn" => {
                spawn = parse_bool(value)
                    .map_err(|message| AgentError::new(Some(path), Some(line), message))?;
            }
            "model" => {
                let route = parse_model_route(value)
                    .map_err(|error| AgentError::new(Some(path), Some(line), error.message))?;
                model = Some(format!("{}/{}", route.provider, route.model));
            }
            "profile" => {
                parse_tier(value)
                    .map_err(|error| AgentError::new(Some(path), Some(line), error.message))?;
                profile = Some(value.to_owned());
            }
            "isolation" => {
                parse_isolation(value)
                    .map_err(|error| AgentError::new(Some(path), Some(line), error.message))?;
                isolation = Some(value.to_owned());
            }
            "approval" => {
                if ApprovalPolicy::parse(value).is_none() {
                    return Err(AgentError::new(
                        Some(path),
                        Some(line),
                        format!("approval `{value}` is not one of on-ask, never, always, auto"),
                    ));
                }
                approval = Some(value.to_owned());
            }
            "tools" => {
                tools = Some(
                    parse_list(value)
                        .map_err(|message| AgentError::new(Some(path), Some(line), message))?,
                );
            }
            "mcp" => {
                mcp = parse_list(value)
                    .map_err(|message| AgentError::new(Some(path), Some(line), message))?;
            }
            "skills" => {
                skills = Some(
                    parse_list(value)
                        .map_err(|message| AgentError::new(Some(path), Some(line), message))?,
                );
            }
            _ => unreachable!("every known key is matched above"),
        }
    }

    Ok(AgentDef {
        name: name.unwrap_or_else(|| expected_name.to_owned()),
        description,
        model,
        thinking,
        profile,
        tools,
        mcp,
        isolation,
        skills,
        spawn,
        approval,
        body: strip_leading_blank_lines(&body),
        source: AgentSource::File(path.to_path_buf()),
        unknown_fields,
    })
}

/// Reads and parses one definition file. The expected name is the file stem.
pub fn parse_file(path: &Path) -> Result<AgentDef, AgentError> {
    let expected = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| AgentError::new(Some(path), None, "the file name is not valid UTF-8"))?;
    let text = std::fs::read_to_string(path)
        .map_err(|error| AgentError::new(Some(path), None, format!("cannot read: {error}")))?;
    parse(path, expected, &text)
}

/// The built-in definition of `default`/`task`/`lead`, or `None` for any other name. The body
/// is the compiled-in role prompt (ADR 0036); the built-ins carry no frontmatter.
pub fn builtin_def(name: &str) -> Option<AgentDef> {
    crate::prompt::builtin_role(name).map(|body| AgentDef {
        name: name.to_owned(),
        description: None,
        model: None,
        thinking: None,
        profile: None,
        tools: None,
        mcp: Vec::new(),
        isolation: None,
        skills: None,
        spawn: false,
        approval: None,
        body: body.to_owned(),
        source: AgentSource::Builtin,
        unknown_fields: Vec::new(),
    })
}

/// Loads the definition `name`. `project` is the **lead's** `<workspace>/.radian/agents` when the
/// workspace is trusted (ADR 0030 O11 / ADR 0031 T1); a project file of that name wins over the
/// user-level file, which wins over the built-in. A task never passes its own worktree here — the
/// registry snapshots the lead's directory (ADR 0030 O7 rev 2 item 4). An unknown name with no
/// file and no built-in is an error (an explicit `agent` must name something real).
pub fn load(
    name: &str,
    radian_home: Option<&Path>,
    project: Option<&Path>,
) -> Result<AgentDef, AgentError> {
    // Project first: the project customises the same name for its own runs.
    if let Some(dir) = project {
        let path = dir.join(format!("{name}.md"));
        if path.is_file() {
            return load_file(&path, name);
        }
    }
    if let Some(home) = radian_home {
        let path = home.join("agents").join(format!("{name}.md"));
        if path.is_file() {
            return load_file(&path, name);
        }
    }
    builtin_def(name)
        .ok_or_else(|| AgentError::new(None, None, format!("no agent definition `{name}`")))
}

/// Parses one definition file and applies the "frontmatter-only file keeps the built-in body" rule.
fn load_file(path: &Path, name: &str) -> Result<AgentDef, AgentError> {
    let mut def = parse_file(path)?;
    // Unknown frontmatter keys are ignored, with a warning (ADR 0030 O11).
    for warning in def.warnings() {
        eprintln!("radian: {warning}");
    }
    // A file that is only frontmatter has no body; fall back to the built-in body of the same
    // name when there is one, so an override cannot accidentally blank layer 1.
    if def.body.trim().is_empty()
        && let Some(builtin) = crate::prompt::builtin_role(name)
    {
        def.body = builtin.to_owned();
    }
    Ok(def)
}

/// Splits a `provider/model` route (the same shape `/model` accepts, ADR 0026).
pub fn parse_model_route(value: &str) -> Result<ModelRoute, AgentError> {
    let Some((provider, model)) = value.split_once('/') else {
        return Err(AgentError::new(
            None,
            None,
            format!("model `{value}` is not `provider/model`"),
        ));
    };
    let (provider, model) = (provider.trim(), model.trim());
    if provider.is_empty() || model.is_empty() {
        return Err(AgentError::new(
            None,
            None,
            format!("model `{value}` is not `provider/model`"),
        ));
    }
    Ok(ModelRoute::new(provider, model))
}

/// Parses a tier name (`safe`, `workspace`, `full`, plus the CLI aliases `workspace-write` and
/// `full-access`).
pub fn parse_tier(value: &str) -> Result<Preset, AgentError> {
    match value {
        "safe" => Ok(Preset::Safe),
        "workspace" | "workspace-write" => Ok(Preset::WorkspaceWrite),
        "full" | "full-access" => Ok(Preset::FullAccess),
        other => Err(AgentError::new(
            None,
            None,
            format!("profile `{other}` is not one of safe, workspace, full"),
        )),
    }
}

fn parse_isolation(value: &str) -> Result<Isolation, AgentError> {
    match value {
        "worktree" => Ok(Isolation::Worktree),
        "shared" => Ok(Isolation::Shared),
        other => Err(AgentError::new(
            None,
            None,
            format!("isolation `{other}` is not one of worktree, shared"),
        )),
    }
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected `true` or `false`, found `{other}`")),
    }
}

/// Parses an inline `[a, b]` list. Surrounding single or double quotes on an item are stripped.
fn parse_list(value: &str) -> Result<Vec<String>, String> {
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return Err(format!("expected a list `[a, b]`, found `{value}`"));
    };
    let mut out = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let item = item
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                item.strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            })
            .unwrap_or(item);
        out.push(item.to_owned());
    }
    Ok(out)
}

fn strip_leading_blank_lines(body: &str) -> String {
    body.trim_start_matches('\n').to_owned()
}

// ---------------------------------------------------------------------------
// Resolution: the snapshot a `spawn_task` resolves
// ---------------------------------------------------------------------------

/// The `spawn_task` arguments that override a definition (ADR 0030 O4): the spawn argument wins
/// over the definition's field.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnOverrides<'a> {
    /// The definition name; `None` means the task default (the `task` role).
    pub agent: Option<&'a str>,
    pub isolation: Option<Isolation>,
    /// A `provider/model` route; wins over the definition's `model`.
    pub model: Option<&'a str>,
    /// A tier name; wins over the definition's `profile`.
    pub profile: Option<&'a str>,
    /// The approval policy; wins over the definition's `approval`.
    pub approval: Option<ApprovalPolicy>,
}

/// The definition resolved for one task, snapshotted at spawn (ADR 0030 item 4): the
/// task never re-reads the file, and never reads its own worktree's `.radian/agents`.
///
/// The registry hands this to the [`crate::task::TaskBuilder`] (which builds the model from
/// [`SpawnAgent::model`] and the thinking level) and enforces the narrowing rules on the built
/// setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnAgent {
    pub name: String,
    /// Layer 1 for the task session (ADR 0036).
    pub body: String,
    pub source: AgentSource,
    pub description: Option<String>,
    pub thinking: Option<String>,
    /// The resolved route (spawn argument wins over the definition).
    pub model: Option<ModelRoute>,
    /// The resolved tier (spawn argument wins over the definition); already checked to narrow.
    pub profile: Option<Preset>,
    /// The built-in tool allow-list; `None` means every mounted built-in.
    pub tools: Option<Vec<String>>,
    /// The MCP server names the definition opts into; empty means none (ADR 0030 O8).
    pub mcp: Vec<String>,
    /// The skill allow-list (ADR 0025 / ADR 0030 O11); `None` means every discovered skill.
    pub skills: Option<Vec<String>>,
    /// The resolved isolation (spawn argument wins over the definition).
    pub isolation: Option<Isolation>,
    /// The resolved approval (spawn argument wins over the definition).
    pub approval: Option<ApprovalPolicy>,
    /// Whether the definition asked for `spawn: true`.
    pub spawn: bool,
    /// Whether `spawn: true` was ignored because this is a task (depth limit 2, ADR 0030 O13).
    pub spawn_ignored: bool,
    /// The definition's unknown frontmatter keys (ignored).
    pub unknown_fields: Vec<String>,
}

/// The `[orchestration]`-level spawn defaults (ADR 0030 O7): the lowest precedence, below the
/// spawn arguments and the agent definition. Resolved once, from the lead's workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnDefaults {
    pub isolation: Option<Isolation>,
    /// A `provider/model` route as written; parsed when it is used.
    pub model: Option<String>,
    pub profile: Option<Preset>,
}

impl SpawnDefaults {
    /// Parses the raw config strings, pushing a warning and dropping a value this build cannot
    /// use (a bad isolation, or a `full` profile that would widen the task floor).
    pub fn from_raw(
        isolation: Option<&str>,
        model: Option<&str>,
        profile: Option<&str>,
        warnings: &mut Vec<String>,
    ) -> Self {
        let isolation = isolation.and_then(|raw| match parse_isolation(raw) {
            Ok(isolation) => Some(isolation),
            Err(error) => {
                warnings.push(format!("orchestration.isolation: {}", error.message));
                None
            }
        });
        let profile = profile.and_then(|raw| match parse_tier(raw) {
            Ok(Preset::FullAccess) => {
                warnings.push(
                    "orchestration.profile: a task may only narrow the `workspace` tier; \
                     `full` is refused"
                        .to_owned(),
                );
                None
            }
            Ok(profile) => Some(profile),
            Err(error) => {
                warnings.push(format!("orchestration.profile: {}", error.message));
                None
            }
        });
        Self {
            isolation,
            model: model.map(str::to_owned),
            profile,
        }
    }
}

/// Resolves the definition for a task (ADR 0030 O11). `project` is the lead's trusted
/// `<workspace>/.radian/agents`, or `None` (ADR 0031 T1). The spawn arguments override the
/// definition's fields, which override `defaults` (ADR 0030 O7's orchestration defaults).
pub fn resolve(
    radian_home: Option<&Path>,
    project: Option<&Path>,
    overrides: &SpawnOverrides<'_>,
    defaults: &SpawnDefaults,
) -> Result<SpawnAgent, AgentError> {
    let def = match overrides.agent {
        Some(name) => load(name, radian_home, project)?,
        None => load("task", radian_home, project)?,
    };
    let def_path = def.path();

    let model = match overrides.model {
        Some(raw) => Some(parse_model_route(raw)?),
        None => match def.model.as_deref() {
            Some(raw) => Some(parse_model_route(raw).map_err(|e| e.or_path(def_path))?),
            None => match defaults.model.as_deref() {
                Some(raw) => Some(parse_model_route(raw)?),
                None => None,
            },
        },
    };
    let profile = match overrides.profile {
        Some(raw) => Some(parse_tier(raw)?),
        None => match def.profile.as_deref() {
            Some(raw) => Some(parse_tier(raw).map_err(|e| e.or_path(def_path))?),
            None => defaults.profile,
        },
    };
    // A task can only be narrower than the registry's `workspace` floor (ADR 0030 O3). A file or
    // argument that widens is refused, not clamped: silently weakening the task boundary would
    // defeat the point of the floor. (`SpawnDefaults::from_raw` already dropped a `full` default.)
    if profile == Some(Preset::FullAccess) {
        return Err(AgentError::new(
            def_path,
            None,
            "a task's profile may only narrow the `workspace` tier; `full` is refused",
        ));
    }
    let isolation = match overrides.isolation {
        Some(isolation) => Some(isolation),
        None => match def
            .isolation
            .as_deref()
            .map(|raw| parse_isolation(raw).map_err(|e| e.or_path(def_path)))
            .transpose()?
        {
            Some(isolation) => Some(isolation),
            None => defaults.isolation,
        },
    };
    let approval = match overrides.approval {
        Some(approval) => Some(approval),
        None => def
            .approval
            .as_deref()
            .map(|raw| {
                ApprovalPolicy::parse(raw)
                    .ok_or_else(|| AgentError::new(def_path, None, format!("approval `{raw}`")))
            })
            .transpose()?,
    };

    let spawn = def.spawn;
    Ok(SpawnAgent {
        name: def.name,
        body: def.body,
        source: def.source,
        description: def.description,
        thinking: def.thinking,
        model,
        profile,
        tools: def.tools,
        mcp: def.mcp,
        skills: def.skills,
        isolation,
        approval,
        spawn,
        // ADR 0030 O13: a task is depth 2 and an agent definition's `spawn: true` does not take
        // effect; it is recorded so the result can say so.
        spawn_ignored: spawn,
        unknown_fields: def.unknown_fields,
    })
}

// ---------------------------------------------------------------------------
// Resolution: the snapshot a named agent's first `send` resolves (ADR 0030 O12)
// ---------------------------------------------------------------------------

/// The definition resolved for one **named agent** (ADR 0030 O12): one long session per name,
/// created from the definition on first use and reused after. Unlike a task (ADR 0030 O3), a
/// named agent is not narrowed to the `workspace` floor: it is commanded by the human, so a
/// `full` profile is allowed, and `spawn: true` is **not** ignored (a named lead is the ADR's
/// lead, O13). The snapshot is taken once, at creation, so later edits to the file never move a
/// live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedAgent {
    pub name: String,
    /// Layer 1 for the named session (ADR 0036).
    pub body: String,
    pub source: AgentSource,
    pub description: Option<String>,
    pub thinking: Option<String>,
    /// The resolved route; `None` means the host's own default.
    pub model: Option<ModelRoute>,
    /// The tier the definition names (`safe`, `workspace`, `full`); `None` means the host's
    /// default. A named agent may name any tier — it is the human's own session.
    pub profile: Option<Preset>,
    pub tools: Option<Vec<String>>,
    pub mcp: Vec<String>,
    pub skills: Option<Vec<String>>,
    /// The isolation (ADR 0030 O3); `None` means the `worktree` default.
    pub isolation: Option<Isolation>,
    /// `true` for a lead definition (ADR 0030 O13): the named session mounts the lead tools.
    pub spawn: bool,
    /// The definition's unknown frontmatter keys (ignored).
    pub unknown_fields: Vec<String>,
}

/// Resolves a named agent's definition from the **lead's** definition dirs (ADR 0030 O7 rev 2 item
/// 4): the lead's trusted project dir `project` when the workspace is trusted, else the user-level
/// dir. The name is the file stem; `load` falls back to the built-in `default`/`task`/`lead` of
/// that name. A named agent never reads its own worktree's `.radian/agents` — its definition is
/// snapshotted from the lead's workspace at first use (ADR 0031 T1).
pub fn resolve_named(
    name: &str,
    radian_home: Option<&Path>,
    project: Option<&Path>,
) -> Result<NamedAgent, AgentError> {
    let def = load(name, radian_home, project)?;
    let def_path = def.path();
    let model = match def.model.as_deref() {
        Some(raw) => Some(parse_model_route(raw).map_err(|e| e.or_path(def_path))?),
        None => None,
    };
    let profile = match def.profile.as_deref() {
        Some(raw) => Some(parse_tier(raw).map_err(|e| e.or_path(def_path))?),
        None => None,
    };
    let isolation = match def.isolation.as_deref() {
        Some(raw) => Some(parse_isolation(raw).map_err(|e| e.or_path(def_path))?),
        None => None,
    };
    Ok(NamedAgent {
        name: def.name,
        body: def.body,
        source: def.source,
        description: def.description,
        thinking: def.thinking,
        model,
        profile,
        tools: def.tools,
        mcp: def.mcp,
        skills: def.skills,
        isolation,
        spawn: def.spawn,
        unknown_fields: def.unknown_fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(label: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("radian-agents-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        let path = dir.join("agents").join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn parses_every_field_and_the_body() {
        let home = scratch("all");
        let path = write(
            &home,
            "writer.md",
            "---\n\
             name: writer\n\
             description: writes docs\n\
             model: anthropic/claude-sonnet-5-5\n\
             thinking: low\n\
             profile: safe\n\
             tools: [read, list, patch, git_status]\n\
             mcp: []\n\
             isolation: worktree\n\
             skills: [changelog]\n\
             spawn: true\n\
             approval: on-ask\n\
             ---\n\
             You are a writer.\n",
        );
        let def = parse_file(&path).unwrap();
        assert_eq!(def.name, "writer");
        assert_eq!(def.description.as_deref(), Some("writes docs"));
        assert_eq!(def.model.as_deref(), Some("anthropic/claude-sonnet-5-5"));
        assert_eq!(def.thinking.as_deref(), Some("low"));
        assert_eq!(def.profile.as_deref(), Some("safe"));
        assert_eq!(
            def.tools.as_deref(),
            Some(
                ["read", "list", "patch", "git_status"]
                    .map(str::to_owned)
                    .as_slice()
            )
        );
        assert!(def.mcp.is_empty());
        assert_eq!(def.isolation.as_deref(), Some("worktree"));
        assert_eq!(def.skills, Some(vec!["changelog".to_owned()]));
        assert!(def.spawn);
        assert_eq!(def.approval.as_deref(), Some("on-ask"));
        assert_eq!(def.body, "You are a writer.\n");
        assert_eq!(def.path(), Some(path.as_path()));
    }

    #[test]
    fn an_unknown_field_is_ignored_with_a_warning() {
        let home = scratch("unknown");
        let path = write(&home, "x.md", "---\nname: x\nteam: blue\n---\nbody\n");
        let def = parse_file(&path).unwrap();
        assert_eq!(def.unknown_fields, ["team"]);
        assert!(!def.warnings().is_empty());
        assert!(def.warnings()[0].contains("team"));
    }

    #[test]
    fn a_malformed_line_is_an_error_naming_the_file_and_line() {
        let home = scratch("malformed");
        let path = write(
            &home,
            "x.md",
            "---\nname: x\nthis is not a pair\n---\nbody\n",
        );
        let error = parse_file(&path).unwrap_err();
        assert_eq!(error.path.as_deref(), Some(path.as_path()));
        assert_eq!(error.line, Some(3));
        assert!(error.to_string().contains(":3:"), "{error}");
    }

    #[test]
    fn an_unterminated_frontmatter_is_an_error() {
        let home = scratch("unterminated");
        let path = write(&home, "x.md", "---\nname: x\nbody\n");
        let error = parse_file(&path).unwrap_err();
        assert_eq!(error.line, Some(1));
        assert!(error.to_string().contains("unterminated"), "{error}");
    }

    #[test]
    fn a_name_that_does_not_match_the_file_stem_is_an_error() {
        let home = scratch("mismatch");
        let path = write(&home, "x.md", "---\nname: y\n---\nbody\n");
        let error = parse_file(&path).unwrap_err();
        assert_eq!(error.line, Some(2));
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_bad_field_value_is_an_error_naming_the_line() {
        let home = scratch("badfield");
        for (text, expected) in [
            ("---\nspawn: maybe\n---\n\n", "true"),
            ("---\nmodel: nope\n---\n\n", "provider/model"),
            ("---\nprofile: huge\n---\n\n", "safe"),
            ("---\nisolation: nowhere\n---\n\n", "worktree"),
            ("---\ntools: read\n---\n\n", "list"),
        ] {
            let path = write(&home, "x.md", text);
            let error = parse_file(&path).unwrap_err();
            assert_eq!(error.line, Some(2), "{error}");
            assert!(error.message.contains(expected), "{error}");
        }
    }

    #[test]
    fn no_frontmatter_means_the_whole_text_is_the_body() {
        let home = scratch("nofm");
        let path = write(&home, "x.md", "You are x.\n");
        let def = parse_file(&path).unwrap();
        assert!(def.model.is_none());
        assert_eq!(def.body, "You are x.\n");
    }

    #[test]
    fn a_user_file_overrides_the_builtin_body() {
        let home = scratch("override");
        write(&home, "task.md", "---\nname: task\n---\nMY TASK\n");
        let def = load("task", Some(&home), None).unwrap();
        assert_eq!(def.body, "MY TASK\n");
        // A name with neither a file nor a built-in is an error.
        assert!(load("nope", Some(&home), None).is_err());
        // A name with only a built-in falls back to it.
        assert_eq!(
            load("default", Some(&home), None).unwrap().body,
            crate::prompt::DEFAULT_ROLE
        );
    }

    #[test]
    fn resolve_snapshots_the_file_and_lets_overrides_win() {
        let home = scratch("resolve");
        write(
            &home,
            "writer.md",
            "---\nname: writer\nmodel: p/one\nprofile: safe\nisolation: shared\nspawn: true\n---\nBODY\n",
        );
        let overrides = SpawnOverrides {
            agent: Some("writer"),
            ..SpawnOverrides::default()
        };
        let agent = resolve(Some(&home), None, &overrides, &SpawnDefaults::default()).unwrap();
        assert_eq!(agent.name, "writer");
        assert_eq!(agent.body, "BODY\n");
        assert_eq!(agent.model, Some(ModelRoute::new("p", "one")));
        assert_eq!(agent.profile, Some(Preset::Safe));
        assert_eq!(agent.isolation, Some(Isolation::Shared));
        assert!(agent.spawn);
        assert!(agent.spawn_ignored);

        // The spawn arguments win over the file.
        let overrides = SpawnOverrides {
            agent: Some("writer"),
            model: Some("q/two"),
            profile: Some("workspace"),
            isolation: Some(Isolation::Worktree),
            ..SpawnOverrides::default()
        };
        let agent = resolve(Some(&home), None, &overrides, &SpawnDefaults::default()).unwrap();
        assert_eq!(agent.model, Some(ModelRoute::new("q", "two")));
        assert_eq!(agent.profile, Some(Preset::WorkspaceWrite));
        assert_eq!(agent.isolation, Some(Isolation::Worktree));
    }

    #[test]
    fn resolve_defaults_to_the_task_role() {
        let home = scratch("default");
        let agent = resolve(
            Some(&home),
            None,
            &SpawnOverrides::default(),
            &SpawnDefaults::default(),
        )
        .unwrap();
        assert_eq!(agent.name, "task");
        assert_eq!(agent.body, crate::prompt::TASK_ROLE);
        assert!(!agent.spawn_ignored);
    }

    #[test]
    fn resolve_refuses_a_widening_profile() {
        let home = scratch("widen");
        write(&home, "x.md", "---\nname: x\nprofile: full\n---\nB\n");
        let overrides = SpawnOverrides {
            agent: Some("x"),
            ..SpawnOverrides::default()
        };
        assert!(resolve(Some(&home), None, &overrides, &SpawnDefaults::default()).is_err());
        // And the spawn argument cannot widen either.
        let overrides = SpawnOverrides {
            profile: Some("full"),
            ..SpawnOverrides::default()
        };
        assert!(resolve(Some(&home), None, &overrides, &SpawnDefaults::default()).is_err());
    }
}

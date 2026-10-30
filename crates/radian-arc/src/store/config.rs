//! `<repository>/.radian/arc/config.toml` (ADR 0048 decision 15).
//!
//! The file is written once by `radian arc init` (a second `init` keeps it) and read by
//! every other subcommand. It has the shape of the radian configuration file (ADR 0008):
//! an explicit `schema_version`, rejects unknown keys, and every value set is one list the
//! parser and the manual's drift test both read.
//!
//! The `[gate]` table is [`crate::gate::GateConfig`]: the gate owns the profile types and the
//! one closed stage list, and this module only supplies the file's defaults and validation.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gate::{
    DEFAULT_COMMAND_TIMEOUT_SECS, GateConfig, Profile, ReviewConfig, Stage, StageSpec,
    default_before_attempt,
};
use crate::store::model::{PREFIX_MAX_CHARS, valid_prefix};

pub use crate::gate::STAGE_NAMES;

/// The configuration schema version this build writes and accepts.
pub const SCHEMA_VERSION: u32 = 1;

/// The flow a task with no `flow` field runs (`[flows.default]`).
pub const DEFAULT_FLOW: &str = "default";

/// The longest flow name, in characters.
pub const FLOW_NAME_MAX_CHARS: usize = 32;

/// The concurrency a flow has when its section leaves the key out.
pub const DEFAULT_FLOW_CONCURRENCY: u32 = 1;

/// A flow name: 1 to 32 characters from `[a-z0-9-]`, with no leading or trailing `-`.
pub fn valid_flow_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=FLOW_NAME_MAX_CHARS).contains(&count)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Keys whose default is "not set" rather than a value. The manual's table names each
/// one, and the drift test checks that it does.
pub const OPTIONAL_KEYS: &[&str] = &[
    "limits.max_cost_usd",
    "worker.command",
    "flows.<name>.acp",
    "flows.<name>.command",
    "gate.review.exec_args",
    "gate.review.prompt_file",
    "gate.review.first.exec_args",
    "gate.review.first.timeout_secs",
    "integration.target_branch",
];

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    #[serde(default)]
    pub repository: Repository,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub worker: Worker,
    #[serde(default)]
    pub flows: BTreeMap<String, Flow>,
    #[serde(default)]
    pub ids: Ids,
    #[serde(default = "default_gate")]
    pub gate: GateConfig,
    /// `[integration]`: how the merge puts the candidate on the target branch (ADR 0048
    /// decisions 6 and 10).
    #[serde(default)]
    pub integration: Integration,
    /// `[hooks]`: the commands arc runs at fixed points (ADR 0048 decisions 10 and 13).
    #[serde(default)]
    pub hooks: Hooks,
    /// Whether the file wrote a `[flows]` table. A file that did not keeps no per-flow
    /// concurrency for its implicit `default` flow, so it schedules as it did before flows
    /// existed (ADR 0048 decision 13). Set when the file is parsed, never read from it.
    #[serde(skip)]
    pub flows_configured: bool,
}

impl Default for Config {
    fn default() -> Self {
        let gate = default_gate();
        let worker = Worker::default();
        let flows = BTreeMap::from([(
            DEFAULT_FLOW.to_owned(),
            Flow::from_worker(&worker, &gate.profile),
        )]);
        Self {
            schema_version: SCHEMA_VERSION,
            repository: Repository::default(),
            limits: Limits::default(),
            worker,
            flows,
            ids: Ids::default(),
            gate,
            integration: Integration::default(),
            hooks: Hooks::default(),
            flows_configured: false,
        }
    }
}

impl Config {
    /// The concurrency this flow's own `doing` tasks are held to, or `None` when it has no
    /// per-flow limit. A file that writes no `[flows]` table has none: its implicit `default`
    /// flow's tasks count against [`Limits::concurrency`] alone, so it schedules as it did
    /// before flows existed. Once the file writes a `[flows]` table, a bare one included,
    /// every flow it holds is held to its `concurrency`, the implicit `default` flow included
    /// (ADR 0048 decision 13).
    pub fn flow_concurrency(&self, name: &str) -> Option<u32> {
        if self.flows_configured {
            self.flows.get(name).map(|flow| flow.concurrency)
        } else {
            None
        }
    }

    /// The value a task of `flow` uses when its own `allow_noop` is absent: the flow's
    /// `allow_noop`, or `false` for a flow the file does not define (ADR 0048 decisions 8 and
    /// 15). A task that names no flow uses the implicit `default` flow.
    pub fn flow_allow_noop(&self, flow: &str) -> bool {
        self.flows.get(flow).is_some_and(|flow| flow.allow_noop)
    }

    /// The gate profile a task of `flow` runs, and its name: the flow's `gate_profile`,
    /// resolved to `gate.profile` for a file without `[flows]` (ADR 0048 decisions 10 and 15).
    ///
    /// # Errors
    ///
    /// The file defines no flow `flow`, or its `gate_profile` names no
    /// `[gate.profiles.<name>]` section. The message names the task, the flow and the profile.
    pub fn task_gate_profile<'a>(
        &'a self,
        task: &str,
        flow: &str,
    ) -> Result<(&'a str, &'a Profile), String> {
        let Some(flow_config) = self.flows.get(flow) else {
            return Err(missing_flow_reason(task, flow));
        };
        let name = flow_config.gate_profile(&self.gate.profile);
        let Some(profile) = self.gate.profiles.get(name) else {
            return Err(format!(
                "task `{task}` uses flow `{flow}`, whose `flows.{flow}.gate_profile` is \
                 `{name}` but no such profile exists"
            ));
        };
        Ok((name, profile))
    }

    /// The branch integration fetches, builds the candidate on, pushes to, and the branch a
    /// first attempt starts from: `[integration] target_branch`, else `[repository]
    /// main_branch` (ADR 0048 decisions 6 and 10).
    pub fn target_branch(&self) -> &str {
        self.integration
            .target_branch
            .as_deref()
            .unwrap_or(&self.repository.main_branch)
    }
}

/// How the integration puts the candidate on the target branch (`[integration] method`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IntegrationMethod {
    /// Push the candidate to the target branch (the default).
    #[default]
    Push,
    /// Run `[integration] command` and let it merge.
    Command,
    /// The built-in `command` preset that opens a pull request (decided; not built yet).
    Pr,
}

/// Every value `integration.method` names, in one list: the parser and the manual's drift test
/// read it. [`IntegrationMethod::Push`] and [`IntegrationMethod::Command`] are built.
pub const INTEGRATION_METHODS: &[IntegrationMethod] = &[
    IntegrationMethod::Push,
    IntegrationMethod::Command,
    IntegrationMethod::Pr,
];

impl IntegrationMethod {
    /// The stored spelling: the TOML value and the word the manual uses.
    pub fn as_str(self) -> &'static str {
        match self {
            IntegrationMethod::Push => "push",
            IntegrationMethod::Command => "command",
            IntegrationMethod::Pr => "pr",
        }
    }

    /// The value `text` names, if any.
    pub fn parse(text: &str) -> Option<Self> {
        INTEGRATION_METHODS
            .iter()
            .copied()
            .find(|value| value.as_str() == text)
    }

    /// Whether this build runs the method.
    pub fn is_built(self) -> bool {
        matches!(self, IntegrationMethod::Push | IntegrationMethod::Command)
    }
}

impl std::fmt::Display for IntegrationMethod {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A value outside [`INTEGRATION_METHODS`] is refused by name when the file is read, and a
/// value the list holds but this build does not run yet is refused as not built.
impl<'de> Deserialize<'de> for IntegrationMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let value = Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`integration.method` is `{text}`; it must be one of {}",
                INTEGRATION_METHODS
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        if !value.is_built() {
            return Err(serde::de::Error::custom(format!(
                "`integration.method` is `{text}`; it is not built yet (this build runs `push` \
                 and `command`)"
            )));
        }
        Ok(value)
    }
}

/// How the integration builds the candidate commit (`[integration] style`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IntegrationStyle {
    /// A merge commit of the task's head onto the target branch (the default).
    #[default]
    Merge,
    /// One commit on top of the target branch with the task's changes applied.
    Squash,
    /// The task's commits replayed on the target branch.
    Rebase,
}

/// Every value `integration.style` names, in one list: the parser and the manual's drift test
/// read it.
pub const INTEGRATION_STYLES: &[IntegrationStyle] = &[
    IntegrationStyle::Merge,
    IntegrationStyle::Squash,
    IntegrationStyle::Rebase,
];

impl IntegrationStyle {
    /// The stored spelling: the TOML value and the word the manual uses.
    pub fn as_str(self) -> &'static str {
        match self {
            IntegrationStyle::Merge => "merge",
            IntegrationStyle::Squash => "squash",
            IntegrationStyle::Rebase => "rebase",
        }
    }

    /// The value `text` names, if any.
    pub fn parse(text: &str) -> Option<Self> {
        INTEGRATION_STYLES
            .iter()
            .copied()
            .find(|value| value.as_str() == text)
    }
}

impl std::fmt::Display for IntegrationStyle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A value outside [`INTEGRATION_STYLES`] is refused by name when the file is read.
impl<'de> Deserialize<'de> for IntegrationStyle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`integration.style` is `{text}`; it must be one of {}",
                INTEGRATION_STYLES
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

/// `[integration]`: the key commands and the branch the merge puts the candidate on (ADR 0048
/// decisions 6 and 10). `key_commands` lives here and nowhere else; a gate profile that still
/// writes it is a config error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Integration {
    /// Shell command lines the integration stage runs on the merge candidate, in order.
    pub key_commands: Vec<String>,
    /// The branch integration merges into and pushes to. `None` is `[repository]
    /// main_branch`.
    pub target_branch: Option<String>,
    /// How the candidate reaches the target branch: `push` or `command`.
    pub method: IntegrationMethod,
    /// The one shell command line `method = "command"` runs, in the repository root, to merge
    /// the candidate. Required by that method and refused with `push`.
    pub command: Option<String>,
    /// How the candidate is built: `merge`, `squash` or `rebase`.
    pub style: IntegrationStyle,
}

impl Default for Integration {
    fn default() -> Self {
        Self {
            key_commands: Vec::new(),
            target_branch: None,
            method: IntegrationMethod::Push,
            command: None,
            style: IntegrationStyle::Merge,
        }
    }
}

/// `[hooks]`: the commands arc runs at fixed points (ADR 0048 decisions 10 and 13). Each key
/// is a list of shell command lines; an unknown name is refused as an unknown key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Hooks {
    /// Shell command lines run in the repository root after a task reaches `done.merged`, in
    /// order. Empty runs nothing.
    pub after_merge: Vec<String>,
    /// Shell command lines run after a task enters any `failed.*` phase, in order.
    pub after_fail: Vec<String>,
    /// Shell command lines run after a task enters `review.rework_pending`, in order.
    pub after_rework: Vec<String>,
    /// Shell command lines run after a task enters `review.awaiting_human`, in order.
    pub after_awaiting_human: Vec<String>,
    /// Shell command lines run after a new task enters `todo.proposed`, in order.
    pub after_proposed: Vec<String>,
}

impl Hooks {
    /// The command lines of `point`, in order. An unknown point has none.
    #[must_use]
    pub fn commands(&self, point: &str) -> &[String] {
        self.value(point).unwrap_or(&[])
    }

    /// The value this file holds for `point`, when it holds one.
    fn value(&self, point: &str) -> Option<&[String]> {
        match point {
            "after_merge" => Some(&self.after_merge),
            "after_fail" => Some(&self.after_fail),
            "after_rework" => Some(&self.after_rework),
            "after_awaiting_human" => Some(&self.after_awaiting_human),
            "after_proposed" => Some(&self.after_proposed),
            _ => None,
        }
    }
}

/// `[repository]`: what `init` found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Repository {
    /// The git remote arc pushes task branches to and reads the main branch from.
    pub remote: String,
    /// The branch a task starts from and merges into.
    pub main_branch: String,
}

impl Default for Repository {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
            main_branch: "main".to_owned(),
        }
    }
}

/// `[limits]`: concurrency and budgets. A per-task field of the same name wins for that
/// task; a plan-level budget adds to the task budgets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// How many tasks may be in `doing` at once across every flow, next to each flow's own
    /// `concurrency` (ADR 0048 decision 13).
    pub concurrency: u32,
    /// How many retries one task gets since its last lead or review rework before it is
    /// `failed.exhausted`. A conflict rework keeps the count.
    pub max_attempts: u32,
    /// No new line on the worker's jsonl stream for this long is a stall.
    pub stall_timeout_secs: u64,
    /// An absolute limit on one attempt, on top of the stall timeout.
    pub max_wall_time_secs: u64,
    /// The limit of one git call that talks to a remote (`fetch`, `push`, `ls-remote`). Local
    /// git calls are not bounded.
    pub git_remote_timeout_secs: u64,
    /// No limit when unset.
    pub max_cost_usd: Option<f64>,
    /// How long a cancel waits for the worker before SIGKILL.
    pub cancel_grace_secs: u64,
    /// How many automatic reworks one task may get before the plan pauses.
    pub max_auto_rework: u32,
    /// How many conflicts with main one task may reach in a row before arc stops writing the
    /// conflict rework and waits for the lead. `0` writes no conflict rework.
    pub max_conflict_rework: u32,
    /// How many times a task a command failure blocked may be re-verified by itself once the
    /// remote main moved (ADR 0048 decision 9). `0` turns the step off.
    pub max_auto_reverify: u32,
    /// How many gate starts may die for the same attempt and inputs before the pass records
    /// `died` instead of starting the gate again, whether or not it could plan them (ADR 0048
    /// decision 10).
    pub max_gate_deaths: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 1,
            max_attempts: 3,
            stall_timeout_secs: 900,
            max_wall_time_secs: 7200,
            git_remote_timeout_secs: crate::git::DEFAULT_REMOTE_TIMEOUT_SECS,
            max_cost_usd: None,
            cancel_grace_secs: 10,
            max_auto_rework: 2,
            max_conflict_rework: 3,
            max_auto_reverify: 2,
            max_gate_deaths: 2,
        }
    }
}

/// How a later attempt of a task gets its worker session (`[worker] rework_session`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReworkSession {
    /// Every attempt has a session of its own, `<task>-<attempt>`; nothing is resumed.
    #[default]
    Fresh,
    /// One session per task, the task id; a later attempt resumes it.
    Resume,
}

/// The values `worker.rework_session` accepts, in one list: the parser refuses anything else
/// and the manual's drift test reads it.
pub const REWORK_SESSIONS: &[ReworkSession] = &[ReworkSession::Fresh, ReworkSession::Resume];

impl ReworkSession {
    /// The stored spelling: the TOML value and the word the manual uses.
    pub fn as_str(self) -> &'static str {
        match self {
            ReworkSession::Fresh => "fresh",
            ReworkSession::Resume => "resume",
        }
    }

    /// The value `text` names, if any.
    pub fn parse(text: &str) -> Option<Self> {
        REWORK_SESSIONS
            .iter()
            .copied()
            .find(|value| value.as_str() == text)
    }
}

impl std::fmt::Display for ReworkSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A value outside [`REWORK_SESSIONS`] is refused by name when the file is read, so the field
/// never holds one.
impl<'de> Deserialize<'de> for ReworkSession {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`worker.rework_session` is `{text}`; it must be one of {}",
                REWORK_SESSIONS
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

/// `[worker]`: how arc starts the model process of one attempt.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Worker {
    /// Extra `radian acp` arguments, after the wrapper's fixed ones. Empty: the worker uses
    /// the host's normal model configuration. It belongs to the `radian acp` worker line
    /// only: a file that sets it together with [`Worker::command`] is refused.
    pub exec_args: Vec<String>,
    /// The program and its arguments of a worker that is not `radian acp` (ADR 0048
    /// decision 7): the first element is the program, the rest are its arguments, and no
    /// shell is involved. `None` (the default) keeps `radian acp`. An empty array is refused.
    pub command: Option<Vec<String>>,
    /// Whether a later attempt of a task gets a session of its own (`fresh`, the default) or
    /// continues the task's one session (`resume`).
    pub rework_session: ReworkSession,
}

/// One `[flows.<name>]` section: the worker and settings a task with `flow = "<name>"`
/// runs with (ADR 0048 decision 15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Flow {
    /// Extra `radian acp` arguments, after the wrapper's fixed ones. Empty: the host's normal
    /// model configuration. It belongs to the `radian acp` worker line only: a flow that sets
    /// it together with [`Flow::command`] is refused.
    pub exec_args: Vec<String>,
    /// The program and its arguments of a worker that is not `radian acp`: the first element
    /// is the program, the rest are its arguments, and no shell is involved. `None` keeps
    /// `radian acp` with this flow's `exec_args`. An empty array is refused.
    pub command: Option<Vec<String>>,
    /// The program and its arguments of an ACP agent: the first element is the program, the
    /// rest are its arguments, and no shell is involved. `None` keeps `radian acp` with this
    /// flow's `exec_args`, or the flow's `command`. The wrapper is the ACP client and starts
    /// this array as the agent (ADR 0048 decision 7). An empty array is refused, and a flow
    /// sets at most one of `acp`, `command` and a non-empty `exec_args`.
    pub acp: Option<Vec<String>>,
    /// How many tasks of this flow may be in `doing` at once. The implicit `default` flow of a
    /// file without `[flows]` has no limit of its own and counts against `[limits] concurrency`
    /// alone (ADR 0048 decision 13).
    pub concurrency: u32,
    /// The `[gate.profiles.<name>]` section this flow uses by default. Unset, it is the value
    /// of `gate.profile`; the key is parsed and validated, and a gate run of a task of this
    /// flow resolves it (ADR 0048 decisions 10 and 15).
    pub gate_profile: Option<String>,
    /// The value a task of this flow uses at its delivery check when the task's own
    /// `allow_noop` is absent; an explicit task value wins (ADR 0048 decisions 8 and 15).
    pub allow_noop: bool,
    /// Whether the wrapper commits what a `completed` worker left uncommitted or untracked
    /// before the delivery check (ADR 0048 decision 8). A changed git control file still
    /// fails the attempt, and a failed or cancelled worker's changes are not committed. Read
    /// at the claim, so a change takes effect at the next claim.
    pub commit_dirty: bool,
}

impl Default for Flow {
    fn default() -> Self {
        Self {
            exec_args: Vec::new(),
            command: None,
            acp: None,
            concurrency: DEFAULT_FLOW_CONCURRENCY,
            gate_profile: None,
            allow_noop: false,
            commit_dirty: false,
        }
    }
}

impl Flow {
    /// The `default` flow built from today's global keys: the worker line, the gate profile,
    /// concurrency `1`, `allow_noop` `false`, and `commit_dirty` `false`.
    fn from_worker(worker: &Worker, gate_profile: &str) -> Self {
        Self {
            exec_args: worker.exec_args.clone(),
            command: worker.command.clone(),
            acp: None,
            concurrency: DEFAULT_FLOW_CONCURRENCY,
            gate_profile: Some(gate_profile.to_owned()),
            allow_noop: false,
            commit_dirty: false,
        }
    }

    /// The gate profile this flow names, or `gate.profile` when its section leaves it out.
    pub fn gate_profile<'a>(&'a self, gate_profile: &'a str) -> &'a str {
        self.gate_profile.as_deref().unwrap_or(gate_profile)
    }
}

/// The flow `name` names, when the file defines it. `default` always exists after parsing.
pub fn flow<'a>(config: &'a Config, name: &str) -> Option<&'a Flow> {
    config.flows.get(name)
}

/// The refusal of an unknown flow, naming every flow the file defines.
pub fn unknown_flow_message(name: &str, config: &Config) -> String {
    format!(
        "`{name}` is not a flow; flows are {}",
        config
            .flows
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The line the loop and `show`'s queue reason print for a task whose `flow` names no section.
pub fn missing_flow_reason(task: &str, flow: &str) -> String {
    format!("task `{task}` uses flow `{flow}`, which `config.toml` does not define")
}

/// `[ids]`: the prefix every new plan and task id starts with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ids {
    /// The first letters of a new plan id, before its four-digit number.
    pub plan_prefix: String,
    /// The first letters of a new task id, before its four-digit number.
    pub task_prefix: String,
}

impl Default for Ids {
    fn default() -> Self {
        Self {
            plan_prefix: "PL".to_owned(),
            task_prefix: "TK".to_owned(),
        }
    }
}

/// The `[gate]` table the file starts with: profile `default`, `commands` then `human`, both
/// required. The gate's [`GateConfig::default`] has no profiles (a library that is handed one);
/// the file has to be usable as written, so the file's default is this.
fn default_gate() -> GateConfig {
    let mut profiles = BTreeMap::new();
    profiles.insert("default".to_owned(), default_profile());
    GateConfig {
        profile: "default".to_owned(),
        command_timeout_secs: DEFAULT_COMMAND_TIMEOUT_SECS,
        profiles,
        review: ReviewConfig::default(),
    }
}

fn default_profile() -> Profile {
    Profile {
        stages: vec![
            StageSpec {
                stage: Stage::Commands,
                required: true,
            },
            StageSpec {
                stage: Stage::Human,
                required: true,
            },
        ],
        before_attempt: default_before_attempt(),
        commands: Vec::new(),
        key_commands: Vec::new(),
    }
}

/// The file `init` writes when none exists, with the main branch it found. The remote is the
/// default `origin`; [`default_text_with_remote`] writes the remote `init` found.
pub fn default_text(main_branch: &str) -> String {
    default_text_with_remote(main_branch, &Repository::default().remote)
}

/// The file `init` writes, with the remote and main branch it found. The default `after_merge`
/// lines name that remote, so a repository whose remote is not `origin` deletes its own task
/// branch.
pub fn default_text_with_remote(main_branch: &str, remote: &str) -> String {
    format!(
        r#"schema_version = 1

[repository]
remote = "{remote}"
main_branch = "{main_branch}"          # what init found

[limits]
concurrency = 1
max_attempts = 3
stall_timeout_secs = 900
max_wall_time_secs = 7200
git_remote_timeout_secs = 300   # limit of one git call to a remote; local git is unbounded
# max_cost_usd = 5.0          # not set = no limit
cancel_grace_secs = 10
max_auto_rework = 2
max_conflict_rework = 3       # consecutive conflicts before the task waits for you
max_auto_reverify = 2         # 0 turns the automatic reverify off
max_gate_deaths = 2           # 0 stops at the first gate death

[worker]
exec_args = []                 # extra `radian acp` arguments for the worker
rework_session = "fresh"       # `fresh`: a session per attempt; `resume`: the task's one session
# command = ["codex", "exec"]  # not set: the worker is `radian acp`; do not combine it with exec_args

[ids]
plan_prefix = "PL"             # 1 to 4 uppercase letters; the first letters of a new plan id
task_prefix = "TK"             # 1 to 4 uppercase letters; the first letters of a new task id

[integration]
key_commands = []             # what the integration stage runs on the merge candidate
# target_branch = "main"      # not set: the value of [repository] main_branch
# method = "push"             # `push`, or `command` with a `command` line below
# command = "./merge.sh"     # the line `method = "command"` runs; refused with `push`
# style = "merge"             # `merge`, `squash` or `rebase`: how the candidate is built

[hooks]
after_merge = [                # shell command lines run after a task reaches done.merged
  "git push {remote} --delete work/$ARC_TASK",
  "git worktree remove --force $ARC_WORKTREE",
  "git branch -D work/$ARC_TASK",
]

[gate]
profile = "default"
command_timeout_secs = 3600

[gate.review]
# exec_args = ["--model", "m"]   # not set: the same as [worker] exec_args
timeout_secs = 900              # wall-time limit of one review session; 0 waits forever
concurrency = 2                 # how many review sessions run at once in this repository; 1 or more
propose_followups = true        # add the plan's review follow-up once it has no open task
# prompt_file = "review.md"      # not set: the built-in prompt only

[gate.review.first]
# exec_args = ["--model", "m"]   # not set: the same as [gate.review] exec_args
# timeout_secs = 1800            # not set: the same as [gate.review] timeout_secs

[gate.profiles.default]
stages = [{{ stage = "commands", required = true }}, {{ stage = "human", required = true }}]
before_attempt = ["merge-main"]   # steps the wrapper runs in the worktree before the worker; [] runs none
commands = []                 # shell command lines, run in order on the gate checkout
"#
    )
}

/// Parses the file's text. Unknown keys, an unknown schema version, an unknown stage
/// name, a profile that lists `integration` or a stage twice, a `gate.profile` that names
/// no profile, and an id prefix that is not 1 to 4 uppercase letters (or is the same for
/// both kinds) are errors that name the offender.
pub fn parse(text: &str) -> Result<Config, String> {
    check_profile_key_commands(text)?;
    check_review_shape(text)?;
    check_worker_shape(text)?;
    check_hooks_shape(text)?;
    // A bare `[flows]` table counts too: the manual and the ADR say the per-flow limit is on
    // once the file writes a `[flows]` section, so the table's presence is read from the
    // document, not inferred from the named sections the map holds.
    let flows_configured = check_flows_shape(text)?;
    let mut config: Config = toml::from_str(text).map_err(|error| error.to_string())?;
    config.flows_configured = flows_configured;
    if config.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {} (this build reads {SCHEMA_VERSION})",
            config.schema_version
        ));
    }
    if config.limits.git_remote_timeout_secs == 0 {
        return Err(
            "`limits.git_remote_timeout_secs` is 0; it must be 1 second or more".to_owned(),
        );
    }
    // `command` and `method` belong to each other: a command method without a command line
    // has nothing to run, and a push with one would never run the line.
    match config.integration.method {
        IntegrationMethod::Command => {
            let line = config
                .integration
                .command
                .as_deref()
                .map(str::trim)
                .filter(|line| !line.is_empty());
            if line.is_none() {
                return Err(
                    "`integration.method` is `command`, so `[integration] command` must name the \
                     one shell command line that merges the candidate"
                        .to_owned(),
                );
            }
        }
        IntegrationMethod::Push => {
            if config.integration.command.is_some() {
                return Err(
                    "`integration.command` is set while `integration.method` is `push`; set \
                     `integration.method = \"command\"` or remove the command"
                        .to_owned(),
                );
            }
        }
        // The deserializer refuses `pr` as not built, so it never reaches here.
        IntegrationMethod::Pr => {}
    }
    // A command worker is one program and its arguments, and `exec_args` describes the other
    // worker line. Both set is ambiguous, so the file is refused by both names.
    if let Some(command) = &config.worker.command {
        if command.is_empty() {
            return Err(
                "`worker.command` is empty; its first element must be the program".to_owned(),
            );
        }
        if !config.worker.exec_args.is_empty() {
            return Err(format!(
                "`worker.command` and `worker.exec_args` are both set; `worker.exec_args` belongs \
                 to the `radian acp` worker line only, so set one or the other (the command is \
                 `{}`)",
                command[0]
            ));
        }
    }
    // One validation for every profile: `Profile::validate` reads the gate's one closed
    // stage list, so the parser cannot drift from it.
    for (name, profile) in &config.gate.profiles {
        profile
            .validate()
            .map_err(|error| format!("gate profile `{name}`: {error}"))?;
    }
    if !config.gate.profiles.contains_key(&config.gate.profile) {
        return Err(format!(
            "gate.profile is `{}` but no [gate.profiles.{}] section exists",
            config.gate.profile, config.gate.profile
        ));
    }
    check_flows(&mut config)?;
    check_prefix("ids.plan_prefix", &config.ids.plan_prefix)?;
    check_prefix("ids.task_prefix", &config.ids.task_prefix)?;
    if config.ids.plan_prefix == config.ids.task_prefix {
        return Err(format!(
            "`ids.plan_prefix` and `ids.task_prefix` are both `{}`; they must differ",
            config.ids.plan_prefix
        ));
    }
    if let Some(file) = &config.gate.review.prompt_file {
        check_prompt_file(file)?;
    }
    if config.gate.review.concurrency == 0 {
        return Err(
            "`gate.review.concurrency` is 0; it must be 1 or more (how many review sessions run \
             at once in the repository)"
                .to_owned(),
        );
    }
    if config.gate.review.first.timeout_secs == Some(0) {
        return Err(
            "`gate.review.first.timeout_secs` is 0; it must be 1 or more (the wall-time limit of \
             a first review's session)"
                .to_owned(),
        );
    }
    Ok(config)
}

/// Names a `gate.profiles.<name>.key_commands` before the TOML deserializer accepts it: the
/// key moved to `[integration] key_commands`, so a profile that still writes it is refused.
/// The check reads the document, so an empty array is refused too: the key itself is the
/// mistake, not its value.
fn check_profile_key_commands(text: &str) -> Result<(), String> {
    let document: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let Some(profiles) = document
        .get("gate")
        .and_then(|gate| gate.get("profiles"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(());
    };
    for (name, profile) in profiles {
        if profile.get("key_commands").is_some() {
            return Err(format!(
                "`gate.profiles.{name}.key_commands` is set; `key_commands` moved to \
                 `[integration] key_commands`, so set it there and remove it from the gate profile"
            ));
        }
    }
    Ok(())
}

/// Names a `gate.review.exec_args` or `gate.review.first.exec_args` that is not an array of
/// strings before the TOML deserializer reports a position without the key. A non-array value
/// is refused by name.
fn check_review_shape(text: &str) -> Result<(), String> {
    let document: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let Some(review) = document.get("gate").and_then(|gate| gate.get("review")) else {
        return Ok(());
    };
    check_exec_args_shape(review, "gate.review.exec_args")?;
    if let Some(first) = review.get("first") {
        check_exec_args_shape(first, "gate.review.first.exec_args")?;
    }
    Ok(())
}

/// `key` under `review`, when present, must be an array of strings.
fn check_exec_args_shape(review: &toml::Value, key: &str) -> Result<(), String> {
    if let Some(exec_args) = review.get("exec_args")
        && !exec_args
            .as_array()
            .is_some_and(|items| items.iter().all(toml::Value::is_str))
    {
        return Err(format!(
            "`{key}` must be an array of strings (the `radian exec` arguments of the review \
             session)"
        ));
    }
    Ok(())
}

/// `worker.command` must be an array of strings before the TOML deserializer reports a
/// position without the key. A non-array value is refused by name.
fn check_worker_shape(text: &str) -> Result<(), String> {
    let document: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let Some(command) = document
        .get("worker")
        .and_then(|worker| worker.get("command"))
    else {
        return Ok(());
    };
    if !command
        .as_array()
        .is_some_and(|items| items.iter().all(toml::Value::is_str))
    {
        return Err(
            "`worker.command` must be an array of strings (the program first, then its \
             arguments)"
                .to_owned(),
        );
    }
    Ok(())
}

/// Every `hooks.<point>` must be an array of strings before the TOML deserializer reports a
/// position without the key. A non-array value is refused by name, for every point the code
/// knows; an unknown point name is the deserializer's unknown-key error.
fn check_hooks_shape(text: &str) -> Result<(), String> {
    let document: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let Some(hooks) = document.get("hooks").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for point in crate::hooks::HOOK_POINTS {
        let Some(value) = hooks.get(*point) else {
            continue;
        };
        if !value
            .as_array()
            .is_some_and(|items| items.iter().all(toml::Value::is_str))
        {
            return Err(format!(
                "`hooks.{point}` must be an array of strings (shell command lines run in the \
                 repository root)"
            ));
        }
    }
    Ok(())
}

/// `flows.<name>.command` and `flows.<name>.acp` must be arrays of strings before the TOML
/// deserializer reports a position without the key. A non-array value is refused by name.
/// Returns whether the file writes a `[flows]` table at all: a bare table counts (ADR 0048
/// decision 13).
fn check_flows_shape(text: &str) -> Result<bool, String> {
    let document: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let Some(flows) = document.get("flows").and_then(toml::Value::as_table) else {
        return Ok(false);
    };
    for (name, flow) in flows {
        for key in ["command", "acp"] {
            let Some(words) = flow.get(key) else {
                continue;
            };
            if !words
                .as_array()
                .is_some_and(|items| items.iter().all(toml::Value::is_str))
            {
                return Err(format!(
                    "`flows.{name}.{key}` must be an array of strings (the program first, then its \
                     arguments)"
                ));
            }
        }
    }
    Ok(true)
}

/// Validates every `[flows.<name>]` section, refuses a `[flows.default]` beside `[worker]`'s
/// worker keys, fills the `default` flow from those keys when the file has no
/// `[flows.default]`, and resolves each flow's `gate_profile` from `gate.profile`.
fn check_flows(config: &mut Config) -> Result<(), String> {
    // One setting lives in one place: a `default` section means the worker comes from it, and
    // today's `[worker]` keys that build the default flow may not also carry a value.
    if config.flows.contains_key(DEFAULT_FLOW) {
        if config.worker.command.is_some() {
            return Err(format!(
                "`flows.{DEFAULT_FLOW}` and `worker.command` are both set; the default flow's \
                 worker is one place, so set one or the other"
            ));
        }
        if !config.worker.exec_args.is_empty() {
            return Err(format!(
                "`flows.{DEFAULT_FLOW}` and `worker.exec_args` are both set; the default flow's \
                 worker is one place, so set one or the other"
            ));
        }
    }
    for (name, flow) in &config.flows {
        if !valid_flow_name(name) {
            return Err(format!(
                "`{name}` is not a flow name: 1 to {FLOW_NAME_MAX_CHARS} characters from \
                 [a-z0-9-], with no leading or trailing -"
            ));
        }
        if let Some(acp) = &flow.acp
            && acp.is_empty()
        {
            return Err(format!(
                "`flows.{name}.acp` is empty; its first element must be the program"
            ));
        }
        if let Some(command) = &flow.command {
            if command.is_empty() {
                return Err(format!(
                    "`flows.{name}.command` is empty; its first element must be the program"
                ));
            }
            // An `acp` beside these is deferred to `worker_args::check_flow`, so every
            // combination that involves `acp` is refused (exit `3`) by the worker check and
            // names all the keys the flow set. `command` beside `exec_args` alone keeps this
            // parse-time error.
            if !flow.exec_args.is_empty() && flow.acp.is_none() {
                return Err(format!(
                    "`flows.{name}.command` and `flows.{name}.exec_args` are both set; they \
                     belong to different worker lines, so set one (the command is `{}`)",
                    command[0]
                ));
            }
        }
        if flow.concurrency == 0 {
            return Err(format!(
                "`flows.{name}.concurrency` is 0; it must be 1 or more"
            ));
        }
    }
    if !config.flows.contains_key(DEFAULT_FLOW) {
        let flow = Flow::from_worker(&config.worker, &config.gate.profile);
        config.flows.insert(DEFAULT_FLOW.to_owned(), flow);
    }
    // An unset `gate_profile` is `gate.profile`, and every name must be one of the sections the
    // file defines. `gate.profile` was checked above, so its own name resolves.
    let gate_profile = config.gate.profile.clone();
    for (name, flow) in &mut config.flows {
        let profile = flow
            .gate_profile
            .get_or_insert_with(|| gate_profile.clone());
        if !config.gate.profiles.contains_key(profile) {
            return Err(format!(
                "`flows.{name}.gate_profile` is `{profile}` but no [gate.profiles.{profile}] \
                 section exists"
            ));
        }
    }
    Ok(())
}

/// `gate.review.prompt_file` is read from the repository root: it must be a relative path
/// that does not climb out of it.
fn check_prompt_file(file: &str) -> Result<(), String> {
    let path = Path::new(file);
    if file.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "`gate.review.prompt_file` is `{file}`; it must be a path relative to the \
             repository root without `..`"
        ));
    }
    Ok(())
}

/// A prefix must be 1 to 4 ASCII uppercase letters; the error names the key.
fn check_prefix(key: &str, prefix: &str) -> Result<(), String> {
    if valid_prefix(prefix) {
        return Ok(());
    }
    Err(format!(
        "`{key}` is `{prefix}`; it must be 1 to {PREFIX_MAX_CHARS} ASCII uppercase letters"
    ))
}

/// Reads and parses a `config.toml`.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The remote git calls are bounded by `[limits] git_remote_timeout_secs`: its default is
    /// the code's, the text `init` writes names it, a file without it takes the default, and
    /// `0` is refused by name.
    #[test]
    fn the_git_remote_timeout_is_a_bounded_positive_seconds_value() {
        assert_eq!(
            Limits::default().git_remote_timeout_secs,
            crate::git::DEFAULT_REMOTE_TIMEOUT_SECS
        );
        let text = default_text("main");
        assert!(
            text.contains("git_remote_timeout_secs = 300"),
            "init names the key:\n{text}"
        );
        assert_eq!(
            parse(&text)
                .expect("the default text parses")
                .limits
                .git_remote_timeout_secs,
            crate::git::DEFAULT_REMOTE_TIMEOUT_SECS
        );
        assert_eq!(
            parse("schema_version = 1\n")
                .expect("an empty file")
                .limits
                .git_remote_timeout_secs,
            crate::git::DEFAULT_REMOTE_TIMEOUT_SECS,
            "a missing key takes the default"
        );
        for value in ["1", "300"] {
            let config = parse(&format!(
                "schema_version = 1\n[limits]\ngit_remote_timeout_secs = {value}\n"
            ))
            .expect("a positive value parses");
            assert_eq!(
                config.limits.git_remote_timeout_secs,
                value.parse::<u64>().unwrap()
            );
        }
        let error = parse("schema_version = 1\n[limits]\ngit_remote_timeout_secs = 0\n")
            .expect_err("0 must be refused");
        assert!(error.contains("limits.git_remote_timeout_secs"), "{error}");
    }

    /// The text `init` writes parses, and it equals the code's defaults, except the three
    /// `[hooks] after_merge` lines `init` starts a repository with: the code's default is an
    /// empty array, so a repository that never configured hooks runs none. `key_commands` is
    /// written under `[integration]` and not in the gate profile.
    #[test]
    fn the_default_text_parses_to_the_code_defaults() {
        let text = default_text("main");
        assert!(
            text.contains("[integration]\nkey_commands = []"),
            "init writes the integration key commands:\n{text}"
        );
        let profile = text
            .split("[gate.profiles.default]")
            .nth(1)
            .expect("the default profile is in the text");
        assert!(
            !profile.contains("key_commands"),
            "the gate profile no longer writes key_commands:\n{profile}"
        );
        let parsed = parse(&text).expect("the default text parses");
        assert_eq!(
            parsed.hooks.after_merge,
            vec![
                "git push origin --delete work/$ARC_TASK".to_owned(),
                "git worktree remove --force $ARC_WORKTREE".to_owned(),
                "git branch -D work/$ARC_TASK".to_owned(),
            ],
            "init writes the three default after_merge lines"
        );
        let mut expected = Config::default();
        expected.hooks.after_merge = parsed.hooks.after_merge.clone();
        assert_eq!(parsed, expected);
    }

    /// `[hooks]`: every point defaults to `[]`, each parses a list of strings, `init` writes
    /// three `after_merge` lines that name the configured remote, and a value that is not an
    /// array of strings is refused by name.
    #[test]
    fn the_hooks_section_defaults_to_empty_lists_and_parses_every_point() {
        let defaults = Hooks::default();
        for point in crate::hooks::HOOK_POINTS {
            assert!(
                defaults.commands(point).is_empty(),
                "`hooks.{point}` defaults to []"
            );
            let text = format!("schema_version = 1\n[hooks]\n{point} = [\"true\"]\n");
            let configured = parse(&text).expect("a built point parses");
            assert_eq!(
                configured.hooks.commands(point),
                ["true".to_owned()],
                "`hooks.{point}` reads its list"
            );
            let empty = format!("schema_version = 1\n[hooks]\n{point} = []\n");
            assert!(
                parse(&empty)
                    .expect("an empty list parses")
                    .hooks
                    .commands(point)
                    .is_empty()
            );
        }
        assert!(
            parse("schema_version = 1\n")
                .expect("the minimum file")
                .hooks
                .after_merge
                .is_empty()
        );
        let configured = parse(
            "schema_version = 1\n[hooks]\nafter_merge = [\"git status\", \"git branch -D x\"]\n",
        )
        .expect("an after_merge list parses");
        assert_eq!(
            configured.hooks.after_merge,
            vec!["git status".to_owned(), "git branch -D x".to_owned()]
        );
        assert_eq!(
            configured.hooks.commands(crate::hooks::AFTER_MERGE),
            configured.hooks.after_merge.as_slice()
        );

        let shape = parse("schema_version = 1\n[hooks]\nafter_merge = \"git status\"\n")
            .expect_err("a string is not an array");
        assert!(shape.contains("hooks.after_merge"), "{shape}");
        assert!(shape.contains("array of strings"), "{shape}");
        for point in crate::hooks::HOOK_POINTS {
            let text = format!("schema_version = 1\n[hooks]\n{point} = \"git status\"\n");
            let error = parse(&text).expect_err("a string is not an array");
            assert!(
                error.contains(&format!("hooks.{point}")),
                "the error names the point: {error}"
            );
            assert!(error.contains("array of strings"), "{error}");
        }
        assert!(
            parse("schema_version = 1\n[hooks]\nafter_nothing = []\n").is_err(),
            "an unknown point is an unknown key"
        );
    }

    /// The default text's `after_merge` lines name the remote `init` found, not a literal
    /// `origin`, so a repository with another remote deletes its own task branch.
    #[test]
    fn the_default_hooks_name_the_configured_remote() {
        let text = default_text_with_remote("trunk", "upstream");
        assert!(
            text.contains("git push upstream --delete work/$ARC_TASK"),
            "{text}"
        );
        assert!(!text.contains("git push origin"), "{text}");
        let parsed = parse(&text).expect("a custom remote parses");
        assert_eq!(parsed.repository.remote, "upstream");
        assert_eq!(parsed.repository.main_branch, "trunk");
    }

    /// `[integration]` defaults: `key_commands` empty, `target_branch` unset (the repository's
    /// `main_branch`), `method` `push`, `style` `merge`.
    #[test]
    fn the_integration_section_defaults_to_push_and_merge() {
        let integration = Integration::default();
        assert!(integration.key_commands.is_empty());
        assert_eq!(integration.target_branch, None);
        assert_eq!(integration.method, IntegrationMethod::Push);
        assert_eq!(integration.style, IntegrationStyle::Merge);
        assert!(
            OPTIONAL_KEYS.contains(&"integration.target_branch"),
            "the manual's table names it as not set"
        );

        let parsed = parse("schema_version = 1\n").expect("the minimum file parses");
        assert_eq!(parsed.integration, Integration::default());
        assert_eq!(parsed.target_branch(), "main");

        let custom = parse("schema_version = 1\n[repository]\nmain_branch = \"trunk\"\n")
            .expect("a custom main branch");
        assert_eq!(custom.target_branch(), "trunk");

        let target = parse("schema_version = 1\n[integration]\ntarget_branch = \"release\"\n")
            .expect("a target branch");
        assert_eq!(target.target_branch(), "release");
    }

    /// A gate profile that still writes `key_commands` is refused, naming the profile and the
    /// place the key moved to. The key itself is the mistake, so an empty array is refused too.
    #[test]
    fn key_commands_in_a_gate_profile_is_refused_by_name() {
        for value in ["key_commands = [\"false\"]\n", "key_commands = []\n"] {
            let text = format!("schema_version = 1\n[gate.profiles.default]\n{value}");
            let error = parse(&text).expect_err("a profile key_commands must fail");
            assert!(error.contains("[integration] key_commands"), "{error}");
            assert!(
                error.contains("gate.profiles.default.key_commands"),
                "the error names the profile key: {error}"
            );
        }
    }

    /// `integration.method` and `integration.style` are closed sets. Every style parses; the
    /// method `command` is built and needs its command line, `pr` is refused as not built, and
    /// an unknown value names every allowed one.
    #[test]
    fn integration_method_and_style_are_closed_sets() {
        let methods: Vec<&str> = INTEGRATION_METHODS
            .iter()
            .map(|value| value.as_str())
            .collect();
        assert_eq!(methods, ["push", "command", "pr"]);
        let styles: Vec<&str> = INTEGRATION_STYLES
            .iter()
            .map(|value| value.as_str())
            .collect();
        assert_eq!(styles, ["merge", "squash", "rebase"]);

        for value in INTEGRATION_METHODS {
            assert_eq!(IntegrationMethod::parse(value.as_str()), Some(*value));
            let command = if *value == IntegrationMethod::Command {
                "command = \"true\"\n"
            } else {
                ""
            };
            let text =
                format!("schema_version = 1\n[integration]\nmethod = \"{value}\"\n{command}");
            if value.is_built() {
                assert_eq!(
                    parse(&text).expect("a built method").integration.method,
                    *value
                );
            } else {
                let error = parse(&text).expect_err("an unbuilt method");
                assert!(error.contains("not built yet"), "{error}");
                assert!(error.contains(value.as_str()), "{error}");
            }
        }
        // `command` and `method` belong to each other.
        let missing = parse("schema_version = 1\n[integration]\nmethod = \"command\"\n")
            .expect_err("a command method needs its command");
        assert!(missing.contains("[integration] command"), "{missing}");
        for empty in ["", "   "] {
            let text = format!(
                "schema_version = 1\n[integration]\nmethod = \"command\"\ncommand = \"{empty}\"\n"
            );
            let error = parse(&text).expect_err("an empty command");
            assert!(error.contains("[integration] command"), "{error}");
        }
        let pushed =
            parse("schema_version = 1\n[integration]\nmethod = \"push\"\ncommand = \"true\"\n")
                .expect_err("push refuses a command");
        assert!(pushed.contains("integration.command"), "{pushed}");
        assert!(pushed.contains("push"), "{pushed}");
        for value in INTEGRATION_STYLES {
            assert_eq!(IntegrationStyle::parse(value.as_str()), Some(*value));
            let text = format!("schema_version = 1\n[integration]\nstyle = \"{value}\"\n");
            assert_eq!(
                parse(&text).expect("a built style").integration.style,
                *value
            );
        }

        for (key, text) in [
            (
                "method",
                "schema_version = 1\n[integration]\nmethod = \"merge\"\n",
            ),
            (
                "style",
                "schema_version = 1\n[integration]\nstyle = \"push\"\n",
            ),
        ] {
            let error = parse(text).expect_err("a value from the other set");
            assert!(error.contains(&format!("integration.{key}")), "{error}");
            assert!(error.contains("must be one of"), "{error}");
        }
    }

    /// A file with no `[gate]` table gets the profile `init` would have written.
    #[test]
    fn a_missing_gate_table_gets_the_default_profile() {
        let parsed = parse("schema_version = 1\n").expect("the minimum file parses");
        assert_eq!(parsed.gate, default_gate());
        assert_eq!(parsed.gate.selected_profile(), Some(&default_profile()));
    }

    /// Unknown keys are rejected at every level, and the error names the key.
    #[test]
    fn an_unknown_key_is_rejected() {
        for text in [
            "schema_version = 1\nnope = 1\n",
            "schema_version = 1\n[repository]\nupstream = \"x\"\n",
            "schema_version = 1\n[limits]\nbudget = 1\n",
            "schema_version = 1\n[gate]\nkeys = []\n",
            "schema_version = 1\n[gate.profiles.default]\nrunner = \"x\"\n",
            "schema_version = 1\n[integration]\nmode = \"x\"\n",
        ] {
            let error = parse(text).expect_err("an unknown key must fail");
            assert!(!error.is_empty(), "{error}");
        }
    }

    /// The stage set is the closed list: `integration` is not a profile stage.
    #[test]
    fn an_unknown_stage_name_is_rejected() {
        let text = default_text("main").replace(
            "{ stage = \"human\", required = true }",
            "{ stage = \"integration\", required = true }",
        );
        let error = parse(&text).expect_err("integration is not a profile stage");
        assert!(error.contains("integration"), "{error}");
        assert!(!STAGE_NAMES.contains(&"integration"));
    }

    /// A stage listed twice is refused by the same validation.
    #[test]
    fn a_duplicate_stage_is_rejected() {
        let text = default_text("main").replace(
            "{ stage = \"human\", required = true }",
            "{ stage = \"commands\", required = false }",
        );
        let error = parse(&text).expect_err("a stage listed twice");
        assert!(error.contains("twice"), "{error}");
    }

    /// A newer schema version is refused, never half-read.
    #[test]
    fn a_higher_schema_version_is_refused() {
        let text = default_text("main").replace("schema_version = 1", "schema_version = 2");
        let error = parse(&text).expect_err("version 2 is refused");
        assert!(error.contains('2'), "{error}");
    }

    /// `gate.profile` must name an existing section.
    #[test]
    fn a_profile_name_with_no_section_is_refused() {
        let text = default_text("main").replace("profile = \"default\"", "profile = \"strict\"");
        assert!(parse(&text).is_err());
    }

    /// The manual documents the closed stage list from the gate's one constant.
    #[test]
    fn the_stage_names_are_the_gate_list() {
        assert_eq!(
            STAGE_NAMES,
            ["commands", "mutation", "review", "ci", "human"]
        );
        assert_eq!(
            crate::gate::LISTED_STAGES.map(|stage| stage.as_str()),
            STAGE_NAMES
        );
    }

    /// `init` writes the default prefixes, a file without the section gets the defaults, and a
    /// file with the section keeps what it says.
    #[test]
    fn the_ids_section_has_the_default_prefixes() {
        assert_eq!(Ids::default().plan_prefix, "PL");
        assert_eq!(Ids::default().task_prefix, "TK");
        let text = default_text("main");
        assert!(
            text.contains("[ids]\n")
                && text.contains("plan_prefix = \"PL\"")
                && text.contains("task_prefix = \"TK\""),
            "{text}"
        );
        assert_eq!(
            parse(&text).expect("the default text parses").ids,
            Ids::default()
        );
        assert_eq!(
            parse("schema_version = 1\n").expect("an empty file").ids,
            Ids::default(),
            "a missing [ids] section takes the defaults"
        );
        let custom = parse("schema_version = 1\n[ids]\nplan_prefix = \"P\"\ntask_prefix = \"T\"\n")
            .expect("a custom section parses");
        assert_eq!(custom.ids.plan_prefix, "P");
        assert_eq!(custom.ids.task_prefix, "T");
    }

    /// An empty, lowercase, five-letter or equal prefix is refused, and the error names the
    /// key.
    #[test]
    fn a_bad_prefix_is_refused_by_name() {
        let base = default_text("main");
        for (key, value) in [
            ("plan_prefix", ""),
            ("plan_prefix", "pl"),
            ("plan_prefix", "TOOLS"),
            ("task_prefix", "t"),
        ] {
            let text = if key == "plan_prefix" {
                base.replace(
                    "plan_prefix = \"PL\"",
                    &format!("plan_prefix = \"{value}\""),
                )
            } else {
                base.replace(
                    "task_prefix = \"TK\"",
                    &format!("task_prefix = \"{value}\""),
                )
            };
            let error = parse(&text).expect_err("a bad prefix must fail");
            assert!(
                error.contains(&format!("ids.{key}")),
                "the error names `ids.{key}`: {error}"
            );
        }
        let equal = base.replace("task_prefix = \"TK\"", "task_prefix = \"PL\"");
        let error = parse(&equal).expect_err("equal prefixes must fail");
        assert!(error.contains("ids.plan_prefix"), "{error}");
        assert!(error.contains("ids.task_prefix"), "{error}");
    }

    /// `rework_session` defaults to `fresh`, the written file names it, and a file without the
    /// key takes the default. `resume` is the other value of the one closed list.
    #[test]
    fn the_worker_section_defaults_to_a_fresh_rework_session() {
        assert_eq!(
            Worker::default().rework_session,
            ReworkSession::Fresh,
            "the default is `fresh`"
        );
        assert_eq!(ReworkSession::default().as_str(), "fresh");
        let text = default_text("main");
        assert!(
            text.contains("rework_session = \"fresh\""),
            "init names the key:\n{text}"
        );
        assert_eq!(
            parse(&text).expect("the default text parses").worker,
            Worker::default()
        );
        assert_eq!(
            parse("schema_version = 1\n")
                .expect("an empty file")
                .worker
                .rework_session,
            ReworkSession::Fresh,
            "a missing key takes the default"
        );
        let resumed = parse("schema_version = 1\n[worker]\nrework_session = \"resume\"\n")
            .expect("`resume` parses");
        assert_eq!(resumed.worker.rework_session, ReworkSession::Resume);
        for value in REWORK_SESSIONS {
            assert_eq!(
                ReworkSession::parse(value.as_str()),
                Some(*value),
                "every listed value parses back"
            );
        }
    }

    /// A value outside the list is a config error, and the error names the value and the
    /// allowed ones.
    #[test]
    fn an_unknown_rework_session_is_refused_by_name() {
        let text = default_text("main").replace(
            "rework_session = \"fresh\"",
            "rework_session = \"sometimes\"",
        );
        let error = parse(&text).expect_err("an unknown value must fail");
        assert!(error.contains("rework_session"), "{error}");
        assert!(
            error.contains("sometimes"),
            "the error names the value: {error}"
        );
        for value in REWORK_SESSIONS {
            assert!(
                error.contains(&format!("`{value}`")),
                "the error lists `{value}`: {error}"
            );
        }
        assert_eq!(ReworkSession::parse("sometimes"), None);
    }

    /// `worker.command` is optional: unset keeps `radian acp`, and a set array is the program
    /// first and its arguments after it (ADR 0048 decisions 7 and 15).
    #[test]
    fn the_worker_command_is_an_optional_array_with_the_program_first() {
        assert_eq!(Worker::default().command, None, "unset by default");
        assert!(
            OPTIONAL_KEYS.contains(&"worker.command"),
            "the manual's table names it as not set"
        );
        let text = default_text("main");
        assert_eq!(
            parse(&text)
                .expect("the default text parses")
                .worker
                .command,
            None,
            "a commented example is not a value"
        );
        let configured = parse(
            "schema_version = 1\n[worker]\ncommand = [\"/usr/bin/codex\", \"exec\", \"--json\"]\n",
        )
        .expect("a command parses");
        assert_eq!(
            configured.worker.command,
            Some(vec![
                "/usr/bin/codex".to_owned(),
                "exec".to_owned(),
                "--json".to_owned()
            ])
        );
    }

    /// An empty command has no program, and a command beside a non-empty `exec_args` is
    /// ambiguous: both are refused by the key they name (ADR 0048 decision 15).
    #[test]
    fn an_empty_command_or_one_with_exec_args_is_refused_by_name() {
        let empty = parse("schema_version = 1\n[worker]\ncommand = []\n")
            .expect_err("an empty command must fail");
        assert!(empty.contains("worker.command"), "{empty}");
        assert!(empty.contains("empty"), "{empty}");

        let both = parse(
            "schema_version = 1\n[worker]\ncommand = [\"/usr/bin/codex\"]\nexec_args = \
             [\"--model\", \"m\"]\n",
        )
        .expect_err("both keys must fail");
        assert!(both.contains("worker.command"), "{both}");
        assert!(both.contains("worker.exec_args"), "{both}");
        assert!(both.contains("/usr/bin/codex"), "{both}");

        let shape = parse("schema_version = 1\n[worker]\ncommand = \"/usr/bin/codex\"\n")
            .expect_err("a string is not an array");
        assert!(shape.contains("worker.command"), "{shape}");
        assert!(shape.contains("array of strings"), "{shape}");
    }

    /// A file without `[flows]` gets the `default` flow from today's keys, and the flow's
    /// worker is the same one the file would have started before flows existed.
    #[test]
    fn a_file_without_flows_gets_the_default_flow_from_the_worker_keys() {
        assert_eq!(
            Config::default()
                .flows
                .get(DEFAULT_FLOW)
                .map(|flow| (flow.exec_args.clone(), flow.command.clone())),
            Some((Vec::new(), None)),
            "the code's default is an empty `radian acp` flow"
        );
        let text =
            default_text("main").replace("exec_args = []", "exec_args = [\"--model\", \"m\"]");
        let config = parse(&text).expect("a file without [flows]");
        let flow = config
            .flows
            .get(DEFAULT_FLOW)
            .expect("the default flow exists");
        assert_eq!(flow.exec_args, vec!["--model".to_owned(), "m".to_owned()]);
        assert_eq!(flow.command, None);
        assert_eq!(flow.gate_profile(DEFAULT_FLOW), "default");
        assert_eq!(flow.concurrency, 1);

        let configured = parse("schema_version = 1\n[worker]\ncommand = [\"/usr/bin/codex\"]\n")
            .expect("a worker command becomes the default flow's");
        assert_eq!(
            configured
                .flows
                .get(DEFAULT_FLOW)
                .and_then(|flow| flow.command.clone()),
            Some(vec!["/usr/bin/codex".to_owned()])
        );
    }

    /// A bare `[flows]` table, with no named section, turns the per-flow limit on for the
    /// implicit `default` flow: the manual says any `[flows]` section does (ADR 0048
    /// decision 13). A file with no table keeps no per-flow limit at all.
    #[test]
    fn a_bare_flows_table_holds_the_default_flow_to_its_concurrency() {
        let config = parse("schema_version = 1\n[flows]\n").expect("a bare [flows] table parses");
        assert!(config.flows_configured);
        assert_eq!(config.flow_concurrency(DEFAULT_FLOW), Some(1));

        let without = parse("schema_version = 1\n").expect("a file without [flows]");
        assert!(!without.flows_configured);
        assert_eq!(without.flow_concurrency(DEFAULT_FLOW), None);
    }

    /// A flow name is 1 to 32 characters from `[a-z0-9-]`, with no leading or trailing `-`.
    #[test]
    fn a_flow_name_follows_the_one_rule() {
        for name in ["a", "default", "game-art", "a1-b2"] {
            assert!(valid_flow_name(name), "{name}");
        }
        for name in ["", "-a", "a-", "A", "a_b", "a b"] {
            assert!(!valid_flow_name(name), "{name}");
        }
        assert!(valid_flow_name(&"a".repeat(FLOW_NAME_MAX_CHARS)));
        assert!(!valid_flow_name(&"a".repeat(FLOW_NAME_MAX_CHARS + 1)));
        let text = "schema_version = 1\n[flows.\"Game_Art\"]\nexec_args = []\n";
        let error = parse(text).expect_err("a bad name is refused");
        assert!(error.contains("Game_Art"), "{error}");
    }

    /// `[flows.default]` and a `[worker]` worker key are one setting in two places: the file is
    /// refused by both names. `[worker] rework_session` stays a `[worker]` key.
    #[test]
    fn the_default_flow_and_the_worker_keys_are_one_setting() {
        for (worker, key) in [
            ("command = [\"/usr/bin/codex\"]\n", "worker.command"),
            ("exec_args = [\"--model\", \"m\"]\n", "worker.exec_args"),
        ] {
            let text =
                format!("schema_version = 1\n[worker]\n{worker}[flows.default]\nexec_args = []\n");
            let error = parse(&text).expect_err("one worker setting, two places");
            assert!(error.contains("flows.default"), "{error}");
            assert!(error.contains(key), "the error names `{key}`: {error}");
        }
        let text = "schema_version = 1\n[worker]\nrework_session = \"resume\"\n\
                    [flows.default]\nexec_args = []\n";
        let config = parse(text).expect("[worker] rework_session belongs to every flow");
        assert_eq!(config.worker.rework_session, ReworkSession::Resume);
        assert_eq!(
            config
                .flows
                .get(DEFAULT_FLOW)
                .map(|flow| flow.exec_args.clone()),
            Some(Vec::new())
        );
    }

    /// A flow's worker is checked like `[worker]`'s: an empty `command`, `command` beside
    /// `exec_args`, `concurrency` 0 and a `gate_profile` with no section are each refused by
    /// name.
    #[test]
    fn a_broken_flow_is_refused_by_name() {
        for (table, key) in [
            ("command = []\n", "flows.art.command"),
            (
                "command = [\"/usr/bin/codex\"]\nexec_args = [\"--model\", \"m\"]\n",
                "flows.art.command",
            ),
            ("concurrency = 0\n", "flows.art.concurrency"),
            ("gate_profile = \"missing\"\n", "flows.art.gate_profile"),
        ] {
            let text = format!("schema_version = 1\n[flows.art]\n{table}");
            let error = parse(&text).expect_err("a broken flow is refused");
            assert!(error.contains(key), "the error names `{key}`: {error}");
        }
        let both = "schema_version = 1\n[flows.art]\ncommand = [\"/usr/bin/codex\"]\n\
                    exec_args = [\"--model\", \"m\"]\n";
        let error = parse(both).expect_err("command beside exec_args");
        assert!(error.contains("flows.art.exec_args"), "{error}");

        let unknown = "schema_version = 1\n[flows.art]\nrunner = \"x\"\n";
        assert!(parse(unknown).is_err(), "an unknown flow key is refused");
    }

    /// A flow's `gate_profile` defaults to `gate.profile` and is resolved when the file is read;
    /// a named existing profile is kept. `exec_args` and `concurrency` take their defaults.
    #[test]
    fn a_flow_takes_gate_profile_from_the_gate_and_its_other_defaults() {
        let text = "schema_version = 1\n[gate]\nprofile = \"strict\"\n\
                    [gate.profiles.strict]\n\
                    stages = [{ stage = \"commands\", required = true }]\n\
                    [flows.art]\n";
        let config = parse(text).expect("a flow with no keys");
        let flow = config.flows.get("art").expect("the flow exists");
        assert_eq!(flow.gate_profile("strict"), "strict");
        assert_eq!(flow.gate_profile, Some("strict".to_owned()));
        assert_eq!(flow.exec_args, Vec::<String>::new());
        assert_eq!(flow.command, None);
        assert_eq!(flow.concurrency, DEFAULT_FLOW_CONCURRENCY);
        assert!(!flow.allow_noop);
        assert!(!flow.commit_dirty);
    }

    /// `flows.<name>.allow_noop` is a boolean, `false` when its section leaves it out, and the
    /// implicit `default` flow of a file without `[flows]` has `false` (ADR 0048 decisions 8
    /// and 15). A flow the file does not define contributes `false`.
    #[test]
    fn a_flows_allow_noop_defaults_to_false_and_parses() {
        let config = parse("schema_version = 1\n[flows.art]\nallow_noop = true\n")
            .expect("a flow with allow_noop");
        assert!(config.flow_allow_noop("art"));
        assert!(!config.flow_allow_noop(DEFAULT_FLOW));
        assert!(!config.flow_allow_noop("missing"));

        let without = parse("schema_version = 1\n").expect("a file without [flows]");
        assert!(!without.flow_allow_noop(DEFAULT_FLOW));
        assert!(!Config::default().flow_allow_noop(DEFAULT_FLOW));
    }

    /// `flows.<name>.commit_dirty` is a boolean, `false` when its section leaves it out; a
    /// flow and the implicit `default` flow both default to it (ADR 0048 decision 8).
    #[test]
    fn a_flows_commit_dirty_defaults_to_false_and_parses() {
        assert!(!Flow::default().commit_dirty);
        assert!(!Config::default().flows[DEFAULT_FLOW].commit_dirty);

        let config = parse("schema_version = 1\n[flows.art]\ncommit_dirty = true\n")
            .expect("a flow with commit_dirty");
        assert!(config.flows["art"].commit_dirty);
        assert!(
            !config.flows[DEFAULT_FLOW].commit_dirty,
            "the implicit flow keeps false"
        );

        let without = parse("schema_version = 1\n[flows.art]\n").expect("a flow without the key");
        assert!(!without.flows["art"].commit_dirty);
    }

    /// A task's gate profile is its flow's `gate_profile`; a repository without `[flows]`
    /// resolves `gate.profile`. A flow the file does not define, and a flow whose
    /// `gate_profile` names no section, are refused with a message naming the flow.
    #[test]
    fn a_tasks_gate_profile_is_its_flows() {
        let text = "schema_version = 1\n\
                    [gate.profiles.default]\n\
                    stages = [{ stage = \"commands\", required = true }]\n\
                    [gate.profiles.strict]\n\
                    stages = [{ stage = \"review\", required = true }]\n\
                    [flows.art]\ngate_profile = \"strict\"\n\
                    [flows.plain]\n";
        let config = parse(text).expect("two flows and two profiles");
        let (name, profile) = config
            .task_gate_profile("TK0001-a", "art")
            .expect("the flow's profile");
        assert_eq!(name, "strict");
        assert_eq!(profile.stages[0].stage, Stage::Review);
        assert_eq!(
            config
                .task_gate_profile("TK0002-b", "plain")
                .expect("the flow inherits `gate.profile`")
                .0,
            "default"
        );

        // A repository without `[flows]`: its implicit `default` flow resolves `gate.profile`.
        let without =
            parse("schema_version = 1\n[gate.profiles.default]\nstages = []\n").expect("no flows");
        assert_eq!(
            without
                .task_gate_profile("TK0001-a", DEFAULT_FLOW)
                .expect("the implicit default flow")
                .0,
            "default"
        );

        let missing_flow = without
            .task_gate_profile("TK0001-a", "art")
            .expect_err("the file defines no `art` flow");
        for needle in ["TK0001-a", "art", "does not define"] {
            assert!(missing_flow.contains(needle), "{missing_flow}");
        }

        // A flow whose `gate_profile` names no section cannot be parsed, so the resolver is fed
        // a hand-built config: the message keeps today's missing-profile text and names the
        // flow and its key.
        let mut broken = Config::default();
        broken.flows.insert(
            "art".to_owned(),
            Flow {
                gate_profile: Some("gone".to_owned()),
                ..Flow::default()
            },
        );
        let error = broken
            .task_gate_profile("TK0001-a", "art")
            .expect_err("the profile does not exist");
        for needle in ["flows.art.gate_profile", "gone", "no such profile exists"] {
            assert!(error.contains(needle), "{error}");
        }
    }

    /// `flows.<name>.acp` is optional: unset keeps the flow's `command` or `radian acp`, and a
    /// set array is the program first and its arguments after it. The manual's table names it
    /// as not set. An empty array and a non-array value are refused (ADR 0048 decision 7).
    #[test]
    fn the_flows_acp_is_an_optional_array_with_the_program_first() {
        assert_eq!(Flow::default().acp, None, "unset by default");
        assert!(
            OPTIONAL_KEYS.contains(&"flows.<name>.acp"),
            "the manual's table names it as not set"
        );
        let text = default_text("main");
        assert_eq!(
            parse(&text).expect("the default text parses").flows[DEFAULT_FLOW].acp,
            None,
            "a commented example is not a value"
        );
        let configured =
            parse("schema_version = 1\n[flows.art]\nacp = [\"/usr/bin/codex-acp\", \"--flag\"]\n")
                .expect("an acp array parses");
        assert_eq!(
            configured.flows["art"].acp,
            Some(vec!["/usr/bin/codex-acp".to_owned(), "--flag".to_owned()])
        );

        let empty = parse("schema_version = 1\n[flows.art]\nacp = []\n")
            .expect_err("an empty acp array must fail");
        assert!(empty.contains("flows.art.acp"), "{empty}");
        assert!(empty.contains("empty"), "{empty}");

        let shape = parse("schema_version = 1\n[flows.art]\nacp = \"/usr/bin/codex-acp\"\n")
            .expect_err("a string is not an array");
        assert!(shape.contains("flows.art.acp"), "{shape}");
        assert!(shape.contains("array of strings"), "{shape}");
    }

    /// `acp` beside `command` or a non-empty `exec_args` is refused by the worker check before
    /// `start`, not when the file is read: the file still parses, so the command can exit `3`
    /// naming every key (ADR 0048 decisions 1 and 7). `command` beside `exec_args` keeps its
    /// older parse error.
    #[test]
    fn a_flow_with_acp_and_another_worker_line_still_parses() {
        let acp_and_command = "schema_version = 1\n[flows.art]\nacp = [\"/usr/bin/codex-acp\"]\n\
                               command = [\"/bin/sh\"]\n";
        let config = parse(acp_and_command).expect("the worker check refuses this, not the parser");
        assert!(config.flows["art"].acp.is_some());
        assert!(config.flows["art"].command.is_some());

        let acp_and_exec_args = "schema_version = 1\n[flows.art]\nacp = [\"/usr/bin/codex-acp\"]\n\
                                 exec_args = [\"--model\", \"m\"]\n";
        let config = parse(acp_and_exec_args).expect("the worker check refuses this too");
        assert!(config.flows["art"].acp.is_some());
        assert!(!config.flows["art"].exec_args.is_empty());

        let command_and_exec_args = "schema_version = 1\n[flows.art]\ncommand = [\"/bin/sh\"]\n\
                                     exec_args = [\"--model\", \"m\"]\n";
        assert!(
            parse(command_and_exec_args).is_err(),
            "command beside exec_args keeps its config error"
        );

        // All three names are a worker-check refusal too, so the command exits `3` for every
        // combination that involves `acp`, exactly as the manual's `acp` row says.
        let all_three = "schema_version = 1\n[flows.art]\nacp = [\"/usr/bin/codex-acp\"]\n\
                         command = [\"/bin/sh\"]\nexec_args = [\"--model\", \"m\"]\n";
        let config = parse(all_three).expect("the worker check refuses this, not the parser");
        assert!(config.flows["art"].acp.is_some());
        assert!(config.flows["art"].command.is_some());
        assert!(!config.flows["art"].exec_args.is_empty());
    }
}

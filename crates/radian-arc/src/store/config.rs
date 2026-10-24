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

/// Keys whose default is "not set" rather than a value. The manual's table names each
/// one, and the drift test checks that it does.
pub const OPTIONAL_KEYS: &[&str] = &[
    "limits.max_cost_usd",
    "gate.review.exec_args",
    "gate.review.prompt_file",
    "gate.review.first.exec_args",
    "gate.review.first.timeout_secs",
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
    pub ids: Ids,
    #[serde(default = "default_gate")]
    pub gate: GateConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repository: Repository::default(),
            limits: Limits::default(),
            worker: Worker::default(),
            ids: Ids::default(),
            gate: default_gate(),
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
    /// How many tasks run at once.
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
    /// How many times a gate may die after writing `running` for the same attempt and inputs
    /// before the pass records `died` instead of starting it again (ADR 0048 decision 10).
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
    /// Extra `radian exec` arguments, after the wrapper's fixed ones. Empty: the worker uses
    /// the host's normal model configuration.
    pub exec_args: Vec<String>,
    /// Whether a later attempt of a task gets a session of its own (`fresh`, the default) or
    /// continues the task's one session (`resume`).
    pub rework_session: ReworkSession,
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

/// The file `init` writes when none exists, with the main branch it found.
pub fn default_text(main_branch: &str) -> String {
    format!(
        r#"schema_version = 1

[repository]
remote = "origin"
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
exec_args = []                 # extra `radian exec` arguments for the worker
rework_session = "fresh"       # `fresh`: a session per attempt; `resume`: the task's one session

[ids]
plan_prefix = "PL"             # 1 to 4 uppercase letters; the first letters of a new plan id
task_prefix = "TK"             # 1 to 4 uppercase letters; the first letters of a new task id

[gate]
profile = "default"
command_timeout_secs = 3600

[gate.review]
# exec_args = ["--model", "m"]   # not set: the same as [worker] exec_args
timeout_secs = 900              # wall-time limit of one review session; 0 waits forever
concurrency = 2                 # how many review sessions run at once in this repository; 1 or more
propose_followups = true        # add the review's follow-up task when a task merges
# prompt_file = "review.md"      # not set: the built-in prompt only

[gate.review.first]
# exec_args = ["--model", "m"]   # not set: the same as [gate.review] exec_args
# timeout_secs = 1800            # not set: the same as [gate.review] timeout_secs

[gate.profiles.default]
stages = [{{ stage = "commands", required = true }}, {{ stage = "human", required = true }}]
before_attempt = ["merge-main"]   # steps the wrapper runs in the worktree before the worker; [] runs none
commands = []                 # shell command lines, run in order on the gate checkout
key_commands = []             # what the integration stage runs on the merge candidate
"#
    )
}

/// Parses the file's text. Unknown keys, an unknown schema version, an unknown stage
/// name, a profile that lists `integration` or a stage twice, a `gate.profile` that names
/// no profile, and an id prefix that is not 1 to 4 uppercase letters (or is the same for
/// both kinds) are errors that name the offender.
pub fn parse(text: &str) -> Result<Config, String> {
    check_review_shape(text)?;
    let config: Config = toml::from_str(text).map_err(|error| error.to_string())?;
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

    /// The text `init` writes parses, and it equals the code's defaults: a changed default
    /// must change the written file too.
    #[test]
    fn the_default_text_parses_to_the_code_defaults() {
        let parsed = parse(&default_text("main")).expect("the default text parses");
        assert_eq!(parsed, Config::default());
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
}

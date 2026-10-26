//! The `[gate]` table of `config.toml` (ADR 0048 decisions 10 and 15).
//!
//! These are the one set of types for the `[gate]` table: the store parses `config.toml`
//! into them (and supplies the file's defaults), and the gate's stages read them. The names
//! of the profile fields (`stages`, `commands`, `key_commands`) are the file's names.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::error::GateError;

/// The default per-command timeout in seconds (the sample value of ADR 0048 decision 15).
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 3600;

/// The default wall-time limit of one `review` session, in seconds (ADR 0048 decision 15).
pub const DEFAULT_REVIEW_TIMEOUT_SECS: u64 = 900;

/// The default number of review sessions that may run at once in a repository (ADR 0048
/// decision 10).
pub const DEFAULT_REVIEW_CONCURRENCY: u32 = 2;

/// One gate stage.
///
/// `commands`, `mutation`, `review`, `ci` and `human` are the stages a profile can list.
/// [`Stage::Integration`] always runs at merge and is never listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// The fixed acceptance commands of the profile, run in a separate checkout.
    Commands,
    /// The repository's mutation checks, run in a separate checkout.
    Mutation,
    /// A read-only review session.
    Review,
    /// The repository's CI.
    Ci,
    /// The owner's approval.
    Human,
    /// The merge candidate check. Always runs at merge; never listed in a profile.
    Integration,
}

/// The stages a profile can list, in the order ADR 0048 decision 10 names them. This is the
/// one closed stage list: [`Profile::validate`], the `config.toml` parser and the manual's
/// drift test all read it.
pub const LISTED_STAGES: [Stage; 5] = [
    Stage::Commands,
    Stage::Mutation,
    Stage::Review,
    Stage::Ci,
    Stage::Human,
];

/// The names of [`LISTED_STAGES`], in the same order: what `config.toml`, a verdict and the
/// manual spell.
pub const STAGE_NAMES: [&str; 5] = [
    LISTED_STAGES[0].as_str(),
    LISTED_STAGES[1].as_str(),
    LISTED_STAGES[2].as_str(),
    LISTED_STAGES[3].as_str(),
    LISTED_STAGES[4].as_str(),
];

impl Stage {
    /// The lower-case name used in `config.toml`, in verdict files and as the gate checkout
    /// directory name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Stage::Commands => "commands",
            Stage::Mutation => "mutation",
            Stage::Review => "review",
            Stage::Ci => "ci",
            Stage::Human => "human",
            Stage::Integration => "integration",
        }
    }

    /// Whether a profile may list this stage. `integration` is not listable.
    #[must_use]
    pub fn is_listed(self) -> bool {
        LISTED_STAGES.contains(&self)
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of a profile's `stages` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    /// The stage to run.
    pub stage: Stage,
    /// Whether the stage must pass before the task can reach `review.awaiting_human`.
    /// Omitted, a listed stage is required.
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

/// The names of the built-in steps a profile's `before_attempt` may list, in order. This is
/// the one closed list: [`Profile::validate`], the `config.toml` parser and the manual's drift
/// test all read it. A custom command is a table, not a name ([`BeforeAttempt::Run`]).
pub const BEFORE_ATTEMPT_STEPS: [&str; 1] = [MERGE_MAIN];

/// The one built-in step a profile can list in `before_attempt`: merge the remote main into the
/// task's branch before the worker starts (ADR 0048 decision 7).
pub const MERGE_MAIN: &str = "merge-main";

/// One entry of a profile's `before_attempt` (ADR 0048 decisions 7 and 10): a built-in step or
/// a command the wrapper runs in the attempt's worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum BeforeAttempt {
    /// A built-in step; [`BEFORE_ATTEMPT_STEPS`] is the closed list of names.
    Step(String),
    /// A shell command line, run as `sh -c <run>` in the attempt's worktree.
    Run(BeforeAttemptCommand),
}

/// One custom `before_attempt` command: `{ run = "<command line>", timeout_secs = <integer> }`
/// (ADR 0048 decisions 7 and 10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeforeAttemptCommand {
    /// The shell command line, run with `sh -c` in the worktree. Required and not empty.
    pub run: String,
    /// The command's time limit in seconds. Unset: `gate.command_timeout_secs`. `0` means no
    /// limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl BeforeAttempt {
    /// The built-in step this entry names, when it is one.
    #[must_use]
    pub fn step(&self) -> Option<&str> {
        match self {
            BeforeAttempt::Step(step) => Some(step),
            BeforeAttempt::Run(_) => None,
        }
    }

    /// The command this entry runs, when it is one.
    #[must_use]
    pub fn command(&self) -> Option<&BeforeAttemptCommand> {
        match self {
            BeforeAttempt::Step(_) => None,
            BeforeAttempt::Run(command) => Some(command),
        }
    }
}

/// The entry's shaped error names the offenders a TOML table would report by position only:
/// an unknown key, a duplicate key, or a missing `run` (ADR 0048 decisions 7 and 10).
impl<'de> Deserialize<'de> for BeforeAttempt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EntryVisitor;

        impl<'de> serde::de::Visitor<'de> for EntryVisitor {
            type Value = BeforeAttempt;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .write_str("`merge-main` or a table with `run` and optional `timeout_secs`")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(BeforeAttempt::Step(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(BeforeAttempt::Step(value))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut run: Option<String> = None;
                let mut timeout_secs: Option<u64> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "run" => {
                            if run.is_some() {
                                return Err(serde::de::Error::duplicate_field("run"));
                            }
                            run = Some(map.next_value()?);
                        }
                        "timeout_secs" => {
                            if timeout_secs.is_some() {
                                return Err(serde::de::Error::duplicate_field("timeout_secs"));
                            }
                            timeout_secs = Some(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["run", "timeout_secs"],
                            ));
                        }
                    }
                }
                let run = run.ok_or_else(|| serde::de::Error::missing_field("run"))?;
                Ok(BeforeAttempt::Run(BeforeAttemptCommand {
                    run,
                    timeout_secs,
                }))
            }
        }

        deserializer.deserialize_any(EntryVisitor)
    }
}

/// What a profile's `before_attempt` holds when the key is absent (ADR 0048 decision 10).
#[must_use]
pub fn default_before_attempt() -> Vec<BeforeAttempt> {
    vec![BeforeAttempt::Step(MERGE_MAIN.to_owned())]
}

/// One gate profile: the stages, the steps before an attempt, and the acceptance and
/// merge-candidate commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The stages the profile runs, in order.
    #[serde(default)]
    pub stages: Vec<StageSpec>,
    /// The steps the wrapper runs in the task's worktree before the worker starts, in order.
    /// Absent: [`default_before_attempt`].
    #[serde(default = "default_before_attempt")]
    pub before_attempt: Vec<BeforeAttempt>,
    /// Shell command lines the `commands` stage runs, in order.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Shell command lines the `integration` stage runs on the merge candidate, in order.
    #[serde(default)]
    pub key_commands: Vec<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            stages: Vec::new(),
            before_attempt: default_before_attempt(),
            commands: Vec::new(),
            key_commands: Vec::new(),
        }
    }
}

impl Profile {
    /// Rejects a profile that lists `integration`, lists one stage twice, lists `review`
    /// before `commands`, or lists a `before_attempt` entry this build does not run or a
    /// command with an empty `run` (the order and the entry list ADR 0048 decisions 7 and 10
    /// fix).
    ///
    /// # Errors
    ///
    /// [`GateError::UnlistedStage`], [`GateError::DuplicateStage`],
    /// [`GateError::StageOrder`], [`GateError::UnknownBeforeAttemptStep`] or
    /// [`GateError::EmptyBeforeAttemptRun`].
    pub fn validate(&self) -> Result<(), GateError> {
        for entry in &self.before_attempt {
            match entry {
                BeforeAttempt::Step(step) => {
                    if !BEFORE_ATTEMPT_STEPS.contains(&step.as_str()) {
                        return Err(GateError::UnknownBeforeAttemptStep(step.clone()));
                    }
                }
                BeforeAttempt::Run(command) => {
                    if command.run.trim().is_empty() {
                        return Err(GateError::EmptyBeforeAttemptRun);
                    }
                }
            }
        }
        let mut seen = Vec::with_capacity(self.stages.len());
        for spec in &self.stages {
            if !spec.stage.is_listed() {
                return Err(GateError::UnlistedStage(spec.stage));
            }
            if seen.contains(&spec.stage) {
                return Err(GateError::DuplicateStage(spec.stage));
            }
            seen.push(spec.stage);
        }
        // `review` reads the change the `commands` stage accepted: a profile that lists both
        // must run `commands` first (ADR 0048 decision 10).
        let commands = self
            .stages
            .iter()
            .position(|spec| spec.stage == Stage::Commands);
        let review = self
            .stages
            .iter()
            .position(|spec| spec.stage == Stage::Review);
        if let (Some(commands), Some(review)) = (commands, review)
            && review < commands
        {
            return Err(GateError::StageOrder {
                stage: Stage::Review,
                after: Stage::Commands,
            });
        }
        Ok(())
    }

    /// Whether the profile merges the remote main into the task's branch before an attempt.
    /// The one place the `merge-main` step is asked about: the wrapper runs it, and the review
    /// half of the state machine writes a conflict rework only when it is on (ADR 0048
    /// decisions 7 and 9).
    #[must_use]
    pub fn runs_merge_main(&self) -> bool {
        self.before_attempt
            .iter()
            .any(|entry| entry.step() == Some(MERGE_MAIN))
    }
}

/// The `[gate.review]` table: how the `review` stage runs its read-only session (ADR 0048
/// decision 10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReviewConfig {
    /// Extra `radian exec` arguments of the review session, after the stage's own. Unset:
    /// `[worker] exec_args`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_args: Option<Vec<String>>,
    /// A file appended to the built-in prompt, relative to the repository root. Unset: only
    /// the built-in prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
    /// Wall-time limit of one review session, in seconds. `0` waits forever.
    pub timeout_secs: u64,
    /// Whether arc proposes the plan's review follow-up task once the plan has no open task
    /// (ADR 0048 decision 10). `true` by default; `false` turns the follow-up off.
    pub propose_followups: bool,
    /// How many review sessions may run at once in the repository. `1` or more.
    pub concurrency: u32,
    /// The values a task's first review uses in place of the keys above (ADR 0048
    /// decision 10).
    pub first: FirstReviewConfig,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            exec_args: None,
            prompt_file: None,
            timeout_secs: DEFAULT_REVIEW_TIMEOUT_SECS,
            propose_followups: true,
            concurrency: DEFAULT_REVIEW_CONCURRENCY,
            first: FirstReviewConfig::default(),
        }
    }
}

/// `[gate.review.first]`: the two keys a task's first review may replace (ADR 0048
/// decision 10). An unset key falls back to the `[gate.review]` key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FirstReviewConfig {
    /// Extra `radian exec` arguments of a first review, in place of `[gate.review] exec_args`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_args: Option<Vec<String>>,
    /// Wall-time limit of a first review's session, in seconds. `1` or more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// The settings one review session runs with, and where each came from (ADR 0048
/// decision 10). [`ReviewConfig::choice`] fills it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewChoice<'a> {
    /// Whether this is a task's first review.
    pub first: bool,
    /// The arguments the session gets, after the stage's own.
    pub exec_args: &'a [String],
    /// The config key `exec_args` came from.
    pub exec_args_key: &'static str,
    /// The wall-time limit of one session, in seconds.
    pub timeout_secs: u64,
    /// The config key `timeout_secs` came from.
    pub timeout_key: &'static str,
}

impl ReviewConfig {
    /// The arguments the review session gets after its own: the configured list, else the
    /// worker's (`[gate.review] exec_args` defaults to `[worker] exec_args`).
    #[must_use]
    pub fn exec_args<'a>(&'a self, worker: &'a [String]) -> &'a [String] {
        self.exec_args.as_deref().unwrap_or(worker)
    }

    /// The wall-time limit as a [`Duration`]. Zero seconds means no timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    /// The settings a review session runs with (ADR 0048 decision 10): a first review reads
    /// `[gate.review.first]` first, and an unset key falls back to `[gate.review]` and then,
    /// for `exec_args`, to `[worker] exec_args`. A rework review never reads `first`.
    #[must_use]
    pub fn choice<'a>(&'a self, first: bool, worker: &'a [String]) -> ReviewChoice<'a> {
        let (exec_args, exec_args_key) = if first {
            match &self.first.exec_args {
                Some(args) => (args.as_slice(), "gate.review.first.exec_args"),
                None => self.fallback_exec_args(worker),
            }
        } else {
            self.fallback_exec_args(worker)
        };
        let (timeout_secs, timeout_key) = if first {
            match self.first.timeout_secs {
                Some(secs) => (secs, "gate.review.first.timeout_secs"),
                None => (self.timeout_secs, "gate.review.timeout_secs"),
            }
        } else {
            (self.timeout_secs, "gate.review.timeout_secs")
        };
        ReviewChoice {
            first,
            exec_args,
            exec_args_key,
            timeout_secs,
            timeout_key,
        }
    }

    fn fallback_exec_args<'a>(&'a self, worker: &'a [String]) -> (&'a [String], &'static str) {
        match &self.exec_args {
            Some(args) => (args.as_slice(), "gate.review.exec_args"),
            None => (worker, "worker.exec_args"),
        }
    }
}

/// The `[gate]` table: the selected profile, the command timeout and the profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    /// The name of the profile the repository uses.
    #[serde(default = "default_profile_name")]
    pub profile: String,
    /// Per-command timeout of `commands` and `integration`, in seconds. `0` waits forever.
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
    /// The profiles, by name.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// The `review` stage's session configuration.
    #[serde(default)]
    pub review: ReviewConfig,
}

fn default_profile_name() -> String {
    "default".to_string()
}

fn default_command_timeout_secs() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_SECS
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            profile: default_profile_name(),
            command_timeout_secs: DEFAULT_COMMAND_TIMEOUT_SECS,
            profiles: BTreeMap::new(),
            review: ReviewConfig::default(),
        }
    }
}

impl GateConfig {
    /// The profile named by [`GateConfig::profile`].
    #[must_use]
    pub fn selected_profile(&self) -> Option<&Profile> {
        self.profiles.get(&self.profile)
    }

    /// The per-command timeout as a [`Duration`]. Zero seconds means no timeout.
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout_secs)
    }
}

/// The stages a task runs: the profile's stages plus the task's extra stages.
///
/// Extra stages are added in their own order. A stage that is already present merges into
/// the existing entry and can only raise `required`; an extra stage can never remove or
/// weaken a profile stage. A stage in `extra` that is not listable (`integration`) is
/// ignored because it always runs at merge.
#[must_use]
pub fn effective_stages(profile: &Profile, extra: &[StageSpec]) -> Vec<StageSpec> {
    let mut stages: Vec<StageSpec> = Vec::with_capacity(profile.stages.len() + extra.len());
    for spec in &profile.stages {
        if spec.stage.is_listed() {
            stages.push(*spec);
        }
    }
    for spec in extra {
        if !spec.stage.is_listed() {
            continue;
        }
        match stages
            .iter_mut()
            .find(|existing| existing.stage == spec.stage)
        {
            Some(existing) => existing.required |= spec.required,
            None => stages.push(*spec),
        }
    }
    stages
}

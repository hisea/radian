//! The `[gate]` table of `config.toml` (ADR 0048 decisions 10 and 15).
//!
//! arc-store owns parsing `config.toml`; these types are the shape it hands to the gate. The
//! names of the profile fields (`stages`, `commands`, `key_commands`) are the file's names.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::error::GateError;

/// The default per-command timeout in seconds (the sample value of ADR 0048 decision 15).
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 3600;

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

/// One gate profile: the stages, the acceptance commands and the merge-candidate commands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The stages the profile runs, in order.
    #[serde(default)]
    pub stages: Vec<StageSpec>,
    /// Shell command lines the `commands` stage runs, in order.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Shell command lines the `integration` stage runs on the merge candidate, in order.
    #[serde(default)]
    pub key_commands: Vec<String>,
}

impl Profile {
    /// Rejects a profile that lists `integration` or lists one stage twice.
    ///
    /// # Errors
    ///
    /// [`GateError::UnlistedStage`] or [`GateError::DuplicateStage`].
    pub fn validate(&self) -> Result<(), GateError> {
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
        Ok(())
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

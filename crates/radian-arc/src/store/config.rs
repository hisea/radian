//! `<repository>/.radian/arc/config.toml` (ADR 0048 decision 15).
//!
//! The file is written once by `radian arc init` (a second `init` keeps it) and read by
//! every other subcommand. It has the shape of the radian configuration file (ADR 0008):
//! an explicit `schema_version`, rejects unknown keys, and every value set is one list the
//! parser and the manual's drift test both read.
//!
//! The gate keys are shared with the gate worker; they are not renamed here.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The configuration schema version this build writes and accepts.
pub const SCHEMA_VERSION: u32 = 1;

/// The stage names a gate profile may list, in the order the manual documents them.
/// `integration` is deliberately absent: it always runs at merge, so a profile cannot
/// select it (ADR 0048 decision 10).
pub const STAGE_NAMES: &[&str] = &["commands", "mutation", "review", "ci", "human"];

/// Keys whose default is "not set" rather than a value. The manual's table names each
/// one, and the drift test checks that it does.
pub const OPTIONAL_KEYS: &[&str] = &["limits.max_cost_usd"];

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
    pub gate: Gate,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repository: Repository::default(),
            limits: Limits::default(),
            gate: Gate::default(),
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
    /// How many attempts one task gets before it is `failed.exhausted`.
    pub max_attempts: u32,
    /// No new line on the worker's jsonl stream for this long is a stall.
    pub stall_timeout_secs: u64,
    /// An absolute limit on one attempt, on top of the stall timeout.
    pub max_wall_time_secs: u64,
    /// No limit when unset.
    pub max_cost_usd: Option<f64>,
    /// How long a cancel waits for the worker before SIGKILL.
    pub cancel_grace_secs: u64,
    /// How many automatic reworks one task may get before the plan pauses.
    pub max_auto_rework: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 1,
            max_attempts: 3,
            stall_timeout_secs: 900,
            max_wall_time_secs: 7200,
            max_cost_usd: None,
            cancel_grace_secs: 10,
            max_auto_rework: 2,
        }
    }
}

/// `[gate]`: which profile runs and how long one command may take.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Gate {
    /// The profile name in `profiles`.
    pub profile: String,
    /// The limit on one gate command line.
    pub command_timeout_secs: u64,
    /// `[gate.profiles.<name>]` sections.
    pub profiles: BTreeMap<String, Profile>,
}

impl Default for Gate {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert("default".to_owned(), Profile::default());
        Self {
            profile: "default".to_owned(),
            command_timeout_secs: 3600,
            profiles,
        }
    }
}

/// One `[gate.profiles.<name>]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Profile {
    /// The stages this profile runs, in order, each with the name from [`STAGE_NAMES`].
    pub stages: Vec<Stage>,
    /// Shell command lines for the `commands` stage, in order, on its own checkout.
    pub commands: Vec<String>,
    /// What the integration stage runs on the merge candidate.
    pub key_commands: Vec<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            stages: vec![
                Stage {
                    stage: "commands".to_owned(),
                    required: true,
                },
                Stage {
                    stage: "human".to_owned(),
                    required: true,
                },
            ],
            commands: Vec::new(),
            key_commands: Vec::new(),
        }
    }
}

/// One entry of a profile's `stages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    /// A [`STAGE_NAMES`] entry.
    pub stage: String,
    /// A required stage must pass for the task to reach `awaiting_human`.
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
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
# max_cost_usd = 5.0          # not set = no limit
cancel_grace_secs = 10
max_auto_rework = 2

[gate]
profile = "default"
command_timeout_secs = 3600

[gate.profiles.default]
stages = [{{ stage = "commands", required = true }}, {{ stage = "human", required = true }}]
commands = []                 # shell command lines, run in order on the gate checkout
key_commands = []             # what the integration stage runs on the merge candidate
"#
    )
}

/// Parses the file's text. Unknown keys, an unknown schema version, an unknown stage
/// name, and a `gate.profile` that names no profile are errors that name the offender.
pub fn parse(text: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(text).map_err(|error| error.to_string())?;
    if config.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {} (this build reads {SCHEMA_VERSION})",
            config.schema_version
        ));
    }
    for (name, profile) in &config.gate.profiles {
        for stage in &profile.stages {
            if !STAGE_NAMES.contains(&stage.stage.as_str()) {
                return Err(format!(
                    "gate profile `{name}` names unknown stage `{}`; stages are {}",
                    stage.stage,
                    STAGE_NAMES.join(", ")
                ));
            }
        }
    }
    if !config.gate.profiles.contains_key(&config.gate.profile) {
        return Err(format!(
            "gate.profile is `{}` but no [gate.profiles.{}] section exists",
            config.gate.profile, config.gate.profile
        ));
    }
    Ok(config)
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

    /// The text `init` writes parses, and it equals the code's defaults: a changed default
    /// must change the written file too.
    #[test]
    fn the_default_text_parses_to_the_code_defaults() {
        let parsed = parse(&default_text("main")).expect("the default text parses");
        assert_eq!(parsed, Config::default());
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
}

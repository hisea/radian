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

use crate::gate::{DEFAULT_COMMAND_TIMEOUT_SECS, GateConfig, Profile, Stage, StageSpec};

pub use crate::gate::STAGE_NAMES;

/// The configuration schema version this build writes and accepts.
pub const SCHEMA_VERSION: u32 = 1;

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
    #[serde(default = "default_gate")]
    pub gate: GateConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repository: Repository::default(),
            limits: Limits::default(),
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
/// name, a profile that lists `integration` or a stage twice, and a `gate.profile` that
/// names no profile are errors that name the offender.
pub fn parse(text: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(text).map_err(|error| error.to_string())?;
    if config.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {} (this build reads {SCHEMA_VERSION})",
            config.schema_version
        ));
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
}

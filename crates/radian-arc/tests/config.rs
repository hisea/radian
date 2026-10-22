use serde::Deserialize;

use radian_arc::gate::{GateConfig, Stage, StageSpec, effective_stages};

#[derive(Deserialize)]
struct File {
    gate: GateConfig,
}

const SAMPLE: &str = r#"
[gate]
profile = "default"
command_timeout_secs = 3600

[gate.profiles.default]
stages = [{ stage = "commands", required = true }, { stage = "human", required = true }]
commands = ["cargo fmt --all -- --check"]
key_commands = ["cargo test"]

[gate.profiles.radian]
stages = [
  { stage = "commands", required = true },
  { stage = "mutation", required = true },
  { stage = "review" },
  { stage = "human", required = true },
]
commands = []
key_commands = []
"#;

#[test]
fn the_gate_table_has_the_config_file_shape() {
    let file: File = toml::from_str(SAMPLE).unwrap();
    let gate = file.gate;
    assert_eq!(gate.profile, "default");
    assert_eq!(gate.command_timeout_secs, 3600);
    assert_eq!(gate.command_timeout(), std::time::Duration::from_secs(3600));

    let default = gate.selected_profile().expect("the selected profile");
    assert_eq!(
        default.stages,
        vec![
            StageSpec {
                stage: Stage::Commands,
                required: true,
            },
            StageSpec {
                stage: Stage::Human,
                required: true,
            },
        ]
    );
    assert_eq!(default.commands, vec!["cargo fmt --all -- --check"]);
    assert_eq!(default.key_commands, vec!["cargo test"]);
    assert!(default.validate().is_ok());

    let radian = &gate.profiles["radian"];
    assert_eq!(radian.stages.len(), 4);
    assert_eq!(radian.stages[0].stage, Stage::Commands);
    assert!(radian.stages[1].required, "mutation is required");
    assert!(!radian.stages[2].required, "review is optional");
    assert!(radian.validate().is_ok());
}

#[test]
fn the_gate_table_defaults_match_the_adr() {
    let gate: GateConfig = toml::from_str("").unwrap();
    assert_eq!(gate.profile, "default");
    assert_eq!(gate.command_timeout_secs, 3600);
    assert!(gate.profiles.is_empty());
    assert!(gate.selected_profile().is_none());
    assert!(GateConfig::default().selected_profile().is_none());

    let file: File = toml::from_str(
        "[gate]\n[gate.profiles.default]\nstages = [{ stage = \"commands\", required = true }]\n",
    )
    .unwrap();
    let default = file.gate.selected_profile().unwrap();
    assert!(default.stages[0].required);
    assert!(default.commands.is_empty());
    assert!(default.key_commands.is_empty());
}

#[test]
fn a_profile_cannot_list_integration_and_stage_names_are_a_closed_set() {
    let file: File = toml::from_str(
        "[gate.profiles.default]\nstages = [{ stage = \"integration\", required = true }]\n",
    )
    .unwrap();
    assert!(
        file.gate
            .profiles
            .get("default")
            .unwrap()
            .validate()
            .is_err(),
        "a profile listed integration"
    );

    let unknown: Result<File, _> =
        toml::from_str("[gate.profiles.default]\nstages = [{ stage = \"lint\" }]\n");
    assert!(unknown.is_err(), "an unknown stage name parsed");

    let profile = radian_arc::gate::Profile {
        stages: vec![StageSpec {
            stage: Stage::Commands,
            required: true,
        }],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    let stages = effective_stages(
        &profile,
        &[StageSpec {
            stage: Stage::Commands,
            required: false,
        }],
    );
    assert!(stages[0].required, "an extra stage weakened a required one");
}

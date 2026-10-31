use serde::Deserialize;

use radian_arc::gate::{
    BeforeAttempt, BeforeAttemptCommand, GateConfig, Profile, Stage, StageSpec, effective_stages,
};

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

[gate.profiles.radian]
stages = [
  { stage = "commands", required = true },
  { stage = "mutation", required = true },
  { stage = "review", required = false },
  { stage = "human", required = true },
]
commands = []
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
    assert!(
        default.key_commands.is_empty(),
        "the profile record no longer carries the key"
    );
    assert!(default.validate().is_ok());

    let radian = &gate.profiles["radian"];
    assert_eq!(radian.stages.len(), 4);
    assert_eq!(radian.stages[0].stage, Stage::Commands);
    assert!(radian.stages[1].required, "mutation is required");
    assert!(
        !radian.stages[2].required,
        "review is optional when it says so"
    );
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

/// A profile record written by an older arc still reads its `key_commands`, even though a
/// `config.toml` that writes one is refused: a merge of that attempt reads the record.
#[test]
fn a_legacy_profile_key_commands_still_reads() {
    let profile: Profile = toml::from_str("stages = []\nkey_commands = [\"false\"]\n")
        .expect("a legacy profile record reads");
    assert_eq!(profile.key_commands, vec!["false"]);
}

#[test]
fn an_omitted_required_is_true() {
    let file: File =
        toml::from_str("[gate.profiles.default]\nstages = [{ stage = \"ci\" }]\n").unwrap();
    assert!(
        file.gate.profiles["default"].stages[0].required,
        "a listed stage is required unless it says otherwise"
    );
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
        before_attempt: Vec::new(),
        commands: Vec::new(),
        key_commands: Vec::new(),
        reviewer: "default".to_owned(),
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

/// A base file whose only profile is valid, so a test can add one broken table.
const BASE: &str = "schema_version = 1\n\
[gate.profiles.default]\n\
stages = [{ stage = \"commands\", required = true }]\n";

/// A `before_attempt` step this build does not run is a config error that names it. The one
/// step and an empty list parse. Mutation: accept any step and the wrapper would skip a merge
/// it cannot know about.
#[test]
fn an_unknown_before_attempt_step_is_refused_by_name() {
    let error = radian_arc::store::config::parse(&format!("{BASE}before_attempt = [\"rebase\"]\n"))
        .expect_err("an unknown step must fail");
    assert!(error.contains("rebase"), "{error}");
    for value in ["[\"merge-main\"]", "[]"] {
        let config = radian_arc::store::config::parse(&format!("{BASE}before_attempt = {value}\n"))
            .expect("a valid step list parses");
        assert!(
            config
                .gate
                .profiles
                .get("default")
                .expect("the profile")
                .validate()
                .is_ok()
        );
    }
}

/// A `before_attempt` entry may be a command table (ADR 0048 decisions 7 and 10): `run` is
/// required and not empty, `timeout_secs` is optional, and any other key is refused by name.
#[test]
fn a_before_attempt_command_table_is_closed_and_needs_a_run() {
    for (value, needle) in [
        ("[{ run = \"\" }]", "empty"),
        ("[{ timeout_secs = 5 }]", "run"),
        ("[{ run = \"true\", shell = \"bash\" }]", "shell"),
    ] {
        let error = radian_arc::store::config::parse(&format!("{BASE}before_attempt = {value}\n"))
            .expect_err("a broken command table must fail");
        assert!(
            error.contains(needle),
            "the error names `{needle}`: {error}"
        );
    }

    let config = radian_arc::store::config::parse(&format!(
        "{BASE}before_attempt = [\"merge-main\", {{ run = \"cargo build --locked\", \
         timeout_secs = 1200 }}]\n"
    ))
    .expect("both entry forms parse");
    assert_eq!(
        config.gate.profiles["default"].before_attempt,
        vec![
            BeforeAttempt::Step("merge-main".to_owned()),
            BeforeAttempt::Run(BeforeAttemptCommand {
                run: "cargo build --locked".to_owned(),
                timeout_secs: Some(1200),
            }),
        ]
    );
    assert!(config.gate.profiles["default"].runs_merge_main());

    let without_timeout = radian_arc::store::config::parse(&format!(
        "{BASE}before_attempt = [{{ run = \"true\" }}]\n"
    ))
    .expect("`timeout_secs` is optional");
    assert_eq!(
        without_timeout.gate.profiles["default"].before_attempt,
        vec![BeforeAttempt::Run(BeforeAttemptCommand {
            run: "true".to_owned(),
            timeout_secs: None,
        })]
    );
}

/// `gate.review.exec_args` must be an array of strings, and a non-array value is refused by
/// name (not only by a TOML position).
#[test]
fn a_non_array_review_exec_args_is_refused_by_name() {
    for addition in [
        "[gate.review]\nexec_args = \"--model m\"\n",
        "[gate.review]\nexec_args = [1]\n",
        "[gate.review]\nexec_args = [true]\n",
    ] {
        let error = radian_arc::store::config::parse(&format!("{BASE}{addition}"))
            .expect_err("a non-array exec_args must fail");
        assert!(error.contains("gate.review.exec_args"), "{error}");
    }
}

/// `gate.review.first.exec_args` must be an array of strings, and an unknown key under
/// `[gate.review.first]` is refused like every other unknown config key.
#[test]
fn the_first_review_table_is_a_closed_table() {
    for addition in [
        "[gate.review.first]\nexec_args = \"--model m\"\n",
        "[gate.review.first]\nexec_args = [1]\n",
    ] {
        let error = radian_arc::store::config::parse(&format!("{BASE}{addition}"))
            .expect_err("a non-array exec_args must fail");
        assert!(error.contains("gate.review.first.exec_args"), "{error}");
    }
    let error =
        radian_arc::store::config::parse(&format!("{BASE}[gate.review.first]\nmodel = \"m\"\n"))
            .expect_err("an unknown key must fail");
    assert!(error.contains("model"), "{error}");
}

/// `gate.review.concurrency` is `1` or more and `gate.review.first.timeout_secs` is `1` or
/// more; `0` is refused by name.
#[test]
fn review_concurrency_and_first_timeout_are_positive() {
    for (addition, key) in [
        (
            "[gate.review]\nconcurrency = 0\n",
            "gate.review.concurrency",
        ),
        (
            "[gate.review.first]\ntimeout_secs = 0\n",
            "gate.review.first.timeout_secs",
        ),
    ] {
        let error = radian_arc::store::config::parse(&format!("{BASE}{addition}"))
            .expect_err("a zero must fail");
        assert!(error.contains(key), "the error names `{key}`: {error}");
    }
}

/// The first review's keys replace the same `[gate.review]` key when set, and each unset key
/// falls back: `exec_args` to `[gate.review] exec_args` and then `[worker] exec_args`;
/// `timeout_secs` to `[gate.review] timeout_secs`. A rework review never reads `first`.
#[test]
fn the_first_review_choice_replaces_each_key_and_falls_back() {
    let worker = vec!["--model".to_owned(), "worker".to_owned()];
    let mut review = radian_arc::gate::ReviewConfig::default();
    let choice = review.choice(true, &worker);
    assert!(choice.first);
    assert_eq!(choice.exec_args, worker.as_slice());
    assert_eq!(choice.exec_args_key, "worker.exec_args");
    assert_eq!(choice.timeout_secs, 900);
    assert_eq!(choice.timeout_key, "gate.review.timeout_secs");

    let review_args = vec!["--model".to_owned(), "review".to_owned()];
    review.exec_args = Some(review_args.clone());
    let choice = review.choice(true, &worker);
    assert_eq!(choice.exec_args, review_args.as_slice());
    assert_eq!(choice.exec_args_key, "gate.review.exec_args");
    review.exec_args = None;

    let first_args = vec!["--model".to_owned(), "first".to_owned()];
    review.first.exec_args = Some(first_args.clone());
    review.first.timeout_secs = Some(12);
    let choice = review.choice(true, &worker);
    assert_eq!(choice.exec_args, first_args.as_slice());
    assert_eq!(choice.exec_args_key, "gate.review.first.exec_args");
    assert_eq!(choice.timeout_secs, 12);
    assert_eq!(choice.timeout_key, "gate.review.first.timeout_secs");

    // A rework review ignores the whole `first` table.
    let choice = review.choice(false, &worker);
    assert!(!choice.first);
    assert_eq!(choice.exec_args, worker.as_slice());
    assert_eq!(choice.exec_args_key, "worker.exec_args");
    assert_eq!(choice.timeout_secs, 900);
    assert_eq!(choice.timeout_key, "gate.review.timeout_secs");
}

/// `review` reads the change `commands` accepted: a profile that lists both must list
/// `commands` first.
#[test]
fn review_before_commands_is_refused_by_name() {
    let text = "schema_version = 1\n[gate.profiles.default]\nstages = [\
                { stage = \"review\", required = true }, \
                { stage = \"commands\", required = true }]\n";
    let error = radian_arc::store::config::parse(text).expect_err("review before commands");
    assert!(error.contains("review"), "{error}");
    assert!(error.contains("commands"), "{error}");

    // `review` with no `commands` at all is a non-code flow's gate and is allowed.
    let text = "schema_version = 1\n[gate.profiles.default]\nstages = [\
                { stage = \"review\", required = true }, \
                { stage = \"human\", required = true }]\n";
    let config = radian_arc::store::config::parse(text).expect("review alone is allowed");
    assert_eq!(config.gate.review.timeout_secs, 900);
    assert!(config.gate.review.exec_args.is_none());
}

/// `gate.review.prompt_file` is read from the repository root: it must be relative and may
/// not climb out.
#[test]
fn a_prompt_file_outside_the_repository_is_refused() {
    for value in ["/etc/passwd", "../outside.md", ""] {
        let text = format!("{BASE}[gate.review]\nprompt_file = \"{value}\"\n");
        let error = radian_arc::store::config::parse(&text).expect_err("a bad prompt_file");
        assert!(error.contains("gate.review.prompt_file"), "{error}");
    }
    let text = format!("{BASE}[gate.review]\nprompt_file = \"docs/review.md\"\n");
    let config = radian_arc::store::config::parse(&text).expect("a relative file is allowed");
    assert_eq!(
        config.gate.review.prompt_file.as_deref(),
        Some("docs/review.md")
    );
}

/// The review table's defaults: the worker's `exec_args` until one is set, the built-in
/// prompt until a file is set, and 900 seconds.
#[test]
fn the_review_table_defaults_are_the_adr_values() {
    let config = radian_arc::store::config::parse(BASE).expect("the base file");
    let worker = vec!["--model".to_owned(), "m".to_owned()];
    assert_eq!(config.gate.review.exec_args(&worker), worker);
    assert_eq!(
        config.gate.review.timeout(),
        std::time::Duration::from_secs(900)
    );
    assert!(config.gate.review.prompt_file.is_none());

    let text = format!("{BASE}[gate.review]\nexec_args = []\ntimeout_secs = 0\n");
    let config = radian_arc::store::config::parse(&text).expect("an explicit table");
    assert!(
        config.gate.review.exec_args(&worker).is_empty(),
        "an explicit empty list wins over the worker's"
    );
    assert_eq!(
        config.gate.review.timeout(),
        std::time::Duration::ZERO,
        "0 waits forever"
    );
}

/// The config.toml the `init` of the previous build wrote, kept as a fixture: its review keys
/// are all in `[gate.review]`, and it must still load with the same effective settings.
const LEGACY_INIT: &str = r#"schema_version = 1

[repository]
remote = "origin"
main_branch = "main"          # what init found

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
# method = "push"             # `push`, `command` with a `command` line, or the `pr` gh preset
# command = "./merge.sh"     # the line `method = "command"` runs; refused with `push` and `pr`
# style = "merge"             # `merge`, `squash` or `rebase`: how the candidate is built

[hooks]
after_merge = [                # shell command lines run after a task reaches done.merged
  "git push origin --delete work/$ARC_TASK",
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
stages = [{ stage = "commands", required = true }, { stage = "human", required = true }]
before_attempt = ["merge-main"]   # steps the wrapper runs in the worktree before the worker; [] runs none
commands = []                 # shell command lines, run in order on the gate checkout
"#;

/// A file with only the legacy `[gate.review]` spellings gives the `default` reviewer their
/// values: `exec_args` from `[gate.review]`, `first.timeout_secs` from `[gate.review.first]`,
/// and the source keys still name the legacy table.
#[test]
fn the_legacy_review_table_is_the_default_reviewers_values() {
    let worker = vec!["--model".to_owned(), "worker".to_owned()];
    let text = format!(
        "{BASE}[gate.review]\nexec_args = [\"--model\", \"legacy\"]\n\
         [gate.review.first]\ntimeout_secs = 12\n"
    );
    let config = radian_arc::store::config::parse(&text).expect("the legacy file parses");
    assert!(!config.gate.reviewers.is_empty());
    let reviewer = &config.gate.reviewers["default"];
    assert!(
        reviewer.legacy,
        "the default reviewer came from [gate.review]"
    );
    assert_eq!(
        reviewer.exec_args.as_deref(),
        Some(["--model".to_owned(), "legacy".to_owned()].as_slice())
    );
    assert_eq!(reviewer.first.timeout_secs, Some(12));
    assert_eq!(reviewer.fix_level, "major");

    let choice = reviewer.choice("default", true, &worker);
    assert_eq!(choice.exec_args_key, "gate.review.exec_args");
    assert_eq!(choice.timeout_secs, 12);
    assert_eq!(choice.timeout_key, "gate.review.first.timeout_secs");
    let choice = reviewer.choice("default", false, &worker);
    assert_eq!(choice.exec_args_key, "gate.review.exec_args");
    assert_eq!(choice.timeout_key, "gate.review.timeout_secs");
    assert_eq!(choice.timeout_secs, 900);
}

/// The legacy spelling describes the `default` reviewer whenever the file has no
/// `[gate.reviewers.default]` section, even when another reviewer's section is present.
#[test]
fn legacy_review_keys_beside_another_reviewer_still_describe_the_default() {
    let worker = vec!["--model".to_owned(), "worker".to_owned()];
    let text = format!(
        "{BASE}[gate.reviewers.cheap]\ntimeout_secs = 60\n\
         [gate.review]\nexec_args = [\"--model\", \"legacy\"]\n\
         [gate.review.first]\ntimeout_secs = 12\n"
    );
    let config = radian_arc::store::config::parse(&text).expect("the file parses");
    assert_eq!(config.gate.reviewers["cheap"].timeout_secs, 60);
    let reviewer = &config.gate.reviewers["default"];
    assert!(
        reviewer.legacy,
        "the default reviewer came from [gate.review]"
    );
    assert_eq!(
        reviewer.exec_args.as_deref(),
        Some(["--model".to_owned(), "legacy".to_owned()].as_slice())
    );
    assert_eq!(reviewer.first.timeout_secs, Some(12));

    let choice = reviewer.choice("default", true, &worker);
    assert_eq!(choice.exec_args_key, "gate.review.exec_args");
    assert_eq!(choice.timeout_key, "gate.review.first.timeout_secs");
}

/// `init` writes `[gate.reviewers.default]`, and a file the previous `init` wrote still loads
/// with the same effective review settings.
#[test]
fn init_writes_the_default_reviewer_and_yesterdays_file_still_loads() {
    let text = radian_arc::store::config::default_text("main");
    assert!(
        text.contains("[gate.reviewers.default]"),
        "init names the reviewer table:\n{text}"
    );
    assert!(
        text.contains("# [gate.reviewers.default.first]"),
        "init writes the first-review block commented out:\n{text}"
    );
    assert!(
        !text.contains("\n[gate.review.first]"),
        "the legacy first-review table is gone from init:\n{text}"
    );
    assert!(
        !text.contains("[gate.review]\nexec_args"),
        "the legacy review keys are gone from init:\n{text}"
    );
    let fresh = radian_arc::store::config::parse(&text).expect("the init text parses");
    let legacy = radian_arc::store::config::parse(LEGACY_INIT).expect("yesterday's text parses");
    let worker = vec!["--model".to_owned(), "worker".to_owned()];
    for config in [&fresh, &legacy] {
        let reviewer = config
            .gate
            .reviewers
            .get("default")
            .expect("the default reviewer exists");
        assert_eq!(reviewer.exec_args, None);
        assert_eq!(reviewer.timeout_secs, 900);
        assert_eq!(reviewer.prompt_file, None);
        assert_eq!(reviewer.fix_level, "major");
        let choice = reviewer.choice("default", true, &worker);
        assert_eq!(choice.exec_args, worker.as_slice());
        assert_eq!(choice.timeout_secs, 900);
    }
    // The two files resolve the same values; only the source keys differ, because one names
    // the reviewer table and the other the legacy spelling it came from.
    let fresh_choice = fresh.gate.reviewers["default"].choice("default", false, &worker);
    let legacy_choice = legacy.gate.reviewers["default"].choice("default", false, &worker);
    assert_eq!(fresh_choice.timeout_secs, legacy_choice.timeout_secs);
    assert_eq!(
        fresh_choice.timeout_key,
        "gate.reviewers.default.timeout_secs"
    );
    assert_eq!(legacy_choice.timeout_key, "gate.review.timeout_secs");
}

/// A `[gate.reviewers.default]` section and a legacy `[gate.review]` session key are one
/// setting in two places: the file is refused and names both keys. Every legacy session key
/// the parser refuses has a case here.
#[test]
fn a_legacy_review_key_beside_the_default_reviewer_is_refused_by_both_names() {
    for (addition, keys) in [
        (
            "[gate.review]\ntimeout_secs = 10\n[gate.reviewers.default]\ntimeout_secs = 20\n",
            vec![
                "gate.review.timeout_secs",
                "gate.reviewers.default.timeout_secs",
            ],
        ),
        (
            "[gate.review]\nexec_args = []\n[gate.reviewers.default]\n",
            vec!["gate.review.exec_args", "gate.reviewers.default.exec_args"],
        ),
        (
            "[gate.review]\nprompt_file = \"review.md\"\n[gate.reviewers.default]\n",
            vec![
                "gate.review.prompt_file",
                "gate.reviewers.default.prompt_file",
            ],
        ),
        (
            "[gate.review.first]\nexec_args = []\n[gate.reviewers.default]\n",
            vec![
                "gate.review.first.exec_args",
                "gate.reviewers.default.first.exec_args",
            ],
        ),
        (
            "[gate.review.first]\ntimeout_secs = 10\n[gate.reviewers.default]\n",
            vec![
                "gate.review.first.timeout_secs",
                "gate.reviewers.default.first.timeout_secs",
            ],
        ),
    ] {
        let error = radian_arc::store::config::parse(&format!("{BASE}{addition}"))
            .expect_err("the two spellings conflict");
        for key in keys {
            assert!(error.contains(key), "the error names `{key}`: {error}");
        }
    }
}

/// A profile's `reviewer` must name a `[gate.reviewers.<name>]` section: a missing one is
/// refused by the profile key and the reviewer name.
#[test]
fn a_profile_names_a_missing_reviewer_is_refused() {
    let text = format!("{BASE}[gate.profiles.art]\nreviewer = \"cheap\"\n");
    let error = radian_arc::store::config::parse(&text).expect_err("no cheap reviewer");
    assert!(error.contains("gate.profiles.art.reviewer"), "{error}");
    assert!(error.contains("cheap"), "{error}");

    let text = format!(
        "{BASE}[gate.reviewers.cheap]\ntimeout_secs = 60\n\
         [gate.profiles.art]\nreviewer = \"cheap\"\n"
    );
    radian_arc::store::config::parse(&text).expect("a named reviewer resolves");
}

/// `fix_level` is one of `REVIEW_LEVELS`: a value outside it is refused with all three.
#[test]
fn a_fix_level_outside_the_review_levels_is_refused_with_all_of_them() {
    let text = format!("{BASE}[gate.reviewers.strict]\nfix_level = \"blocker\"\n");
    let error = radian_arc::store::config::parse(&text).expect_err("blocker is not a level");
    assert!(error.contains("gate.reviewers.strict.fix_level"), "{error}");
    for level in radian_arc::gate::REVIEW_LEVELS {
        assert!(
            error.contains(&format!("`{level}`")),
            "the error lists `{level}`: {error}"
        );
    }
    let text = format!("{BASE}[gate.reviewers.strict]\nfix_level = \"minor\"\n");
    radian_arc::store::config::parse(&text).expect("a listed level parses");
}

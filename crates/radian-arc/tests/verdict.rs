use radian_arc::gate::{
    Profile, Stage, StageSpec, Verdict, VerdictResult, effective_stages, required_passed,
    valid_verdicts, verdict_set_digest,
};

fn verdict(stage: Stage, sha: &str, result: VerdictResult, ended_at: &str) -> Verdict {
    Verdict {
        schema_version: 1,
        stage,
        sha: sha.to_string(),
        result,
        reason: None,
        commands: Vec::new(),
        started_at: "2026-10-22T00:00:00.000000Z".to_string(),
        ended_at: ended_at.to_string(),
        base_sha: None,
        candidate_sha: None,
        conflict_paths: Vec::new(),
        approver: None,
    }
}

fn required(stage: Stage) -> StageSpec {
    StageSpec {
        stage,
        required: true,
    }
}

fn optional(stage: Stage) -> StageSpec {
    StageSpec {
        stage,
        required: false,
    }
}

#[test]
fn a_verdict_for_an_old_sha_does_not_count_after_the_branch_moves() {
    let profile = Profile {
        stages: vec![required(Stage::Commands), required(Stage::Human)],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    let old = verdict(
        Stage::Commands,
        "aaaa",
        VerdictResult::Pass,
        "2026-10-22T01:00:00.000000Z",
    );
    let old_human = verdict(
        Stage::Human,
        "aaaa",
        VerdictResult::Pass,
        "2026-10-22T01:01:00.000000Z",
    );
    let new = verdict(
        Stage::Commands,
        "bbbb",
        VerdictResult::Pass,
        "2026-10-22T02:00:00.000000Z",
    );
    let verdicts = vec![old, old_human, new];

    let valid = valid_verdicts(&verdicts, "bbbb");
    assert_eq!(valid.len(), 1);
    assert!(valid.iter().all(|verdict| verdict.sha == "bbbb"));
    assert_eq!(valid_verdicts(&verdicts, "cccc").len(), 0);

    assert!(required_passed(&profile, &[], &verdicts, "aaaa"));
    assert!(!required_passed(&profile, &[], &verdicts, "bbbb"));
    assert!(!required_passed(&profile, &[], &verdicts, "cccc"));
}

#[test]
fn extra_stages_cannot_drop_a_required_stage() {
    let profile = Profile {
        stages: vec![required(Stage::Commands), required(Stage::Human)],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    let extra = vec![optional(Stage::Commands), optional(Stage::Review)];

    let stages = effective_stages(&profile, &extra);
    assert_eq!(
        stages,
        vec![
            required(Stage::Commands),
            required(Stage::Human),
            optional(Stage::Review),
        ],
        "an extra stage removed or weakened a profile stage"
    );

    // `integration` is not a listable stage; it always runs at merge and is ignored here.
    let stages = effective_stages(&profile, &[optional(Stage::Integration)]);
    assert_eq!(stages, profile.stages);

    // A task may turn an optional stage into a required one.
    let optional_human = Profile {
        stages: vec![required(Stage::Commands), optional(Stage::Human)],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    let stages = effective_stages(&optional_human, &[required(Stage::Human)]);
    assert_eq!(stages[1], required(Stage::Human));

    assert!(profile.validate().is_ok());
    let invalid = Profile {
        stages: vec![required(Stage::Integration)],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    assert!(invalid.validate().is_err());
    let duplicate = Profile {
        stages: vec![required(Stage::Review), optional(Stage::Review)],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    assert!(duplicate.validate().is_err());
}

#[test]
fn a_newer_attempt_decides_whether_a_required_stage_passed() {
    let profile = Profile {
        stages: vec![required(Stage::Commands), required(Stage::Human)],
        commands: Vec::new(),
        key_commands: Vec::new(),
    };
    let sha = "aaaa";
    let human = verdict(
        Stage::Human,
        sha,
        VerdictResult::Pass,
        "2026-10-22T01:00:00.000000Z",
    );
    let old_pass = verdict(
        Stage::Commands,
        sha,
        VerdictResult::Pass,
        "2026-10-22T01:00:00.000000Z",
    );
    let new_fail = verdict(
        Stage::Commands,
        sha,
        VerdictResult::Fail,
        "2026-10-22T02:00:00.000000Z",
    );

    assert!(
        !required_passed(
            &profile,
            &[],
            &[old_pass.clone(), human.clone(), new_fail.clone()],
            sha
        ),
        "a newer failing attempt was counted as a pass"
    );
    assert!(
        !required_passed(
            &profile,
            &[],
            &[new_fail.clone(), human.clone(), old_pass.clone()],
            sha
        ),
        "the check depends on the order the verdicts are passed in"
    );

    // The other way round: an older failure, a newer pass.
    let new_pass = verdict(
        Stage::Commands,
        sha,
        VerdictResult::Pass,
        "2026-10-22T03:00:00.000000Z",
    );
    assert!(
        required_passed(
            &profile,
            &[],
            &[new_fail, human.clone(), new_pass.clone()],
            sha
        ),
        "a newer passing attempt did not count"
    );
    assert!(
        required_passed(&profile, &[], &[new_pass, human.clone()], sha),
        "one passing verdict per required stage must be enough"
    );
    assert!(
        !required_passed(&profile, &[], &[human], sha),
        "the required commands stage passed without a verdict"
    );
}

#[test]
fn the_digest_changes_when_a_verdict_of_the_same_sha_is_replaced_by_a_new_attempt() {
    let sha = "aaaa";
    let first = verdict(
        Stage::Commands,
        sha,
        VerdictResult::Pass,
        "2026-10-22T01:00:00.000000Z",
    );
    let second = verdict(
        Stage::Commands,
        sha,
        VerdictResult::Fail,
        "2026-10-22T02:00:00.000000Z",
    );
    let human = verdict(
        Stage::Human,
        sha,
        VerdictResult::Pass,
        "2026-10-22T01:30:00.000000Z",
    );

    let newest = verdict_set_digest(&[second.clone(), human.clone()], sha);
    assert_eq!(
        verdict_set_digest(&[first.clone(), second.clone(), human.clone()], sha),
        newest,
        "the newest verdict of a stage must count, not both"
    );
    assert_ne!(
        verdict_set_digest(&[first, human.clone()], sha),
        newest,
        "a new attempt's verdict did not change the digest"
    );

    let other_sha = verdict(
        Stage::Commands,
        "bbbb",
        VerdictResult::Pass,
        "2026-10-22T03:00:00.000000Z",
    );
    assert_eq!(
        verdict_set_digest(&[second.clone(), human.clone(), other_sha], sha),
        newest,
        "a verdict for another sha changed the digest"
    );
}

#[test]
fn the_digest_is_stable_under_reordering() {
    let sha = "aaaa";
    let commands = verdict(
        Stage::Commands,
        sha,
        VerdictResult::Pass,
        "2026-10-22T01:00:00.000000Z",
    );
    let human = verdict(
        Stage::Human,
        sha,
        VerdictResult::Pass,
        "2026-10-22T02:00:00.000000Z",
    );
    assert_eq!(
        verdict_set_digest(&[commands.clone(), human.clone()], sha),
        verdict_set_digest(&[human, commands], sha)
    );
}

#[test]
fn a_human_verdict_is_the_owner_record() {
    let pass = Verdict::human("aaaa", true, None);
    assert_eq!(pass.stage, Stage::Human);
    assert!(pass.is_pass());
    assert_eq!(pass.sha, "aaaa");
    assert!(pass.commands.is_empty());
    assert!(pass.reason.is_none());

    let fail = Verdict::human("aaaa", false, Some("the tests are not enough".to_string()));
    assert_eq!(fail.result, VerdictResult::Fail);
    assert_eq!(fail.reason.as_deref(), Some("the tests are not enough"));
}

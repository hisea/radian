//! A task's gate profile is its flow's `gate_profile` (ADR 0048 decisions 10 and 15).
//!
//! The gate tests use `TestRepo` and drive the library against a temp repository with a bare
//! `origin`; the review stage runs a fake `radian exec`, so no model is needed. The
//! `before_attempt` test drives the real `radian` wrapper with a test-only worker.

mod common;

use std::fs;

use common::*;
use radian_arc::gate::{Stage, Verdict, verdict_path};
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};
use serde_json::{Value, json};

fn setup(label: &str) -> (TestRepo, Store) {
    let repo = TestRepo::new(label);
    Store::init(&repo.repo, false).expect("arc init");
    let store = Store::open(&repo.repo).expect("the store opens");
    (repo, store)
}

fn append_config(store: &Store, text: &str) {
    let path = store.root().join("config.toml");
    let mut config = fs::read_to_string(&path).expect("config.toml");
    config.push('\n');
    config.push_str(text);
    fs::write(&path, config).expect("write config.toml");
}

/// Replaces one piece of `config.toml`.
fn tune(store: &Store, from: &str, to: &str) {
    let path = store.root().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// Commits `file` on `work/<task>`, pushes the branch and returns the commit.
fn publish(repo: &TestRepo, task: &str, base: &str, file: &str, content: &str) -> String {
    let branch = format!("work/{task}");
    let head = repo.commit_on(&branch, base, file, content);
    repo.git(&["push", "origin", &branch]);
    head
}

/// Puts the task in `review.verifying` for `head`, the way a pushed attempt leaves it.
fn set_verifying(store: &Store, task: &str, head: &str, base: &str) {
    store
        .change_task(task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.token = Some("token-1".to_owned());
            task.base_sha = Some(base.to_owned());
            task.head_sha = Some(head.to_owned());
            Ok(())
        })
        .expect("the task is in review.verifying");
}

/// The file names in the first attempt's verdict directory, sorted.
fn verdict_names(store: &Store, task: &str) -> Vec<String> {
    let dir = store
        .root()
        .join("runs")
        .join(task)
        .join("attempts")
        .join(format!("{task}-1"))
        .join("verdicts");
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// One flow's profile has `commands`, the other's has `review` instead. Each task's gate
/// runs only its own profile's stages, and every verdict names that profile. The image task
/// then merges, so the approval's required-stage check and the integration stage read the
/// same profile: its `key_commands` runs, which the default profile does not have.
#[test]
fn each_flow_gates_with_its_own_profile() {
    let (repo, store) = setup("gate-profile-two-flows");
    append_config(
        &store,
        "[gate.profiles.image]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n\
         key_commands = [\"true\"]\n\
         before_attempt = []\n\
         \n\
         [flows.image]\n\
         gate_profile = \"image\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let plain = store
        .add_task(
            &plan,
            "plain",
            "Plain",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
        )
        .expect("add the plain task");
    let image = store
        .add_task_with_flow(
            &plan,
            "image",
            "Image",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("image"),
        )
        .expect("add the image task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.rev("main");
    let plain_head = publish(&repo, &plain, &base, "plain.txt", "plain\n");
    let image_head = publish(&repo, &image, &base, "image.txt", "image\n");
    set_verifying(&store, &plain, &plain_head, &base);
    set_verifying(&store, &image, &image_head, &base);

    let plain_verified = review::verify(&store, &plain).expect("the plain gate runs");
    assert_eq!(plain_verified.phase, "awaiting_human", "{plain_verified:?}");
    assert_eq!(
        verdict_names(&store, &plain),
        vec!["commands.json".to_owned()],
        "the default profile runs `commands`, not `review`"
    );
    let commands = Verdict::read(
        &verdict_path(store.repo(), &plain, &format!("{plain}-1"), Stage::Commands)
            .expect("the commands verdict path"),
    )
    .expect("the commands verdict reads");
    assert_eq!(commands.profile.as_deref(), Some("default"));

    let fake = FakeReview::new(
        "gate-profile-image",
        &[review_message("the image change is fine", &[], &[])],
        0,
    );
    let image_verified =
        review::verify_with(&store, &image, Some(fake.program())).expect("the image gate runs");
    assert_eq!(image_verified.phase, "awaiting_human", "{image_verified:?}");
    assert_eq!(
        verdict_names(&store, &image),
        vec!["review.json".to_owned()],
        "the image profile runs `review`, not `commands`"
    );
    let review_verdict = Verdict::read(
        &verdict_path(store.repo(), &image, &format!("{image}-1"), Stage::Review)
            .expect("the review verdict path"),
    )
    .expect("the review verdict reads");
    assert_eq!(review_verdict.profile.as_deref(), Some("image"));
    // `show` says which profile the task's gate ran, from the verdict that records it.
    let shown = radian_arc::store::cli::show_text(&store, &image).expect("show");
    assert!(shown.contains("gate_profile: image"), "{shown}");
    let shown_json = radian_arc::store::cli::show_json(&store, &image).expect("show --json");
    assert_eq!(shown_json["gate_profile"], json!("image"));

    let merged = review::merge(&store, &image).expect("the image task merges");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    let integration = Verdict::read(
        &verdict_path(
            store.repo(),
            &image,
            &format!("{image}-1"),
            Stage::Integration,
        )
        .expect("the integration verdict path"),
    )
    .expect("the integration verdict reads");
    assert!(integration.is_pass(), "{:?}", integration.reason);
    assert_eq!(integration.profile.as_deref(), Some("image"));
    assert_eq!(
        integration.commands.len(),
        1,
        "the image profile's `key_commands` ran on the merge candidate"
    );
}

/// The background integration command reads the task's profile too: it runs the image
/// profile's `key_commands`, not the default profile's empty list.
#[test]
fn the_background_integration_uses_the_tasks_profile() {
    let (repo, store) = setup("gate-profile-integration");
    append_config(
        &store,
        "[gate.profiles.image]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n\
         key_commands = [\"true\"]\n\
         \n\
         [flows.image]\n\
         gate_profile = \"image\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let image = store
        .add_task_with_flow(
            &plan,
            "image",
            "Image",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("image"),
        )
        .expect("add the image task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.rev("main");
    let head = publish(&repo, &image, &base, "image.txt", "image\n");
    set_verifying(&store, &image, &head, &base);
    let fake = FakeReview::new(
        "gate-profile-image-integration",
        &[review_message("the image change is fine", &[], &[])],
        0,
    );
    review::verify_with(&store, &image, Some(fake.program())).expect("the image gate runs");
    store
        .change_task(&image, |task| {
            task.phase = "integrating".to_owned();
            Ok(())
        })
        .expect("the task is integrating");

    let label = review::gate_integration(&store, &image).expect("the background integration runs");
    assert_eq!(label, format!("{image}-1"));
    let integration = Verdict::read(
        &verdict_path(store.repo(), &image, &label, Stage::Integration)
            .expect("the integration verdict path"),
    )
    .expect("the integration verdict reads");
    assert!(integration.is_pass(), "{:?}", integration.reason);
    assert_eq!(integration.profile.as_deref(), Some("image"));
    assert_eq!(integration.commands.len(), 1);
}

/// The background integration reads the profile the attempt's verification ran under, not the
/// flow's profile after a change: the started gate's `key_commands` run on the candidate.
#[test]
fn a_background_integration_keeps_the_verifications_profile() {
    let (repo, store) = setup("gate-profile-integration-changed");
    append_config(
        &store,
        "[gate.profiles.image]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n\
         key_commands = [\"true\"]\n\
         \n\
         [gate.profiles.after]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n\
         \n\
         [flows.image]\n\
         gate_profile = \"image\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let image = store
        .add_task_with_flow(
            &plan,
            "image",
            "Image",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("image"),
        )
        .expect("add the image task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.rev("main");
    let head = publish(&repo, &image, &base, "image.txt", "image\n");
    set_verifying(&store, &image, &head, &base);
    let fake = FakeReview::new(
        "gate-profile-image-integration-changed",
        &[review_message("the image change is fine", &[], &[])],
        0,
    );
    review::verify_with(&store, &image, Some(fake.program())).expect("the image gate runs");

    tune(
        &store,
        "gate_profile = \"image\"",
        "gate_profile = \"after\"",
    );
    store
        .change_task(&image, |task| {
            task.phase = "integrating".to_owned();
            Ok(())
        })
        .expect("the task is integrating");
    let label = review::gate_integration(&store, &image).expect("the background integration runs");
    let integration = Verdict::read(
        &verdict_path(store.repo(), &image, &label, Stage::Integration)
            .expect("the integration verdict path"),
    )
    .expect("the integration verdict reads");
    assert!(integration.is_pass(), "{:?}", integration.reason);
    assert_eq!(integration.profile.as_deref(), Some("image"));
    assert_eq!(integration.commands.len(), 1);
}

/// A repository with no `[flows]` table keeps gating with `gate.profile`, including a
/// profile that is not `default`.
#[test]
fn a_repository_without_flows_keeps_gate_profile() {
    let (repo, store) = setup("gate-profile-no-flows");
    append_config(
        &store,
        "[gate.profiles.strict]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n",
    );
    tune(&store, "profile = \"default\"", "profile = \"strict\"");
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task(
            &plan,
            "one",
            "One",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
        )
        .expect("add a task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.rev("main");
    let head = publish(&repo, &task, &base, "one.txt", "one\n");
    set_verifying(&store, &task, &head, &base);

    let fake = FakeReview::new(
        "gate-profile-strict",
        &[review_message("the change is fine", &[], &[])],
        0,
    );
    let verified = review::verify_with(&store, &task, Some(fake.program()))
        .expect("the gate runs `gate.profile`");
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    assert_eq!(verdict_names(&store, &task), vec!["review.json".to_owned()]);
    let verdict = Verdict::read(
        &verdict_path(store.repo(), &task, &format!("{task}-1"), Stage::Review)
            .expect("the review verdict path"),
    )
    .expect("the review verdict reads");
    assert_eq!(verdict.profile.as_deref(), Some("strict"));
}

/// A profile removed after a task's gate started is the missing-profile case: the gate's
/// planning fails with the config error, which names the flow and its `gate_profile`.
#[test]
fn a_removed_profile_fails_the_gate_naming_the_flow() {
    let (repo, store) = setup("gate-profile-removed");
    append_config(
        &store,
        "[gate.profiles.art]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         \n\
         [flows.art]\n\
         gate_profile = \"art\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task_with_flow(
            &plan,
            "art",
            "Art",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("art"),
        )
        .expect("add the art task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.rev("main");
    let head = publish(&repo, &task, &base, "art.txt", "art\n");
    set_verifying(&store, &task, &head, &base);

    tune(
        &store,
        "[gate.profiles.art]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n",
        "",
    );
    let error = review::verify_with(&store, &task, None).expect_err("the profile is gone");
    let text = error.message();
    assert!(text.contains("flows.art.gate_profile"), "{text}");
    assert!(text.contains("art"), "{text}");
    assert!(text.contains("section exists"), "{text}");
}

/// The flow itself can be removed after the task's gate started. The gate then has no profile
/// to run and says which flow it could not resolve instead of falling back to `gate.profile`.
#[test]
fn a_removed_flow_fails_the_gate_naming_the_flow() {
    let (repo, store) = setup("gate-profile-flow-removed");
    append_config(&store, "[flows.art]\n");
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task_with_flow(
            &plan,
            "art",
            "Art",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("art"),
        )
        .expect("add the art task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.rev("main");
    let head = publish(&repo, &task, &base, "art.txt", "art\n");
    set_verifying(&store, &task, &head, &base);

    tune(&store, "[flows.art]\n", "");
    let error = review::verify_with(&store, &task, None).expect_err("the flow is gone");
    let text = error.message();
    assert!(text.contains(&task), "{text}");
    assert!(text.contains("art"), "{text}");
    assert!(text.contains("does not define"), "{text}");
}

/// A gate that already started keeps the profile it started with when `config.toml` changes
/// before the pass that applies its verdicts: the pass judges the verdicts by the profile that
/// produced them, not by the one the flow names now.
#[test]
fn a_started_gate_keeps_the_profile_it_ran_under() {
    let repo = Repo::new("gate-profile-started");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    append_config(
        &store,
        "[gate.profiles.running]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         commands = [\"true\"]\n\
         \n\
         [gate.profiles.after]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n\
         \n\
         [flows.art]\n\
         gate_profile = \"running\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task_with_flow(
            &plan,
            "art",
            "Art",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("art"),
        )
        .expect("add the art task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("art.txt"), "art\n");
    repo.git(&["add", "art.txt"]);
    repo.git(&["commit", "-m", "art"]);
    repo.git(&["push", "origin", &branch]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    set_verifying(&store, &task, &head, &base);

    // The gate runs to completion and leaves its `done` record with the profile it planned.
    let gate = repo.radian(&["arc", "gate", "--task", &task]);
    assert_eq!(gate.code, 0, "gate: {}", gate.stderr);
    let record: Value = serde_json::from_str(
        &fs::read_to_string(store.root().join("runs").join(&task).join("gate.json"))
            .expect("the gate record"),
    )
    .expect("gate.json reads");
    assert_eq!(record["status"], "done");
    assert_eq!(record["profile"]["name"], "running");
    assert_eq!(
        verdict_names(&store, &task),
        vec!["commands.json".to_owned()]
    );

    // The flow now names a `review`-only profile, before the pass applies the verdicts.
    tune(
        &store,
        "gate_profile = \"running\"",
        "gate_profile = \"after\"",
    );
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    let task_now = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (task_now.state.as_str(), task_now.phase.as_str()),
        ("review", "awaiting_human"),
        "the applied gate: {run:?}"
    );
    assert_eq!(
        verdict_names(&store, &task),
        vec!["commands.json".to_owned()],
        "the gate ran again under the new profile"
    );
}

/// A flow removed after the task's gate started still applies the verdicts: the configuration
/// stays valid (the implicit `default` flow is added), and the pass judges the completed gate
/// by the profile it recorded, not by a flow that no longer exists.
#[test]
fn a_flow_removed_after_the_gate_started_still_applies_its_verdicts() {
    let repo = Repo::new("gate-profile-flow-removed-after-start");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    append_config(
        &store,
        "[gate.profiles.art]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         commands = [\"true\"]\n\
         \n\
         [flows.art]\n\
         gate_profile = \"art\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task_with_flow(
            &plan,
            "art",
            "Art",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("art"),
        )
        .expect("add the art task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("art.txt"), "art\n");
    repo.git(&["add", "art.txt"]);
    repo.git(&["commit", "-m", "art"]);
    repo.git(&["push", "origin", &branch]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    set_verifying(&store, &task, &head, &base);

    // The gate runs to completion under `flows.art` and records its profile.
    let gate = repo.radian(&["arc", "gate", "--task", &task]);
    assert_eq!(gate.code, 0, "gate: {}", gate.stderr);
    let record: Value = serde_json::from_str(
        &fs::read_to_string(store.root().join("runs").join(&task).join("gate.json"))
            .expect("the gate record"),
    )
    .expect("gate.json reads");
    assert_eq!(record["status"], "done");
    assert_eq!(record["profile"]["name"], "art");

    // The flow is gone before the pass applies the verdicts. The file is valid: the implicit
    // `default` flow replaces it.
    tune(&store, "[flows.art]\ngate_profile = \"art\"\n", "");
    assert!(
        !fs::read_to_string(store.root().join("config.toml"))
            .expect("config.toml")
            .contains("[flows.art]")
    );
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    let task_now = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (task_now.state.as_str(), task_now.phase.as_str()),
        ("review", "awaiting_human"),
        "the applied gate: {run:?}"
    );
    assert_eq!(
        verdict_names(&store, &task),
        vec!["commands.json".to_owned()],
        "the gate ran again under the implicit default flow"
    );
}

/// A flow whose `gate_profile` changes after a completed gate is applied still merges that
/// gate's verdicts: the acceptance and the integration run with the profile the verification
/// ran under, not the one the flow names now (ADR 0048 decisions 10 and 15).
#[test]
fn a_changed_flow_profile_still_merges_the_started_gates_verdicts() {
    let repo = Repo::new("gate-profile-changed-after-apply");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    append_config(
        &store,
        "[gate.profiles.running]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         commands = [\"true\"]\n\
         key_commands = [\"true\"]\n\
         \n\
         [gate.profiles.after]\n\
         stages = [{ stage = \"review\", required = true }, \
         { stage = \"human\", required = true }]\n\
         \n\
         [flows.art]\n\
         gate_profile = \"running\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task_with_flow(
            &plan,
            "art",
            "Art",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("art"),
        )
        .expect("add the art task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("art.txt"), "art\n");
    repo.git(&["add", "art.txt"]);
    repo.git(&["commit", "-m", "art"]);
    repo.git(&["push", "origin", &branch]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    set_verifying(&store, &task, &head, &base);

    let gate = repo.radian(&["arc", "gate", "--task", &task]);
    assert_eq!(gate.code, 0, "gate: {}", gate.stderr);
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    let applied = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (applied.state.as_str(), applied.phase.as_str()),
        ("review", "awaiting_human"),
        "the applied gate: {run:?}"
    );

    // The flow now names a `review`-only profile, after the gate was applied.
    tune(
        &store,
        "gate_profile = \"running\"",
        "gate_profile = \"after\"",
    );
    let merged = repo.radian(&["arc", "merge", &task]);
    assert_eq!(
        merged.code, 0,
        "merge: {}\n{}",
        merged.stdout, merged.stderr
    );
    assert!(merged.stdout.contains("done.merged"), "{}", merged.stdout);
    let integration = Verdict::read(
        &verdict_path(
            store.repo(),
            &task,
            &format!("{task}-1"),
            Stage::Integration,
        )
        .expect("the integration verdict path"),
    )
    .expect("the integration verdict reads");
    assert!(integration.is_pass(), "{:?}", integration.reason);
    assert_eq!(integration.profile.as_deref(), Some("running"));
    assert_eq!(
        integration.commands.len(),
        1,
        "the started gate's profile `key_commands` ran on the merge candidate"
    );
}

/// A flow removed after a completed gate is applied still merges that gate's verdicts: the
/// acceptance and the integration do not resolve the task's now-missing flow.
#[test]
fn a_flow_removed_after_apply_still_merges_the_started_gates_verdicts() {
    let repo = Repo::new("gate-profile-flow-removed-after-apply");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    append_config(
        &store,
        "[gate.profiles.art]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         commands = [\"true\"]\n\
         key_commands = [\"true\"]\n\
         \n\
         [flows.art]\n\
         gate_profile = \"art\"\n",
    );
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task_with_flow(
            &plan,
            "art",
            "Art",
            Vec::new(),
            false,
            Vec::new(),
            "brief\n",
            Some("art"),
        )
        .expect("add the art task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("art.txt"), "art\n");
    repo.git(&["add", "art.txt"]);
    repo.git(&["commit", "-m", "art"]);
    repo.git(&["push", "origin", &branch]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    set_verifying(&store, &task, &head, &base);

    let gate = repo.radian(&["arc", "gate", "--task", &task]);
    assert_eq!(gate.code, 0, "gate: {}", gate.stderr);
    let run = repo.radian(&["arc", "run"]);
    assert_eq!(run.code, 0, "run: {}", run.stderr);
    let applied = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (applied.state.as_str(), applied.phase.as_str()),
        ("review", "awaiting_human"),
        "the applied gate: {run:?}"
    );

    // The flow is gone; the file stays valid through the implicit `default` flow.
    tune(&store, "[flows.art]\ngate_profile = \"art\"\n", "");
    let merged = repo.radian(&["arc", "merge", &task]);
    assert_eq!(
        merged.code, 0,
        "merge: {}\n{}",
        merged.stdout, merged.stderr
    );
    assert!(merged.stdout.contains("done.merged"), "{}", merged.stdout);
    let integration = Verdict::read(
        &verdict_path(
            store.repo(),
            &task,
            &format!("{task}-1"),
            Stage::Integration,
        )
        .expect("the integration verdict path"),
    )
    .expect("the integration verdict reads");
    assert!(integration.is_pass(), "{:?}", integration.reason);
    assert_eq!(integration.profile.as_deref(), Some("art"));
    assert_eq!(integration.commands.len(), 1);
}

// ---- `before_attempt` follows the flow's profile ------------------------------------

/// One attempt of `task` in `flow`, with a test-only worker that does nothing.
fn attempt(repo: &Repo, task: &str, number: u32, flow: &str, token: &str) -> Run {
    let number = number.to_string();
    let args = vec![
        "--task",
        task,
        "--attempt",
        number.as_str(),
        "--token",
        token,
        "--flow",
        flow,
        "--worker-command",
        "true",
        "--allow-noop",
    ];
    repo.work(&args, "the brief\n")
}

/// Commits `file` on main in a second clone and pushes it, leaving the main clone's
/// `origin/<main>` stale.
fn advance_main(repo: &Repo, file: &str, content: &str) -> String {
    let other = repo.second_clone("other-main");
    repo.git_at(&other, &["config", "user.email", "arc@example.invalid"]);
    repo.git_at(&other, &["config", "user.name", "arc test"]);
    write(&other.join(file), content);
    repo.git_at(&other, &["add", file]);
    repo.git_at(&other, &["commit", "-m", &format!("main: {file}")]);
    repo.git_at(&other, &["push", "origin", &repo.main_branch]);
    repo.git_at(&other, &["rev-parse", "HEAD"])
        .trim()
        .to_owned()
}

/// The merge commits on `work/<task>`, newest first.
fn merge_commits(repo: &Repo, task: &str) -> Vec<String> {
    repo.git(&[
        "log",
        "--merges",
        "--format=%H",
        &format!("refs/heads/work/{task}"),
    ])
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(str::to_owned)
    .collect()
}

/// A rework of each flow runs only its own profile's `before_attempt` steps: the `merging`
/// flow merges the moved main, the `quiet` flow fetches nothing and merges nothing.
#[test]
fn a_rework_runs_only_its_flows_before_attempt_steps() {
    let repo = Repo::new("gate-profile-before-attempt");
    Store::init(&repo.clone, false).expect("arc init");
    let config = repo.arc().join("config.toml");
    let mut text = fs::read_to_string(&config).expect("config.toml");
    text.push_str(
        "\n[gate.profiles.merging]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         before_attempt = [\"merge-main\"]\n\
         \n\
         [gate.profiles.quiet]\n\
         stages = [{ stage = \"commands\", required = true }, \
         { stage = \"human\", required = true }]\n\
         before_attempt = []\n\
         \n\
         [flows.merging]\n\
         gate_profile = \"merging\"\n\
         \n\
         [flows.quiet]\n\
         gate_profile = \"quiet\"\n",
    );
    fs::write(&config, text).expect("write config.toml");
    let merging = "TK0200-merging";
    let quiet = "TK0201-quiet";
    for (task, flow) in [(merging, "merging"), (quiet, "quiet")] {
        let first = attempt(&repo, task, 1, flow, "tok-1");
        assert!(
            repo.arc()
                .join("attempts")
                .join(task)
                .join("last_head")
                .is_file(),
            "the first attempt ran: {}",
            first.stderr
        );
    }

    let moved = advance_main(&repo, "main.txt", "main\n");
    let stale = repo.main_sha();
    assert_ne!(stale, moved, "the clone's tracking ref is stale");

    let quiet_second = attempt(&repo, quiet, 2, "quiet", "tok-2");
    assert!(
        merge_commits(&repo, quiet).is_empty(),
        "the quiet flow ran a step: {}",
        quiet_second.stderr
    );
    assert_eq!(
        repo.main_sha(),
        stale,
        "the quiet flow fetched the remote main"
    );

    let merging_second = attempt(&repo, merging, 2, "merging", "tok-3");
    assert_eq!(
        merge_commits(&repo, merging).len(),
        1,
        "the merging flow did not merge main: {}",
        merging_second.stderr
    );
    assert_eq!(
        repo.main_sha(),
        moved,
        "the merging flow did not fetch the remote main"
    );
}

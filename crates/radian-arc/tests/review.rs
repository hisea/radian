//! Verification, merge and reject (ADR 0048 decisions 6, 10 and 11).
//!
//! The end-to-end cases run the real `radian` binary and a real `radian exec` child under
//! the mock model; the merge and refuse cases drive the library against a temp repository
//! with a bare `origin`, so real git decides every push and merge.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use radian_arc::gate::{
    ReviewItem, ReviewReport, Stage, VERDICT_SCHEMA_VERSION, Verdict, VerdictResult,
    effective_stages, required_passed, verdict_path, verdict_set_digest,
};
use radian_arc::review;
use radian_arc::store::attempt_ops::{self, Class, Finish};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Exit, Store, plan_ops};
use serde_json::Value;

// ---- store-level fixture -------------------------------------------------------------

/// A repository with a bare `origin`, arc set up, one confirmed plan and one task, and the
/// task's work branch published.
struct Fixture {
    repo: TestRepo,
    store: Store,
    task: String,
    base: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let repo = TestRepo::new(label);
        Store::init(&repo.repo, false).expect("arc init");
        let store = Store::open(&repo.repo).expect("the store opens");
        let plan = store
            .add_plan("p", "A plan", "the request\n")
            .expect("add a plan");
        let task = store
            .add_task(
                &plan,
                "work",
                "A task",
                Vec::new(),
                false,
                Vec::new(),
                "the brief\n",
            )
            .expect("add a task");
        plan_ops::confirm(&store, &plan).expect("confirm");
        let base = repo.rev("main");
        Self {
            repo,
            store,
            task,
            base,
        }
    }

    /// Commits `file` on `work/<task>` at `at`, pushes the branch and returns the commit.
    fn publish(&self, at: &str, file: &str, content: &str) -> String {
        let branch = format!("work/{}", self.task);
        let head = self.repo.commit_on(&branch, at, file, content);
        self.repo.git(&["push", "origin", &branch]);
        head
    }

    /// Puts the task in `review.verifying` for `head`, the way a pushed attempt leaves it.
    fn set_verifying(&self, head: &str) {
        self.set_verifying_at(head, 1);
    }

    /// The same, for a given attempt number.
    fn set_verifying_at(&self, head: &str, attempt: u32) {
        let base = self.base.clone();
        self.store
            .change_task(&self.task, |task| {
                task.state = TaskState::Review;
                task.phase = "verifying".to_owned();
                task.attempt = attempt;
                task.attempt_id = Some(format!("{}-{attempt}", task.id));
                task.token = Some(format!("token-{attempt}"));
                task.base_sha = Some(base.clone());
                task.head_sha = Some(head.to_owned());
                Ok(())
            })
            .expect("the task is in review.verifying");
    }

    fn tune(&self, from: &str, to: &str) {
        let path = self.store.root().join("config.toml");
        let text = fs::read_to_string(&path).expect("config.toml");
        assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
        fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
    }

    fn commands(&self, lines: &[&str]) {
        let list = lines
            .iter()
            .map(|line| format!("\"{line}\""))
            .collect::<Vec<_>>()
            .join(", ");
        self.tune("\ncommands = []", &format!("\ncommands = [{list}]"));
    }

    fn key_commands(&self, lines: &[&str]) {
        let list = lines
            .iter()
            .map(|line| format!("\"{line}\""))
            .collect::<Vec<_>>()
            .join(", ");
        self.tune("\nkey_commands = []", &format!("\nkey_commands = [{list}]"));
    }

    /// Sets the profile's `before_attempt` steps.
    fn before_attempt(&self, steps: &[&str]) {
        let list = steps
            .iter()
            .map(|step| format!("\"{step}\""))
            .collect::<Vec<_>>()
            .join(", ");
        self.tune(
            "before_attempt = [\"merge-main\"]",
            &format!("before_attempt = [{list}]"),
        );
    }

    fn state(&self) -> (String, String) {
        let task = self
            .store
            .load_task(&self.task)
            .expect("the task reads")
            .item;
        (task.state.as_str().to_owned(), task.phase)
    }

    fn verdict(&self, attempt: &str, stage: Stage) -> Verdict {
        let path = self
            .store
            .root()
            .join("runs")
            .join(&self.task)
            .join("attempts")
            .join(attempt)
            .join("verdicts")
            .join(format!("{stage}.json"));
        Verdict::read(&path).expect("the verdict reads")
    }

    fn attempt_label(&self) -> String {
        format!("{}-1", self.task)
    }

    fn verifying_with(&self, head: &str) -> review::Verified {
        self.set_verifying(head);
        review::verify(&self.store, &self.task).expect("verification runs")
    }

    /// The same, with a test-only review program instead of `radian exec`.
    fn verifying_with_review(&self, head: &str, program: &Path) -> review::Verified {
        self.set_verifying(head);
        review::verify_with(&self.store, &self.task, Some(program)).expect("verification runs")
    }

    /// Adds `review`, required, between `commands` and `human` in the default profile.
    fn add_review_stage(&self) {
        self.tune(
            "stages = [{ stage = \"commands\", required = true }, { stage = \"human\", required = true }]",
            "stages = [{ stage = \"commands\", required = true }, { stage = \"review\", \
             required = true }, { stage = \"human\", required = true }]",
        );
    }

    /// Sets `gate.review.exec_args`.
    fn review_exec_args(&self, args: &[&str]) {
        let list = args
            .iter()
            .map(|arg| format!("\"{arg}\""))
            .collect::<Vec<_>>()
            .join(", ");
        self.tune(
            "# exec_args = [\"--model\", \"m\"]",
            &format!("exec_args = [{list}]"),
        );
    }

    /// Sets `gate.review.timeout_secs`.
    fn review_timeout(&self, seconds: u64) {
        self.tune(
            "timeout_secs = 900              # wall-time",
            &format!("timeout_secs = {seconds}              # wall-time"),
        );
    }

    /// Sets `gate.review.prompt_file`.
    fn review_prompt_file(&self, file: &str) {
        self.tune(
            "# prompt_file = \"review.md\"",
            &format!("prompt_file = \"{file}\""),
        );
    }

    fn add_worktree(&self) -> PathBuf {
        let path = self.store.root().join("worktrees").join(&self.task);
        fs::create_dir_all(path.parent().expect("a parent")).unwrap();
        self.repo.git(&["checkout", "main"]);
        self.repo.git(&[
            "worktree",
            "add",
            common::path(&path),
            &format!("work/{}", self.task),
        ]);
        path
    }

    /// Sets `[limits] git_remote_timeout_secs`.
    fn git_remote_timeout(&self, seconds: u64) {
        self.tune(
            "git_remote_timeout_secs = 300",
            &format!("git_remote_timeout_secs = {seconds}"),
        );
    }

    /// Sets the plan's auto level.
    fn set_auto(&self, level: &str) {
        let plan = self
            .store
            .load_task(&self.task)
            .expect("the task")
            .item
            .plan;
        plan_ops::set_auto(&self.store, &plan, level).expect("the auto level");
    }

    /// Runs one rework attempt: the lead's brief, the claim, a commit on `work/<task>` at `at`,
    /// and the pushed result, so the task is back in `review.verifying` for `head`.
    fn rework(&self, at: &str, file: &str, content: &str) -> String {
        plan_ops::add_rework(&self.store, &self.task, "fix the review").expect("a rework brief");
        let claim = attempt_ops::claim(&self.store, &self.task).expect("the next attempt");
        let head = self.publish(at, file, content);
        let finish = Finish {
            attempt_id: claim.attempt_id.clone(),
            token: claim.token.clone(),
            exit_code: 0,
            outcome: "completed".to_owned(),
            base_sha: Some(self.base.clone()),
            head_sha: Some(head.clone()),
            pushed: true,
            failure: None,
        };
        attempt_ops::apply(
            &self.store,
            &self.task,
            claim.attempt,
            Some(&finish),
            None,
            Some(0),
        )
        .expect("the rework result applies");
        head
    }
}

fn assert_state(fixture: &Fixture, expected: (&str, &str)) {
    assert_eq!(
        fixture.state(),
        (expected.0.to_owned(), expected.1.to_owned())
    );
}

// ---- end to end: start ---------------------------------------------------------------

fn write_worker_args(repo: &Repo, base_url: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains("exec_args = []"), "the key is in the file");
    let text = text.replace(
        "exec_args = []",
        &format!("exec_args = [\"--model\", \"m\", \"--base-url\", \"{base_url}\"]"),
    );
    fs::write(&path, text).expect("write config.toml");
}

/// Sets the `commands` stage of the `default` profile to one line.
fn worker_commands(repo: &Repo, line: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains("\ncommands = []"), "the key is in the file");
    let text = text.replacen("\ncommands = []", &format!("\ncommands = [\"{line}\"]"), 1);
    fs::write(&path, text).expect("write config.toml");
}

fn worker_fixture(
    prefix: &str,
    command: &str,
    extra_stages: &[&str],
) -> (Repo, Store, String, Mock) {
    let repo = Repo::new(prefix);
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("the store opens");
    let plan = store
        .add_plan("p", "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task(
            &plan,
            "work",
            "A task",
            Vec::new(),
            false,
            extra_stages
                .iter()
                .map(|stage| (*stage).to_owned())
                .collect(),
            "the brief\n",
        )
        .expect("add a task");
    plan_ops::confirm(&store, &plan).expect("confirm");
    worker_commands(&repo, command);
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    write_worker_args(&repo, mock.base_url());
    (repo, store, task, mock)
}

/// `start` runs attempt, gate, approval reading: one `start` ends at `review.awaiting_human`
/// with a passing `commands` profile.
#[test]
fn start_verifies_a_passing_commands_profile_to_awaiting_human() {
    let (repo, store, task, _mock) = worker_fixture("merge-start-pass", "true", &[]);
    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("verify -> review.awaiting_human")),
        "{lines:?}"
    );
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human")
    );
    assert_eq!(stored.verified_sha, stored.head_sha);
    let digest = stored.verdict_set_digest.expect("a digest is recorded");
    assert_eq!(digest.len(), 64, "the digest is a SHA-256");
    let verdict = Verdict::read(
        &store
            .root()
            .join("runs")
            .join(&task)
            .join("attempts")
            .join(format!("{task}-1"))
            .join("verdicts/commands.json"),
    )
    .expect("the commands verdict reads");
    assert!(verdict.is_pass());
    assert_eq!(verdict.sha, stored.head_sha.unwrap());
    let shown = repo.radian(&["arc", "show", task.as_str()]);
    assert_eq!(shown.code, 0, "{shown:?}");
    assert!(
        shown.stdout.contains("verdict_set_digest:"),
        "{}",
        shown.stdout
    );
}

/// A failing command sends the task to `review.rework_pending`; `show` prints the stage, the
/// reason and the verdict path.
#[test]
fn a_failing_command_makes_rework_pending_with_the_verdict_path() {
    let (repo, store, task, _mock) = worker_fixture("merge-start-fail", "false", &[]);
    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 0, "start failed: {}", run.stderr);
    assert!(
        run.stdout.contains("verify -> review.rework_pending"),
        "{}",
        run.stdout
    );
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending")
    );
    assert!(
        stored.verdict_set_digest.is_none(),
        "no approval without a pass"
    );
    let shown = repo.radian(&["arc", "show", task.as_str()]);
    assert!(
        shown
            .stdout
            .contains("rework_reason: commands: command_failed"),
        "{}",
        shown.stdout
    );
    assert!(
        shown.stdout.contains("verdicts/commands.json"),
        "{}",
        shown.stdout
    );
    // The rework path is the same after a failed stage as after a reject.
    plan_ops::add_rework(&store, &task, "make the command pass").expect("a rework brief");
    assert_eq!(store.load_task(&task).unwrap().item.state, TaskState::Todo);
    assert_eq!(store.load_task(&task).unwrap().item.phase, "ready");
}

/// A profile or task that lists a stage this build does not run refuses verification; the
/// task stays in `review.verifying` and `show` prints it.
#[test]
fn a_listed_mutation_stage_is_not_built_yet() {
    let (repo, store, task, _mock) = worker_fixture("merge-start-mutation", "true", &["mutation"]);
    let run = repo.radian(&["arc", "start", task.as_str()]);
    assert_eq!(run.code, 3, "start: {run:?}");
    assert!(
        run.stderr.contains("stage mutation is not built yet"),
        "{}",
        run.stderr
    );
    let stored = store.load_task(&task).expect("the task reads").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "verifying")
    );
    let shown = repo.radian(&["arc", "show", task.as_str()]);
    assert!(
        shown.stdout.contains("stage mutation is not built yet"),
        "{}",
        shown.stdout
    );
}

// ---- verification --------------------------------------------------------------------

/// A crash between stages leaves the attempt's verdict: the next verify reuses it instead of
/// running a command that now fails. Mutation: ignore an existing verdict and this test sees
/// `review.rework_pending`.
#[test]
fn a_verdict_is_reused_after_a_crash_between_stages() {
    let fixture = Fixture::new("verify-reuse");
    let head = fixture.publish(&fixture.base.clone(), "change.txt", "change\n");
    let first = fixture.verifying_with(&head);
    assert_eq!(first.phase, "awaiting_human");
    let digest = first.digest.clone().expect("a digest");
    let path = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(fixture.attempt_label())
        .join("verdicts/commands.json");
    let before = fs::read(&path).expect("the verdict");

    // A restart: the task is verifying again for the same attempt and sha, and the profile's
    // command would now fail if it ran.
    fixture
        .store
        .change_task(&fixture.task, |task| {
            task.phase = "verifying".to_owned();
            task.verdict_set_digest = None;
            Ok(())
        })
        .expect("the task is verifying again");
    fixture.commands(&["false"]);

    let second = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(second.phase, "awaiting_human");
    assert_eq!(
        second.reused,
        vec![Stage::Commands],
        "the verdict was reused"
    );
    assert_eq!(second.digest.as_deref(), Some(digest.as_str()));
    assert_eq!(
        fs::read(&path).expect("the verdict"),
        before,
        "never rewritten"
    );
}

// ---- verification candidate (main merged in) -----------------------------------------

/// Main unchanged since the task's base: the candidate is `verified_sha` itself, and the
/// verdict says so beside the sha it is for.
#[test]
fn verification_uses_verified_sha_when_main_has_not_moved() {
    let fixture = Fixture::new("verify-candidate-same");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let verified = fixture.verifying_with(&head);
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Commands);
    assert!(verdict.is_pass(), "{:?}", verdict.reason);
    assert_eq!(verdict.sha, head, "the verdict belongs to verified_sha");
    assert_eq!(verdict.main_sha.as_deref(), Some(fixture.base.as_str()));
    assert_eq!(verdict.candidate_sha.as_deref(), Some(head.as_str()));
}

/// Main gained a commit that breaks a gate command: the task's own commit alone would pass,
/// so only running the command on the candidate can send it to rework.
#[test]
fn main_that_breaks_a_gate_command_sends_the_task_to_rework() {
    let fixture = Fixture::new("verify-main-breaks");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    // Passes on the task's commit, fails once main's file is merged in.
    fixture.commands(&["test ! -f main-only.txt"]);
    fixture.repo.advance_main("main-only.txt", "main\n");

    let verified = fixture.verifying_with(&head);
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Commands);
    assert!(!verdict.is_pass());
    assert_eq!(verdict.reason.as_deref(), Some("command_failed"));
    assert_eq!(
        verdict.sha, head,
        "the verdict still belongs to verified_sha"
    );
    assert!(
        verdict.candidate_sha.as_deref() != Some(head.as_str()),
        "the command ran on verified_sha alone: {verdict:?}"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    let reason = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(reason.contains("command_failed"), "{reason}");
}

/// Main gained a commit a gate command needs: `verified_sha` alone fails the command, the
/// candidate passes it.
#[test]
fn main_that_a_gate_command_needs_makes_the_task_pass() {
    let fixture = Fixture::new("verify-main-needed");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    fixture.commands(&["test -f needed.txt"]);
    let moved = fixture.repo.advance_main("needed.txt", "needed\n");

    let verified = fixture.verifying_with(&head);
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Commands);
    assert!(verdict.is_pass(), "{:?}", verdict.reason);
    assert_eq!(verdict.sha, head);
    assert_eq!(verdict.main_sha.as_deref(), Some(moved.as_str()));
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    assert_ne!(candidate, head, "the command ran on verified_sha alone");
    assert!(fixture.repo.is_ancestor(&moved, &candidate));
    assert!(fixture.repo.is_ancestor(&head, &candidate));
}

/// A conflicting commit on main, with `merge-main` off: building the candidate is the failed
/// stage. The task goes to rework with reason `conflict`, the path is listed, and nothing is
/// pushed.
#[test]
fn a_verification_merge_conflict_makes_rework_pending_with_the_path() {
    let fixture = Fixture::new("verify-conflict");
    fixture.before_attempt(&[]);
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    let moved = fixture.repo.advance_main("file.txt", "main side\n");

    let verified = fixture.verifying_with(&head);
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Commands);
    assert!(!verdict.is_pass());
    assert_eq!(verdict.reason.as_deref(), Some("conflict"));
    assert_eq!(verdict.conflict_paths, vec!["file.txt".to_owned()]);
    assert!(
        verdict.candidate_sha.is_none(),
        "a conflicting merge has no candidate"
    );
    assert_eq!(
        verdict.sha, head,
        "the verdict still belongs to verified_sha"
    );
    assert_eq!(verdict.main_sha.as_deref(), Some(moved.as_str()));
    assert!(
        verdict.commands.is_empty(),
        "commands ran on a conflicted tree"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    let reason = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(reason.contains("conflict: file.txt"), "{reason}");
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(moved.as_str()),
        "verification never pushes to main"
    );
    assert_eq!(
        fixture
            .repo
            .ls_remote(&format!("refs/heads/work/{}", fixture.task))
            .as_deref(),
        Some(head.as_str()),
        "the task branch did not move"
    );
}

// ---- reverify -------------------------------------------------------------------------

/// The gate command was broken, the owner fixes it, and `reverify` re-runs the gate for the
/// same commit: a new attempt without a worker, a pass to `review.awaiting_human`, and no
/// retry used. Mutation: count the reverify attempt toward `max_attempts` and the next
/// failure is `failed.exhausted` instead of a retry.
#[test]
fn reverify_reruns_the_gate_without_a_worker_and_does_not_use_an_attempt() {
    let fixture = Fixture::new("verify-reverify");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    fixture.commands(&["false"]);
    let first = fixture.verifying_with(&head);
    assert_eq!(first.phase, "rework_pending", "{first:?}");
    assert_eq!(fixture.attempt_label(), format!("{}-1", fixture.task));

    // The gate command was wrong, not the commit: fix it and verify the same commit again.
    fixture.tune("commands = [\"false\"]", "commands = [\"true\"]");
    let reverified = review::reverify(&fixture.store, &fixture.task).expect("reverify");
    let new_label = format!("{}-2", fixture.task);
    assert_eq!(reverified.attempt, 2);
    assert_eq!(reverified.attempt_id, new_label);
    let verified = reverified
        .verification
        .expect("the verification runs after the claim");
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    assert_eq!(verified.attempt_id, new_label);

    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(stored.attempt, 2);
    assert_eq!(stored.attempt_id.as_deref(), Some(new_label.as_str()));
    assert_eq!(
        stored.head_sha.as_deref(),
        Some(head.as_str()),
        "the commit does not change"
    );
    assert_eq!(stored.verified_sha.as_deref(), Some(head.as_str()));
    assert!(stored.verdict_set_digest.is_some());
    assert_eq!(
        stored.auto_rework, 0,
        "a reverify is not an automatic rework"
    );

    // No worker ran: the new attempt has an attempt result but no result stream.
    let run = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(&new_label);
    assert!(
        !run.join("result.jsonl").exists(),
        "a reverify attempt runs no worker"
    );
    let record: Value = serde_json::from_str(
        &fs::read_to_string(run.join("attempt_result.json")).expect("the attempt result"),
    )
    .expect("the attempt result parses");
    assert_eq!(record["class"], "reverify");
    assert_eq!(record["cost_usd"], 0.0, "a reverify costs nothing");

    // The new attempt's verdict is its own; the old attempt's failing verdict is untouched.
    let new = fixture.verdict(&new_label, Stage::Commands);
    assert!(new.is_pass(), "{:?}", new.reason);
    assert_eq!(new.sha, head);
    let old = fixture.verdict(&fixture.attempt_label(), Stage::Commands);
    assert!(!old.is_pass(), "the old verdict is never rewritten");

    // `max_attempts` (3) is not used up by the reverify attempt: the next worker failure is
    // attempt 3 but counts as the second attempt, so it retries.
    plan_ops::add_rework(&fixture.store, &fixture.task, "try again").expect("a rework brief");
    let claim = attempt_ops::claim(&fixture.store, &fixture.task).expect("the next attempt");
    assert_eq!(claim.attempt, 3);
    let finish = Finish {
        attempt_id: claim.attempt_id.clone(),
        token: claim.token.clone(),
        exit_code: 1,
        outcome: "failed".to_owned(),
        base_sha: Some(fixture.base.clone()),
        head_sha: None,
        pushed: false,
        failure: None,
    };
    let applied = attempt_ops::apply(
        &fixture.store,
        &fixture.task,
        3,
        Some(&finish),
        None,
        Some(1),
    )
    .expect("the failed attempt applies");
    assert_eq!(applied.class, Some(Class::Model));
    assert_eq!(
        (applied.state.as_str(), applied.phase.as_str()),
        ("todo", "retry_wait"),
        "the reverify attempt did not consume a retry"
    );
}

/// A reject note is the owner's decision about the work, not a wrong gate: `reverify` refuses
/// it and claims nothing. Mutation: accept a reject-note task and this test sees attempt 2.
#[test]
fn reverify_refuses_a_reject_note() {
    let fixture = Fixture::new("verify-reverify-reject");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    review::reject(&fixture.store, &fixture.task, "not what the plan asked for").expect("reject");

    let error =
        review::reverify(&fixture.store, &fixture.task).expect_err("a reject note is refused");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("rejected"), "{error:?}");
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(stored.attempt, 1, "a refusal claims nothing");
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending")
    );
}

/// A failed delivery check is not a verification stage: `reverify` refuses it.
#[test]
fn reverify_refuses_a_delivery_failure() {
    let fixture = Fixture::new("verify-reverify-delivery");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let claim = attempt_ops::claim(&fixture.store, &fixture.task).expect("the first claim");
    let finish = Finish {
        attempt_id: claim.attempt_id.clone(),
        token: claim.token.clone(),
        exit_code: 1,
        outcome: "completed".to_owned(),
        base_sha: Some(fixture.base.clone()),
        head_sha: Some(head),
        pushed: false,
        failure: Some("uncommitted or untracked files: dirty.txt".to_owned()),
    };
    attempt_ops::apply(
        &fixture.store,
        &fixture.task,
        claim.attempt,
        Some(&finish),
        None,
        Some(1),
    )
    .expect("the delivery failure applies");
    assert_state(&fixture, ("review", "rework_pending"));

    let error =
        review::reverify(&fixture.store, &fixture.task).expect_err("a delivery failure is refused");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("delivery"), "{error:?}");
    assert_eq!(
        fixture.store.load_task(&fixture.task).unwrap().item.attempt,
        1
    );
}

/// The remote `work/<task>` moved after the verdict: the claim happens, verification refuses
/// with the moved-branch reason, and the task stays in `review.verifying` (as verification
/// leaves it).
#[test]
fn reverify_refuses_a_moved_branch() {
    let fixture = Fixture::new("verify-reverify-moved");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let later = fixture.repo.commit_on(
        &format!("later-{}", fixture.task),
        &head,
        "later.txt",
        "later\n",
    );
    fixture.repo.git(&[
        "push",
        "origin",
        &format!("{later}:refs/heads/work/{}", fixture.task),
    ]);

    let reverified = review::reverify(&fixture.store, &fixture.task).expect("the claim runs");
    let error = reverified
        .verification
        .expect_err("the moved branch is refused");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains(&later), "{error:?}");
    assert_eq!(reverified.attempt, 2);
    assert_state(&fixture, ("review", "verifying"));
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        stored.head_sha.as_deref(),
        Some(head.as_str()),
        "the task's commit does not change"
    );
    let reason = review::rework_reason(&fixture.store, &stored).expect("the moved-branch reason");
    assert!(reason.contains(&later), "{reason}");
}

/// A task that never reached review is refused and nothing is claimed.
#[test]
fn reverify_refuses_a_task_that_never_reached_review() {
    let fixture = Fixture::new("verify-reverify-todo");
    let error =
        review::reverify(&fixture.store, &fixture.task).expect_err("a todo task is refused");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("todo.ready"), "{error:?}");
    assert_state(&fixture, ("todo", "ready"));
    assert_eq!(
        fixture.store.load_task(&fixture.task).unwrap().item.attempt,
        0
    );
}

// ---- merge ---------------------------------------------------------------------------

/// Main unchanged: the candidate is `head_sha`; the remote main becomes it, the task is
/// `done.merged`, and its worktree is gone.
#[test]
fn merge_with_main_unchanged_pushes_the_head_and_removes_the_worktree() {
    let fixture = Fixture::new("merge-unchanged");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let worktree = fixture.add_worktree();
    assert!(worktree.is_dir(), "the worktree exists before the merge");

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert_eq!(merged.candidate_sha.as_deref(), Some(head.as_str()));
    assert!(merged.warning.is_none(), "{merged:?}");
    assert_state(&fixture, ("done", "merged"));
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(head.as_str()),
        "the remote main is the verified head"
    );
    assert_eq!(
        fixture
            .repo
            .ls_remote(&format!("refs/heads/work/{}", fixture.task)),
        Some(head.clone()),
        "the task branch stays on the remote"
    );
    assert!(!worktree.exists(), "the worktree was removed");
    let human = fixture.verdict(&fixture.attempt_label(), Stage::Human);
    assert!(human.is_pass());
    assert_eq!(human.sha, head);
}

/// The CLI path: `radian arc merge` prints `done.merged` and pushes the same way.
#[test]
fn merge_through_the_cli_reaches_done_merged() {
    let repo = Repo::new("merge-cli");
    Store::init(&repo.clone, false).expect("arc init");
    let store = Store::open(&repo.clone).expect("open");
    let plan = store.add_plan("p", "A plan", "the request\n").unwrap();
    let task = store
        .add_task(
            &plan,
            "work",
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    plan_ops::confirm(&store, &plan).unwrap();
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("feature.txt"), "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "feature"]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", "origin", &branch]);
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .unwrap();
    review::verify(&store, &task).expect("verification runs");

    let run = repo.radian(&["arc", "merge", &task]);
    assert_eq!(run.code, 0, "merge: {}", run.stderr);
    assert!(run.stdout.contains("done.merged"), "{}", run.stdout);
    assert_eq!(repo.remote_main_sha(), head);
}

/// Main moved before the merge: the candidate is a merge commit of head onto main, and the
/// remote main becomes it.
#[test]
fn merge_with_main_moved_pushes_a_merge_commit() {
    let fixture = Fixture::new("merge-moved");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let moved = fixture.repo.advance_main("other.txt", "other\n");

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    let candidate = merged.candidate_sha.clone().expect("a candidate");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert_ne!(
        candidate, head,
        "integration pushed the head instead of a merge"
    );
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(candidate.as_str())
    );
    let parents = fixture
        .repo
        .git(&["rev-list", "--parents", "-n", "1", &candidate]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "not a merge commit: {parents}");
    assert!(
        parts.contains(&moved.as_str()),
        "main is not a parent: {parents}"
    );
    assert!(
        parts.contains(&head.as_str()),
        "head is not a parent: {parents}"
    );
    assert!(
        fixture.repo.is_ancestor(&head, &candidate),
        "the candidate contains head"
    );
}

/// Main moves during integration: `merge` integrates again, and the remote main keeps both
/// the moved commit and the task's head.
#[test]
fn merge_integrates_again_when_main_moves_during_the_integration() {
    let fixture = Fixture::new("merge-during");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let marker = fixture.repo.repo.with_file_name("moved-marker");
    let command = format!(
        "if [ ! -f {marker} ]; then touch {marker}; \
         git -c user.name=test -c user.email=test@example.com commit --allow-empty -m moved \
         && git push origin HEAD:refs/heads/main; fi",
        marker = marker.display()
    );
    fixture.key_commands(&[command.as_str()]);

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert_eq!(merged.rounds, 2, "the second integration run settled it");
    let candidate = merged.candidate_sha.clone().expect("a candidate");
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(candidate.as_str())
    );
    let moved = fixture.repo.git(&["rev-parse", "refs/remotes/origin/main"]);
    // Round one's base was `fixture.base`, round two merged onto the moved main.
    let first = fixture.verdict(&fixture.attempt_label(), Stage::Integration);
    let second = fixture.verdict(
        &format!("{}.i2", fixture.attempt_label()),
        Stage::Integration,
    );
    assert_eq!(first.base_sha.as_deref(), Some(fixture.base.as_str()));
    assert!(first.is_pass(), "{:?}", first.reason);
    assert!(second.is_pass(), "{:?}", second.reason);
    assert_ne!(second.base_sha.as_deref(), Some(fixture.base.as_str()));
    assert_eq!(second.base_sha.as_deref(), Some(moved.as_str()));
    assert!(
        fixture.repo.is_ancestor(&head, &candidate),
        "the candidate contains the head"
    );
    assert!(
        fixture.repo.is_ancestor(&moved, &candidate),
        "the candidate contains the moved main"
    );
}

/// The push to the remote main is a remote call: a remote that accepts the connection and
/// then hangs makes the push run out after the limit. The task stays where a rejected push
/// leaves it (`review.integrating`, the next round can try again), the reason carries the
/// line, and the child the remote spawned is gone.
#[test]
fn a_merge_push_that_hangs_reports_the_line_and_kills_its_child() {
    let fixture = Fixture::new("merge-push-hang");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    fixture.git_remote_timeout(1);
    let hang = HangRemote::install(&fixture.repo, "merge-push", "receive");

    let error = review::merge(&fixture.store, &fixture.task).expect_err("the push runs out");
    assert!(
        error.message().contains("git push timed out after 1 s"),
        "{error:?}"
    );
    assert_state(&fixture, ("review", "integrating"));
    hang.assert_gone(hang.child_pid(), "the remote's child");
}

/// A conflict in the integration stage, with `merge-main` off, sends the task to rework with
/// the path and the verdict recorded, and leaves the remote main untouched.
#[test]
fn a_conflict_makes_rework_pending() {
    let fixture = Fixture::new("merge-conflict");
    fixture.before_attempt(&[]);
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let moved = fixture.repo.advance_main("file.txt", "main side\n");

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge returns");
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("review", "rework_pending")
    );
    let reason = merged.reason.expect("a reason");
    assert!(reason.contains("conflict"), "{reason}");
    assert!(reason.contains("file.txt"), "{reason}");
    assert!(reason.contains("verdicts/integration.json"), "{reason}");
    assert_state(&fixture, ("review", "rework_pending"));
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(moved.as_str()),
        "a conflict never moves main"
    );
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Integration);
    assert!(!verdict.is_pass());
    assert_eq!(verdict.reason.as_deref(), Some("conflict"));
    assert_eq!(verdict.conflict_paths, vec!["file.txt".to_owned()]);
    let shown = review::rework_reason(
        &fixture.store,
        &fixture.store.load_task(&fixture.task).unwrap().item,
    );
    assert!(
        shown
            .as_ref()
            .is_some_and(|text| text.contains("conflict: file.txt")),
        "{shown:?}"
    );
}

// ---- the conflict rework arc writes (ADR 0048 decision 9) ------------------------------

/// A verification-stage conflict with `merge-main` on: arc writes the next attempt's brief
/// itself and returns the task to `todo.ready`, with the paths, and counts nothing in
/// `auto_rework`. Mutations: drop the brief (no heading) or count the rework (auto_rework 1).
#[test]
fn a_verification_conflict_with_main_writes_the_next_brief_and_returns_to_ready() {
    let fixture = Fixture::new("verify-conflict-rework");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    let moved = fixture.repo.advance_main("file.txt", "main side\n");

    let verified = fixture.verifying_with(&head);
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );
    let conflict = verified.conflict.clone().expect("the conflict");
    assert_eq!(conflict.stage, Stage::Commands);
    assert_eq!(conflict.main_sha, moved);
    assert_eq!(conflict.paths, vec!["file.txt".to_owned()]);
    assert!(conflict.stop.is_none());
    assert_eq!(
        verified.line(),
        format!(
            "{} conflict with main -> todo.ready (automatic rework)",
            fixture.task
        )
    );

    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert_eq!(
        stored.auto_rework, 0,
        "a conflict rework is not an automatic rework"
    );
    assert_eq!(stored.attempt, 1, "no new attempt is claimed");
    let short = &moved[..7];
    assert!(stored.body.starts_with("the brief"), "{}", stored.body);
    assert!(
        stored.body.contains("## Rework 2 (conflict with main)"),
        "{}",
        stored.body
    );
    assert!(
        stored
            .body
            .contains(&format!("conflicted with main at `{short}` in `commands`")),
        "{}",
        stored.body
    );
    assert!(stored.body.contains("- file.txt"), "{}", stored.body);
    assert!(
        stored.body.contains("The next attempt starts mid-merge"),
        "{}",
        stored.body
    );
    assert!(
        stored.body.contains("git commit --no-edit"),
        "{}",
        stored.body
    );
}

/// An integration conflict takes the same path: the task returns to `todo.ready` with the
/// brief arc writes, and main stays where it was.
#[test]
fn an_integration_conflict_with_main_writes_the_next_brief_and_returns_to_ready() {
    let fixture = Fixture::new("merge-conflict-rework");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let moved = fixture.repo.advance_main("file.txt", "main side\n");

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge returns");
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("todo", "ready"),
        "{merged:?}"
    );
    let conflict = merged.conflict.clone().expect("the conflict");
    assert_eq!(conflict.stage, Stage::Integration);
    assert_eq!(conflict.main_sha, moved);
    assert_eq!(conflict.paths, vec!["file.txt".to_owned()]);
    assert_eq!(
        merged.line(),
        format!(
            "{} conflict with main -> todo.ready (automatic rework)",
            fixture.task
        )
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(stored.auto_rework, 0);
    assert!(
        stored.body.contains("## Rework 2 (conflict with main)"),
        "{}",
        stored.body
    );
    assert!(stored.body.contains("in `integration`"), "{}", stored.body);
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(moved.as_str()),
        "a conflict never moves main"
    );
}

/// A `command_failed` verdict keeps the path to `review.rework_pending`, even with
/// `merge-main` on: the worker would not start mid-merge. Mutation: send every failure reason
/// down the automatic path.
#[test]
fn a_command_failed_verdict_still_waits_for_the_lead() {
    let fixture = Fixture::new("verify-command-failed");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    fixture.commands(&["false"]);

    let verified = fixture.verifying_with(&head);
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    assert!(verified.conflict.is_none());
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending")
    );
    assert!(
        !stored.body.contains("conflict with main"),
        "{}",
        stored.body
    );
}

/// `max_attempts` does not decide a conflict: the attempt number is not the retry count, and
/// a conflict rework is not a retry. A conflict on the attempt that used to be the task's last
/// one writes its brief and returns the task to `todo.ready`. Mutation: compare `max_attempts`
/// with the attempt number again and the task is `failed.exhausted`.
#[test]
fn a_conflict_on_the_old_last_attempt_still_gets_the_conflict_rework() {
    let fixture = Fixture::new("verify-conflict-last-attempt");
    fixture
        .store
        .change_task(&fixture.task, |task| {
            task.max_attempts = Some(1);
            Ok(())
        })
        .expect("the task limit");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    fixture.repo.advance_main("file.txt", "main side\n");

    fixture.set_verifying_at(&head, 3);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );
    let conflict = verified.conflict.clone().expect("the conflict");
    assert!(conflict.stop.is_none());
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert!(
        stored.body.contains("## Rework 4 (conflict with main)"),
        "{}",
        stored.body
    );
}

/// `[limits] max_conflict_rework` is how many conflict reworks arc writes in a row: with the
/// default 3, three conflicts write their briefs and the fourth stops in
/// `review.rework_pending`. The conflict line and `show`'s `rework_reason` say why, and the
/// conflict never counts as an automatic rework and never pauses the plan. Mutation: drop the
/// limit and the fourth conflict returns to `todo.ready`.
#[test]
fn max_conflict_rework_stops_a_run_of_conflicts_and_says_why() {
    let fixture = Fixture::new("verify-conflict-limit");
    let plan = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    plan_ops::set_auto(&fixture.store, &plan, "run").expect("auto run");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    fixture.repo.advance_main("file.txt", "main side\n");

    for attempt in 1..=3u32 {
        fixture.set_verifying_at(&head, attempt);
        let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
        assert_eq!(
            (verified.state, verified.phase.as_str()),
            ("todo", "ready"),
            "conflict {attempt}: {verified:?}"
        );
        assert_eq!(
            verified.line(),
            format!(
                "{} conflict with main -> todo.ready (automatic rework)",
                fixture.task
            )
        );
    }

    fixture.set_verifying_at(&head, 4);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("review", "rework_pending"),
        "{verified:?}"
    );
    let conflict = verified.conflict.clone().expect("the conflict");
    let stop = conflict.stop.clone().expect("the stop text");
    assert!(stop.contains("wrote 3 conflict reworks in a row"), "{stop}");
    assert!(stop.contains("max_conflict_rework"), "{stop}");
    assert_eq!(
        verified.line(),
        format!(
            "{} conflict with main -> review.rework_pending: {stop}",
            fixture.task
        )
    );

    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending")
    );
    assert!(
        stored.body.contains("## Rework 4 (conflict with main)"),
        "{}",
        stored.body
    );
    assert!(
        !stored.body.contains("## Rework 5"),
        "the limit wrote a brief: {}",
        stored.body
    );
    assert_eq!(
        stored.auto_rework, 0,
        "a conflict is not an automatic rework"
    );
    assert_eq!(
        fixture.store.load_plan(&plan).unwrap().item.auto,
        "run",
        "the conflict limit never pauses the plan"
    );
    let reason = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(reason.contains(&stop), "{reason}");
}

/// `1` writes exactly one conflict rework: the first conflict gets its brief and the second
/// waits for the lead. Mutation: drop the limit and the second conflict returns to
/// `todo.ready`.
#[test]
fn a_conflict_limit_of_one_writes_one_brief_and_then_waits() {
    let fixture = Fixture::new("verify-conflict-one");
    fixture.tune("max_conflict_rework = 3", "max_conflict_rework = 1");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    fixture.repo.advance_main("file.txt", "main side\n");

    fixture.set_verifying_at(&head, 1);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );

    fixture.set_verifying_at(&head, 2);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("review", "rework_pending"),
        "{verified:?}"
    );
    let conflict = verified.conflict.clone().expect("the conflict");
    let stop = conflict.stop.clone().expect("the stop text");
    assert!(stop.contains("wrote 1 conflict reworks in a row"), "{stop}");
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        stored.body.contains("## Rework 2 (conflict with main)"),
        "{}",
        stored.body
    );
    assert!(
        !stored.body.contains("## Rework 3"),
        "the limit wrote a second brief: {}",
        stored.body
    );
}

/// The lead's own rework is a person stepping in: it sets the run of conflict reworks back to
/// 0, so the task gets a fresh automatic budget. Mutation: drop the reset and the conflict
/// after the lead's brief waits at once.
#[test]
fn a_lead_rework_resets_the_conflict_run() {
    let fixture = Fixture::new("verify-conflict-lead-rework");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    fixture.repo.advance_main("file.txt", "main side\n");

    for attempt in 1..=3u32 {
        fixture.set_verifying_at(&head, attempt);
        let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
        assert_eq!(verified.phase, "ready", "conflict {attempt}: {verified:?}");
    }
    // The fourth conflict is the one the limit stops.
    fixture.set_verifying_at(&head, 4);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");

    plan_ops::add_rework(&fixture.store, &fixture.task, "the lead takes over")
        .expect("the lead's rework");
    let path = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join(review::CONFLICT_REWORK_FILE);
    let text = fs::read_to_string(&path).expect("the conflict record");
    let record: serde_json::Value = serde_json::from_str(&text).expect("the record parses");
    assert_eq!(
        record["reworks"], 0,
        "the lead's rework kept the run: {text}"
    );

    // The next conflict writes its own brief again.
    fixture.set_verifying_at(&head, 5);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        stored.body.contains("## Rework 6 (conflict with main)"),
        "{}",
        stored.body
    );
}

/// `0` writes no conflict rework at all: the first conflict waits for the lead. Mutation: drop
/// the limit and the brief is written.
#[test]
fn a_zero_conflict_limit_never_writes_the_rework() {
    let fixture = Fixture::new("verify-conflict-zero");
    fixture.tune("max_conflict_rework = 3", "max_conflict_rework = 0");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    fixture.repo.advance_main("file.txt", "main side\n");

    let verified = fixture.verifying_with(&head);
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("review", "rework_pending"),
        "{verified:?}"
    );
    let conflict = verified.conflict.clone().expect("the conflict");
    let stop = conflict.stop.clone().expect("the stop text");
    assert!(stop.contains("wrote 0 conflict reworks in a row"), "{stop}");
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        !stored.body.contains("## Rework"),
        "the limit wrote a brief: {}",
        stored.body
    );
}

/// A verification that does not end in a conflict starts a new run: two conflicts, a review
/// that passes, then two more conflicts still write their briefs. Mutation: drop the reset and
/// the first conflict after the pass reaches the limit and waits.
#[test]
fn a_non_conflicting_verification_resets_the_conflict_count() {
    let fixture = Fixture::new("verify-conflict-reset");
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["true"]);
    fixture.repo.advance_main("file.txt", "main side\n");

    for attempt in 1..=2u32 {
        fixture.set_verifying_at(&head, attempt);
        let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
        assert_eq!(
            (verified.state, verified.phase.as_str()),
            ("todo", "ready"),
            "conflict {attempt}: {verified:?}"
        );
    }

    // The branch takes main's content, so the candidate merges and the gate passes.
    let merged = fixture.publish(&head, "file.txt", "main side\n");
    fixture.set_verifying_at(&merged, 3);
    let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");

    // Main moves again and conflicts with the new branch content: a new run of conflicts.
    fixture.repo.advance_main("file.txt", "main side 2\n");
    for attempt in 4..=5u32 {
        fixture.set_verifying_at(&merged, attempt);
        let verified = review::verify(&fixture.store, &fixture.task).expect("verification runs");
        assert_eq!(
            (verified.state, verified.phase.as_str()),
            ("todo", "ready"),
            "conflict {attempt}: {verified:?}"
        );
    }
}

/// A `reverify` attempt that conflicts takes the same path as a worker attempt.
#[test]
fn a_reverify_conflict_takes_the_same_path() {
    let fixture = Fixture::new("verify-reverify-conflict");
    // The first attempt waits for the lead (the step is off), so `reverify` may run.
    fixture.before_attempt(&[]);
    let head = fixture.publish(&fixture.base.clone(), "file.txt", "work side\n");
    fixture.commands(&["false"]);
    assert_eq!(fixture.verifying_with(&head).phase, "rework_pending");

    fixture.tune("before_attempt = []", "before_attempt = [\"merge-main\"]");
    fixture.tune("commands = [\"false\"]", "commands = [\"true\"]");
    fixture.repo.advance_main("file.txt", "main side\n");
    let reverified = review::reverify(&fixture.store, &fixture.task).expect("reverify runs");
    let verified = reverified.verification.expect("the verification runs");
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        stored.body.contains("## Rework 3 (conflict with main)"),
        "{}",
        stored.body
    );
}

/// A branch pushed to after `review.awaiting_human` makes the approval invalid: `merge`
/// refuses, and the task returns to `review.verifying` with the reason recorded.
#[test]
fn a_branch_pushed_after_awaiting_human_returns_to_verifying() {
    let fixture = Fixture::new("merge-moved-branch");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let later = fixture.repo.commit_on(
        &format!("later-{}", fixture.task),
        &head,
        "later.txt",
        "later\n",
    );
    fixture.repo.git(&[
        "push",
        "origin",
        &format!("{later}:refs/heads/work/{}", fixture.task),
    ]);

    let error = review::merge(&fixture.store, &fixture.task).expect_err("merge refuses");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("work/"), "{error:?}");
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "verifying")
    );
    assert!(stored.verified_sha.is_none(), "the old approval is invalid");
    assert!(stored.verdict_set_digest.is_none());
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(fixture.base.as_str()),
        "main never moved"
    );
    let human = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(fixture.attempt_label())
        .join("verdicts/human.json");
    assert!(!human.exists(), "no approval was written");
    let reason = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(reason.contains(&fixture.task), "{reason}");
    assert!(reason.contains(&later), "{reason}");
}

/// A changed verdict file changes the digest: the approval is invalid, the task returns to
/// `review.verifying` (rather than waiting on a merge that can never happen), and nothing is
/// pushed. Mutation: skip the digest check and the merge completes.
#[test]
fn a_changed_verdict_file_invalidates_the_approval() {
    let fixture = Fixture::new("merge-digest");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    let path = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(fixture.attempt_label())
        .join("verdicts/commands.json");
    let text = fs::read_to_string(&path).expect("the verdict");
    assert!(text.contains("\"result\": \"pass\""), "{text}");
    fs::write(
        &path,
        text.replace("\"result\": \"pass\"", "\"result\": \"fail\""),
    )
    .unwrap();

    let error = review::merge(&fixture.store, &fixture.task).expect_err("merge refuses");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("verdict set"), "{error:?}");
    // The digest can never match again without a new verification, so the task goes back to
    // the step that recomputes it.
    assert_state(&fixture, ("review", "verifying"));
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(stored.verified_sha.is_none(), "the old approval is invalid");
    assert!(stored.verdict_set_digest.is_none());
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(fixture.base.as_str())
    );
    let human = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(fixture.attempt_label())
        .join("verdicts/human.json");
    assert!(!human.exists(), "no approval was written");
}

/// A main that keeps moving under the integration is never overwritten: each round
/// integrates again, and after three rounds the task stays in `review.integrating` with the
/// remote main still the other writer's commits.
///
/// The clone's fetch refspec leaves main out, so only the integration's explicit fetch can
/// see the move; before that fetch a stale tracking ref let the push be rejected instead
/// (see the slice report).
#[test]
fn a_main_that_keeps_moving_is_never_overwritten() {
    let fixture = Fixture::new("merge-no-force");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    fixture.repo.git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/work/*:refs/remotes/origin/work/*",
    ]);
    let marker = fixture.repo.repo.with_file_name("moved-markers");
    let command = format!(
        "git -c user.name=test -c user.email=test@example.com commit --allow-empty -m moved \
         && git push origin HEAD:refs/heads/main \
         && echo moved >> {}",
        marker.display()
    );
    fixture.key_commands(&[command.as_str()]);

    let error = review::merge(&fixture.store, &fixture.task).expect_err("the merge refuses");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("integrating"), "{error:?}");
    assert_state(&fixture, ("review", "integrating"));
    let main = fixture
        .repo
        .ls_remote("refs/heads/main")
        .expect("the remote main");
    assert!(
        fixture.repo.is_ancestor(&fixture.base, &main),
        "main lost the commit it had: {main}"
    );
    for label in [
        fixture.attempt_label(),
        format!("{}.i2", fixture.attempt_label()),
        format!("{}.i3", fixture.attempt_label()),
    ] {
        let verdict = fixture.verdict(&label, Stage::Integration);
        assert!(verdict.is_pass(), "{label}: {:?}", verdict.reason);
        assert_ne!(
            verdict.candidate_sha.as_deref(),
            Some(main.as_str()),
            "{label}: a candidate overwrote main"
        );
    }
    assert_eq!(
        fs::read_to_string(&marker).unwrap().lines().count(),
        3,
        "the other writer moved main once per integration round"
    );
}

// ---- reject and rework ---------------------------------------------------------------

/// A reject writes the note, `show` prints it, and the next attempt's verdicts are its own:
/// the old pass never counts for the new sha. Mutation: accept a verdict of the previous
/// attempt for the new sha and this test reaches `review.awaiting_human`.
#[test]
fn rework_after_reject_starts_a_new_attempt_with_its_own_verdicts() {
    let fixture = Fixture::new("merge-rework");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let first = fixture.verifying_with(&head);
    assert_eq!(first.phase, "awaiting_human");
    let first_digest = first.digest.expect("the first digest");

    let rejected =
        review::reject(&fixture.store, &fixture.task, "not good enough").expect("reject");
    assert_eq!(rejected.phase, "rework_pending");
    assert_state(&fixture, ("review", "rework_pending"));
    let note = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(fixture.attempt_label())
        .join("reject.md");
    assert_eq!(
        fs::read_to_string(&note).expect("the note").trim(),
        "not good enough"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        review::rework_reason(&fixture.store, &stored).as_deref(),
        Some("not good enough")
    );
    assert_eq!(
        review::merge(&fixture.store, &fixture.task)
            .expect_err("a rework task does not merge")
            .exit(),
        Exit::State
    );

    // The rework path: the lead's brief, then a new claim, attempt and push.
    plan_ops::add_rework(&fixture.store, &fixture.task, "try again").expect("a rework brief");
    assert_state(&fixture, ("todo", "ready"));
    let claim = attempt_ops::claim(&fixture.store, &fixture.task).expect("the next attempt");
    assert_eq!(claim.attempt, 2);
    assert!(!claim.resume, "a fresh rework starts a session of its own");
    assert_eq!(claim.session_id, format!("{}-2", fixture.task));
    let head2 = fixture.publish(&head, "second.txt", "second\n");
    let finish = Finish {
        attempt_id: claim.attempt_id.clone(),
        token: claim.token.clone(),
        exit_code: 0,
        outcome: "completed".to_owned(),
        base_sha: Some(fixture.base.clone()),
        head_sha: Some(head2.clone()),
        pushed: true,
        failure: None,
    };
    attempt_ops::apply(
        &fixture.store,
        &fixture.task,
        2,
        Some(&finish),
        None,
        Some(0),
    )
    .expect("the second result applies");
    assert_state(&fixture, ("review", "verifying"));
    fixture.commands(&["false"]);

    let second = review::verify(&fixture.store, &fixture.task).expect("the second verify runs");
    assert_eq!(second.phase, "rework_pending");
    assert!(
        !second.reused.contains(&Stage::Commands),
        "the new attempt ran its own command: {second:?}"
    );
    let old = fixture.verdict(&fixture.attempt_label(), Stage::Commands);
    assert!(old.is_pass());
    assert_eq!(old.sha, head);
    let new_label = format!("{}-2", fixture.task);
    let new = fixture.verdict(&new_label, Stage::Commands);
    assert!(!new.is_pass(), "the second attempt's command failed");
    assert_eq!(new.sha, head2);
    let verdicts = vec![old.clone(), new.clone()];
    assert_ne!(
        verdict_set_digest(&verdicts, &head2),
        first_digest,
        "the new sha has its own set"
    );
    let config = fixture.store.config().unwrap();
    let profile = config.gate.selected_profile().expect("the profile");
    assert!(
        !required_passed(profile, &[], &verdicts, &head2),
        "the old attempt's pass never counts for the new sha"
    );
    let stages = effective_stages(profile, &[]);
    assert_eq!(
        stages,
        vec![
            radian_arc::gate::StageSpec {
                stage: Stage::Commands,
                required: true
            },
            radian_arc::gate::StageSpec {
                stage: Stage::Human,
                required: true
            },
        ]
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending")
    );
    assert!(stored.verdict_set_digest.is_none());
    let reason = review::rework_reason(&fixture.store, &stored).expect("the new reason");
    assert!(reason.contains("commands: command_failed"), "{reason}");
}

/// `show` and the MCP `show` tool share the JSON: the rework reason is in it, and a reject
/// note is what the lead reads.
#[test]
fn show_json_carries_the_rework_reason() {
    let fixture = Fixture::new("merge-show");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    fixture.verifying_with(&head);
    review::reject(&fixture.store, &fixture.task, "the plan wanted less").expect("reject");
    let value: Value =
        radian_arc::store::cli::show_json(&fixture.store, &fixture.task).expect("show json");
    assert_eq!(value["rework_reason"], "the plan wanted less");
    assert_eq!(value["phase"], "rework_pending");
    assert_eq!(value["state"], "review");
    let _ = Path::new(".");
}

// ---- the review stage (ADR 0048 decision 10) -----------------------------------------

/// One review item in the object form.
fn review_item(level: &str, file: &str, line: u32, text: &str) -> Value {
    serde_json::json!({"level": level, "file": file, "line": line, "text": text})
}

/// A session's final message with one fenced report whose items are objects.
fn review_object_message(summary: &str, blocking: &[Value], suggestions: &[Value]) -> String {
    let value = serde_json::json!({
        "summary": summary,
        "blocking": blocking,
        "suggestions": suggestions,
        "verified": ["the diff reads correctly"],
    });
    format!(
        "Here is my review.\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(&value).expect("the report serializes")
    )
}

/// A file with `count` numbered lines.
fn numbered(count: u32) -> String {
    (1..=count).map(|n| format!("line {n}\n")).collect()
}

fn garbage() -> String {
    "I could not review this.".to_owned()
}

/// The stage's argv names a flag's value, if the flag is there.
fn arg_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|arg| arg == flag)
        .and_then(|index| argv.get(index + 1))
        .map(String::as_str)
}

/// Empty `blocking`: the stage passes, the summary is in the verdict, and the session got
/// the read-only mount. A non-empty `suggestions` list does not fail it.
/// Mutation: let `suggestions` fail the stage → this test sees `review.rework_pending`.
#[test]
fn a_review_with_no_blocking_findings_passes_and_keeps_its_summary() {
    let fixture = Fixture::new("review-pass");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let summary = "The change adds feature.txt with one line and its test.";
    let fake = FakeReview::new(
        "review-pass-fake",
        &[
            review_message(summary, &[], &["a nicer name"]),
            review_message("second run", &[], &[]),
        ],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(verdict.is_pass(), "{:?}", verdict.reason);
    let report = verdict.review.clone().expect("the review report");
    assert_eq!(report.summary, summary);
    assert!(report.blocking.is_empty(), "{report:?}");
    assert_eq!(report.suggestions, vec![ReviewItem::text("a nicer name")]);
    assert_eq!(verdict.sha, head);
    assert_eq!(verdict.main_sha.as_deref(), Some(fixture.base.as_str()));
    assert_eq!(verdict.candidate_sha.as_deref(), Some(head.as_str()));
    assert_eq!(verdict.review_session_ids, vec!["fake-review-1".to_owned()]);
    assert_eq!(verdict.review_cost_micros, Some(2000));
    assert_eq!(fake.count(), 1, "one usable run is enough");

    // The session is read-only: exactly the three read tools, and no MCP server.
    let argv = fake.argv(1);
    assert_eq!(arg_value(&argv, "--tools"), Some("read,grep,glob"));
    assert_eq!(
        argv.iter().filter(|arg| *arg == "--tools").count(),
        1,
        "the tool mount may not be repeated: {argv:?}"
    );
    assert!(argv.iter().any(|arg| arg == "--no-config-mcp"));
    assert_eq!(arg_value(&argv, "--profile"), Some("workspace"));
    assert_eq!(arg_value(&argv, "--output"), Some("jsonl"));
    assert!(
        !argv.iter().any(|arg| arg.contains("shell")),
        "a shell tool reached the session: {argv:?}"
    );

    // The built-in prompt reaches the child's stdin.
    let prompt = fake.prompt(1);
    assert!(prompt.contains("the brief"), "{prompt}");
    assert!(
        prompt.contains(&format!("{}..{}", fixture.base, head)),
        "{prompt}"
    );
    assert!(prompt.contains("\"blocking\""), "{prompt}");
}

/// The review session reviews the task's own change only. Main was merged into the branch
/// before verification, so the range starts at the merge base of main and the commit, not at
/// the recorded `base_sha`. Mutation: start the range at `base_sha` and the prompt holds
/// main's file too.
#[test]
fn the_review_diff_after_a_merge_holds_only_the_tasks_own_files() {
    let fixture = Fixture::new("review-diff-own");
    fixture.add_review_stage();
    fixture.commands(&["true"]);
    let head = fixture.publish(&fixture.base.clone(), "own.txt", "own\n");
    fixture.repo.advance_main("main-only.txt", "main\n");

    // The step before an attempt merged main into the branch, and the worker pushed it.
    let branch = format!("work/{}", fixture.task);
    fixture.repo.git(&["checkout", &branch]);
    fixture.repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
    let merged = fixture.repo.rev("HEAD");
    assert_ne!(merged, head, "the branch contains a merge commit");
    fixture.repo.git(&["push", "origin", &branch]);

    let fake = FakeReview::new(
        "review-diff-own-fake",
        &[review_message("The change adds own.txt.", &[], &[])],
        0,
    );
    let verified = fixture.verifying_with_review(&merged, fake.program());
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");

    let prompt = fake.prompt(1);
    assert!(prompt.contains("own.txt"), "{prompt}");
    assert!(
        !prompt.contains("main-only.txt"),
        "the prompt holds main's change:\n{prompt}"
    );
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert_eq!(verdict.sha, merged);
}

/// A blocking item fails the stage: the task goes to rework exactly as after a failed
/// `commands`, and the reason lists the item for the lead.
#[test]
fn a_blocking_finding_sends_the_task_to_rework_with_the_item_in_the_reason() {
    let fixture = Fixture::new("review-blocking");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-blocking-fake",
        &[review_message(
            "The change adds a file.",
            &["rename the helper", "add a test"],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(!verdict.is_pass());
    assert_eq!(verdict.reason.as_deref(), Some("review_blocking"));
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        stored.verdict_set_digest.is_none(),
        "no digest without a pass"
    );
    let reason = review::rework_reason(&fixture.store, &stored).expect("a rework reason");
    assert!(reason.contains("review_blocking"), "{reason}");
    assert!(reason.contains("rename the helper"), "{reason}");
    assert!(reason.contains("add a test"), "{reason}");
    // A plain string in the report is an item with only its text.
    let report = verdict.review.clone().expect("a report");
    assert_eq!(
        report.blocking,
        vec![
            ReviewItem::text("rename the helper"),
            ReviewItem::text("add a test"),
        ]
    );
    assert!(!report.blocking[0].pre_existing);

    // The rework path is the same as after a failed `commands` stage.
    plan_ops::add_rework(&fixture.store, &fixture.task, "fix the review").expect("a rework");
    assert_eq!(
        fixture.store.load_task(&fixture.task).unwrap().item.state,
        TaskState::Todo
    );
}

/// Two unusable outputs fail the stage with `review_output`; both session ids are recorded.
/// Mutation: fail after the first unusable run → this test sees one session id.
#[test]
fn two_unusable_outputs_fail_the_stage_with_review_output() {
    let fixture = Fixture::new("review-output");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new("review-output-fake", &[garbage(), garbage()], 0);

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(!verdict.is_pass());
    assert_eq!(verdict.reason.as_deref(), Some("review_output"));
    assert!(verdict.review.is_none(), "there was no usable report");
    assert_eq!(
        verdict.review_session_ids,
        vec!["fake-review-1".to_owned(), "fake-review-2".to_owned()]
    );
    assert_eq!(fake.count(), 2, "the session ran once more");
}

/// Garbage once then a valid report: the second run passes the stage.
#[test]
fn an_unusable_output_runs_the_session_once_more() {
    let fixture = Fixture::new("review-retry");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-retry-fake",
        &[garbage(), review_message("second time is valid", &[], &[])],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(verdict.is_pass());
    assert_eq!(
        verdict
            .review
            .as_ref()
            .map(|report| report.summary.as_str()),
        Some("second time is valid")
    );
    assert_eq!(verdict.review_session_ids.len(), 2);
    assert_eq!(fake.count(), 2);
}

/// The two stages start together: each program waits for the other's start marker, so neither
/// can finish before the other has started. Mutation: run them one after the other and the
/// `commands` wait times out, fails, and the review never meets its side.
#[test]
fn the_commands_and_review_stages_start_before_either_ends() {
    let fixture = Fixture::new("review-parallel-start");
    fixture.add_review_stage();
    let dir = TempDir::new("review-parallel-start-marks");
    let commands_started = dir.path().join("commands.started");
    let fake = FakeReview::with_barrier(
        "review-parallel-start-fake",
        &[review_message("both ran", &[], &[])],
        0,
        Some((&commands_started, 5)),
    );
    let review_started = fake.started(1);
    let script = format!(
        "touch '{commands}'; i=0; while [ ! -f '{review}' ] && [ $i -lt 100 ]; do \
         i=$((i+1)); sleep 0.05; done; test -f '{review}'",
        commands = commands_started.display(),
        review = review_started.display(),
    );
    fixture.commands(&[script.as_str()]);
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    assert!(
        fixture
            .verdict(&fixture.attempt_label(), Stage::Commands)
            .is_pass()
    );
    assert!(
        fixture
            .verdict(&fixture.attempt_label(), Stage::Review)
            .is_pass()
    );
    assert_eq!(fake.count(), 1, "the review session ran once");
}

/// A `commands` failure decides the round, but the review still ran and wrote its verdict: the
/// cost counts and `show` can list its items. Mutation: stop at the commands verdict and the
/// review verdict is missing.
#[test]
fn a_commands_failure_decides_the_round_and_the_review_verdict_is_recorded() {
    let fixture = Fixture::new("review-commands-fail");
    fixture.add_review_stage();
    fixture.commands(&["false"]);
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-commands-fail-fake",
        &[review_message("the change is fine", &[], &[])],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("review", "rework_pending"),
        "{verified:?}"
    );
    let reason = verified.reason.clone().expect("a reason");
    assert_eq!(reason.stage, Some(Stage::Commands));
    assert_eq!(reason.reason, "command_failed");
    assert!(
        verified.review.is_none(),
        "a commands failure writes no review rework"
    );
    let review = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(review.is_pass(), "{:?}", review.reason);
    assert_eq!(
        review.review.as_ref().map(|report| report.summary.as_str()),
        Some("the change is fine")
    );
    assert!(
        !fixture
            .verdict(&fixture.attempt_label(), Stage::Commands)
            .is_pass()
    );
    assert_eq!(fake.count(), 1, "the review session ran");
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    let shown = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(shown.contains("commands: command_failed"), "{shown}");
    assert!(!stored.body.contains("(review)"), "{}", stored.body);
}

/// `commands` passes and the review fails: today's path, the automatic review rework.
#[test]
fn a_review_failure_after_a_passing_commands_stage_writes_the_automatic_rework() {
    let fixture = Fixture::new("review-after-commands-pass");
    fixture.add_review_stage();
    fixture.commands(&["true"]);
    fixture.set_auto("run");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-after-commands-pass-fake",
        &[review_message(
            "the change needs a test",
            &["add a test"],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );
    let rework = verified.review.clone().expect("the review rework");
    assert_eq!(rework.items, vec![ReviewItem::text("add a test")]);
    assert!(
        fixture
            .verdict(&fixture.attempt_label(), Stage::Commands)
            .is_pass()
    );
    let review = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(!review.is_pass());
    assert_eq!(review.reason.as_deref(), Some("review_blocking"));
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        stored.body.contains("## Rework 2 (review)"),
        "{}",
        stored.body
    );
    assert_eq!(stored.auto_rework, 1, "the rework is automatic");
}

/// Both fail: the round follows the profile order, so `commands` decides; the review verdict is
/// recorded but no automatic review rework is written. Mutation: let the review failure decide
/// and the task returns to `todo.ready` with a review brief.
#[test]
fn a_commands_failure_decides_the_round_when_the_review_fails_too() {
    let fixture = Fixture::new("review-both-fail");
    fixture.add_review_stage();
    fixture.commands(&["false"]);
    fixture.set_auto("run");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-both-fail-fake",
        &[review_message(
            "the change needs a test",
            &["add a test"],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("review", "rework_pending"),
        "{verified:?}"
    );
    let reason = verified.reason.clone().expect("a reason");
    assert_eq!(reason.stage, Some(Stage::Commands));
    assert_eq!(reason.reason, "command_failed");
    assert!(
        verified.review.is_none(),
        "a commands failure writes no review rework"
    );
    let review = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert!(!review.is_pass());
    assert_eq!(review.reason.as_deref(), Some("review_blocking"));
    assert_eq!(
        review.review.as_ref().map(|report| report.blocking.len()),
        Some(1),
        "the review verdict keeps its items for `show`"
    );
    assert_eq!(fake.count(), 1);
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(
        !stored.body.contains("## Rework"),
        "a review rework was written: {}",
        stored.body
    );
    assert_eq!(stored.auto_rework, 0);
    let shown = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(shown.contains("commands: command_failed"), "{shown}");
}

/// A review verdict is part of the digest: a new review of the same sha changes it.
/// Mutation: leave `review` out of the counted verdicts → the two digests are equal.
#[test]
fn a_new_review_of_the_same_sha_changes_the_digest() {
    let fixture = Fixture::new("review-digest");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-digest-fake",
        &[
            review_message("first review", &[], &[]),
            review_message("second review", &[], &[]),
        ],
        0,
    );
    let first = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(first.phase, "awaiting_human", "{first:?}");
    let first_digest = first.digest.clone().expect("a digest");

    let reverified = review::reverify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("reverify");
    let second = reverified.verification.expect("the verification ran");
    assert_eq!(second.phase, "awaiting_human", "{second:?}");
    assert_ne!(
        second.digest.as_deref(),
        Some(first_digest.as_str()),
        "a new review of the same sha must change the digest"
    );
    let new_label = format!("{}-2", fixture.task);
    assert_eq!(
        fixture
            .verdict(&new_label, Stage::Review)
            .review
            .as_ref()
            .map(|report| report.summary.as_str()),
        Some("second review")
    );
    assert_eq!(
        fixture
            .verdict(&fixture.attempt_label(), Stage::Review)
            .review
            .as_ref()
            .map(|report| report.summary.as_str()),
        Some("first review"),
        "the old verdict is never rewritten"
    );
}

/// `reverify` re-runs the review for the same commit without a worker, and the new attempt's
/// verdict is its own.
#[test]
fn reverify_reruns_review_and_its_verdict_is_its_own() {
    let fixture = Fixture::new("review-reverify");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-reverify-fake",
        &[
            review_message("still broken", &["add the missing test"], &[]),
            review_message("now it is right", &[], &[]),
        ],
        0,
    );
    let first = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(first.phase, "rework_pending", "{first:?}");

    let reverified = review::reverify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("reverify");
    let second = reverified.verification.expect("the verification ran");
    assert_eq!(second.phase, "awaiting_human", "{second:?}");
    let new_label = format!("{}-2", fixture.task);
    assert_eq!(reverified.attempt_id, new_label);
    let old = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    let new = fixture.verdict(&new_label, Stage::Review);
    assert!(!old.is_pass(), "the old verdict is unchanged");
    assert!(new.is_pass(), "{:?}", new.reason);
    assert_eq!(new.sha, head);
    assert_eq!(fake.count(), 2, "review ran once per attempt");
}

/// `[gate.review] exec_args` is the session's model route, and it never replaces the
/// read-only mount. Mutation: drop the mount or let `exec_args` override `--tools` → this
/// test sees another tool list.
#[test]
fn the_review_exec_args_reach_the_child_and_the_tools_stay_read_only() {
    let fixture = Fixture::new("review-exec-args");
    fixture.add_review_stage();
    fixture.review_exec_args(&[
        "--model",
        "review-model",
        "--base-url",
        "http://127.0.0.1:9",
    ]);
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-exec-args-fake",
        &[review_message("a passing change", &[], &[])],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    let argv = fake.argv(1);
    assert_eq!(arg_value(&argv, "--model"), Some("review-model"));
    assert_eq!(arg_value(&argv, "--base-url"), Some("http://127.0.0.1:9"));
    assert_eq!(arg_value(&argv, "--tools"), Some("read,grep,glob"));
    let session = format!("{}-review-1", fixture.attempt_label());
    assert_eq!(arg_value(&argv, "--session-id"), Some(session.as_str()));
}

/// An `exec_args` entry that repeats an argument the stage owns is refused by name, and the
/// session does not run: it could unmount the read-only tools.
#[test]
fn an_exec_arg_the_review_stage_owns_is_refused() {
    let fixture = Fixture::new("review-owned-args");
    fixture.add_review_stage();
    fixture.review_exec_args(&["--tools", "shell"]);
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-owned-args-fake",
        &[review_message("must not run", &[], &[])],
        0,
    );
    fixture.set_verifying(&head);

    let error = review::verify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect_err("the stage owns --tools");
    assert_eq!(error.exit(), Exit::State);
    assert!(
        error.message().contains("gate.review.exec_args"),
        "{error:?}"
    );
    assert!(error.message().contains("--tools"), "{error:?}");
    assert_eq!(fake.count(), 0, "the session ran");
    assert_state(&fixture, ("review", "verifying"));
}

/// `[gate.review] timeout_secs` stops a session; two timeouts fail with `review_timeout`.
#[test]
fn a_review_that_times_out_twice_fails_with_review_timeout() {
    let fixture = Fixture::new("review-timeout");
    fixture.add_review_stage();
    fixture.review_timeout(1);
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-timeout-fake",
        &[review_message("too slow", &[], &[])],
        30,
    );

    let started = std::time::Instant::now();
    let verified = fixture.verifying_with_review(&head, fake.program());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the timeout did not stop the sessions"
    );
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert_eq!(verdict.reason.as_deref(), Some("review_timeout"));
    assert_eq!(verdict.review_session_ids.len(), 2);
    assert_eq!(fake.count(), 2);
}

/// `[gate.review] prompt_file` is appended to the built-in prompt.
#[test]
fn the_review_prompt_file_is_appended() {
    let fixture = Fixture::new("review-prompt-file");
    fixture.add_review_stage();
    fixture.review_prompt_file("review.md");
    fs::write(
        fixture.repo.repo.join("review.md"),
        "This repository wants every claim cited.\n",
    )
    .expect("the prompt file");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-prompt-file-fake",
        &[review_message("a passing change", &[], &[])],
        0,
    );

    assert_eq!(
        fixture.verifying_with_review(&head, fake.program()).phase,
        "awaiting_human"
    );
    let prompt = fake.prompt(1);
    assert!(
        prompt.contains("This repository wants every claim cited."),
        "{prompt}"
    );
    assert!(
        prompt.contains("Check the change against the brief"),
        "the built-in prompt is still there: {prompt}"
    );
}

/// `show --json` and the CLI text expose the newest review verdict's fields.
#[test]
fn show_exposes_the_review_findings() {
    let fixture = Fixture::new("review-show");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "review-show-fake",
        &[review_message(
            "the change adds one line",
            &[],
            &["cite it"],
        )],
        0,
    );
    fixture.verifying_with_review(&head, fake.program());

    let value: Value =
        radian_arc::store::cli::show_json(&fixture.store, &fixture.task).expect("show json");
    assert_eq!(value["review"]["summary"], "the change adds one line");
    assert_eq!(value["review"]["blocking"], serde_json::json!([]));
    assert_eq!(value["review"]["suggestions"][0], "cite it");
    assert!(
        value["review_verdict"]
            .as_str()
            .expect("the verdict path")
            .ends_with("verdicts/review.json"),
        "{value}"
    );
    assert_eq!(value["review_session_ids"][0], "fake-review-1");
    assert_eq!(value["review_cost_micros"], 2000);
}

// ---- a gate re-run's review sessions ------------------------------------------------

/// A gate that died mid-review is started again for the same attempt. The re-run must not
/// reuse a session id whose journal exists: it takes `<attempt>-review-<run>-<k>` and writes
/// its logs under the matching name, and the first journal is untouched.
#[test]
fn a_gate_rerun_gets_a_fresh_review_session_and_keeps_the_first_journal() {
    let fixture = Fixture::new("review-rerun-sessions");
    fixture.add_review_stage();
    let session_dir = common::scratch("review-sessions");
    fixture.review_exec_args(&["--model", "m", "--session-dir", common::path(&session_dir)]);
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let attempt = fixture.attempt_label();

    let first = FakeReview::new(
        "review-rerun-first",
        &[review_message("first pass", &[], &[])],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, first.program()).phase,
        "awaiting_human"
    );
    // The first run's journal, left in the session directory.
    let journal = session_dir.join(format!("{attempt}-review-1.jsonl"));
    fs::write(&journal, "the first journal\n").expect("the first journal");
    let before = fs::read(&journal).expect("the first journal reads");
    let first_log = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(&attempt)
        .join("logs/review-1.jsonl");
    assert!(
        first_log.is_file(),
        "the first run's log: {}",
        first_log.display()
    );

    // The gate died before it wrote the verdict: remove it, as a death mid-review leaves it.
    let verdict_path = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(&attempt)
        .join("verdicts/review.json");
    fs::remove_file(&verdict_path).expect("the review verdict is removed");
    fixture.set_verifying(&head);

    let second = FakeReview::new(
        "review-rerun-second",
        &[review_message("second pass", &[], &[])],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, second.program()).phase,
        "awaiting_human"
    );
    assert_eq!(second.count(), 1);
    let argv = second.argv(1);
    let requested = argv
        .iter()
        .position(|arg| arg == "--session-id")
        .map(|at| argv[at + 1].clone())
        .expect("the stage passes a session id");
    assert_eq!(
        requested,
        format!("{attempt}-review-1-1"),
        "the re-run took the next free session id"
    );
    assert_eq!(
        fs::read(&journal).expect("the first journal reads"),
        before,
        "the first journal is unchanged byte for byte"
    );
    let second_log = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(&attempt)
        .join("logs/review-1-1.jsonl");
    assert!(
        second_log.is_file(),
        "the re-run's log has the matching name: {}",
        second_log.display()
    );
    let verdict = fixture.verdict(&attempt, Stage::Review);
    assert_eq!(verdict.result, VerdictResult::Pass);
    assert!(!verdict.review_session_ids.is_empty());
    let _ = fs::remove_dir_all(&session_dir);
}

// ---- the rework round (ADR 0048 decision 10) ------------------------------------------

/// The first review of a task is unchanged: its prompt holds the whole diff of the task's own
/// change, and a blocking item with a place blocks.
#[test]
fn the_first_review_prompt_holds_the_full_diff_and_its_item_blocks() {
    let fixture = Fixture::new("review-first-unchanged");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "one.txt", "one\n");
    let fake = FakeReview::new(
        "review-first-unchanged-fake",
        &[review_object_message(
            "the change adds one.txt",
            &[review_item("major", "one.txt", 1, "the line is wrong")],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let prompt = fake.prompt(1);
    assert!(prompt.contains("## The diff"), "{prompt}");
    assert!(prompt.contains("diff --git a/one.txt"), "{prompt}");
    assert!(prompt.contains("+one"), "{prompt}");
    assert!(!prompt.contains("## The previous review"), "{prompt}");
    let verdict = fixture.verdict(&fixture.attempt_label(), Stage::Review);
    assert_eq!(verdict.reason.as_deref(), Some("review_blocking"));
    assert_eq!(
        verdict.review.expect("a report").blocking,
        vec![ReviewItem {
            level: Some("major".to_owned()),
            file: Some("one.txt".to_owned()),
            line: Some(1),
            text: "the line is wrong".to_owned(),
            pre_existing: false,
        }]
    );
    assert_eq!(
        verdict.review_pre_existing, None,
        "a first review moves nothing"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending"),
        "auto is off, so the lead gets the task"
    );
}

/// A rework round's prompt holds the increment and the previous blocking items, and not a file
/// that only the first attempt changed. Mutation: treat the whole own diff as the increment →
/// the prompt holds one.txt's hunk again.
#[test]
fn a_rework_prompt_holds_the_increment_and_the_previous_items() {
    let fixture = Fixture::new("review-rework-prompt");
    fixture.add_review_stage();
    let first = fixture.publish(&fixture.base.clone(), "one.txt", "one\n");
    let first_fake = FakeReview::new(
        "review-rework-prompt-first",
        &[review_object_message(
            "the change adds one.txt",
            &[review_item("major", "one.txt", 1, "the line is wrong")],
            &[],
        )],
        0,
    );
    assert_eq!(
        fixture
            .verifying_with_review(&first, first_fake.program())
            .phase,
        "rework_pending"
    );

    let second = fixture.rework(&first, "two.txt", "two\n");
    let fake = FakeReview::new(
        "review-rework-prompt-second",
        &[review_object_message(
            "the rework fixes it and adds two.txt",
            &[],
            &[],
        )],
        0,
    );
    let verified = review::verify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("the rework round runs");
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");

    let prompt = fake.prompt(1);
    assert!(prompt.contains("## The previous review"), "{prompt}");
    assert!(
        prompt.contains("- [major] one.txt:1 the line is wrong"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("{first}..{second}")),
        "the commit list is the rework's range: {prompt}"
    );
    assert!(prompt.contains("## The increment"), "{prompt}");
    assert!(prompt.contains("diff --git a/two.txt"), "{prompt}");
    assert!(
        !prompt.contains("diff --git a/one.txt"),
        "a file only the first attempt changed is in the increment:\n{prompt}"
    );
    assert!(
        prompt.contains("Check that each blocking item of the previous review is fixed"),
        "{prompt}"
    );
    assert!(!prompt.contains("## The diff"), "{prompt}");
    assert_eq!(
        fixture
            .verdict(&format!("{}-2", fixture.task), Stage::Review)
            .review_pre_existing,
        Some(0),
        "a rework round records the count, even when it moved nothing"
    );
}

/// A blocking item on a line the rework did not touch is moved to `suggestions` with
/// `pre_existing`, the stage passes, and the verdict records the move. Mutation: skip the move
/// → the stage stays in `review.rework_pending`.
#[test]
fn a_blocking_item_the_rework_did_not_touch_is_moved_and_the_stage_passes() {
    let fixture = Fixture::new("review-pre-existing");
    fixture.add_review_stage();
    let first = fixture.publish(&fixture.base.clone(), "old.txt", &numbered(40));
    let first_fake = FakeReview::new(
        "review-pre-existing-first",
        &[review_object_message(
            "the change adds old.txt",
            &[review_item(
                "major",
                "old.txt",
                1,
                "the first line is wrong",
            )],
            &[],
        )],
        0,
    );
    assert_eq!(
        fixture
            .verifying_with_review(&first, first_fake.program())
            .phase,
        "rework_pending"
    );

    // The rework adds another file and leaves old.txt alone; the review now finds a problem
    // far from its previous one.
    fixture.rework(&first, "new.txt", "new\n");
    let fake = FakeReview::new(
        "review-pre-existing-second",
        &[review_object_message(
            "the rework adds new.txt",
            &[review_item(
                "minor",
                "old.txt",
                30,
                "this line was always odd",
            )],
            &[],
        )],
        0,
    );
    let verified = review::verify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("the rework round runs");
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");

    let verdict = fixture.verdict(&format!("{}-2", fixture.task), Stage::Review);
    assert!(verdict.is_pass(), "{:?}", verdict.reason);
    assert_eq!(verdict.review_pre_existing, Some(1));
    let report = verdict.review.expect("a report");
    assert!(report.blocking.is_empty(), "{report:?}");
    assert_eq!(
        report.suggestions,
        vec![ReviewItem {
            level: Some("minor".to_owned()),
            file: Some("old.txt".to_owned()),
            line: Some(30),
            text: "this line was always odd".to_owned(),
            pre_existing: true,
        }]
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert!(stored.verdict_set_digest.is_some(), "the stage passed");
}

/// A blocking item on an increment line blocks: the rework introduced it. Mutation: treat every
/// item as pre-existing → this test sees `awaiting_human`.
#[test]
fn a_blocking_item_on_an_increment_line_blocks() {
    let fixture = Fixture::new("review-increment-blocks");
    fixture.add_review_stage();
    let first = fixture.publish(&fixture.base.clone(), "one.txt", "one\n");
    let first_fake = FakeReview::new(
        "review-increment-blocks-first",
        &[review_object_message("the change adds one.txt", &[], &[])],
        0,
    );
    assert_eq!(
        fixture
            .verifying_with_review(&first, first_fake.program())
            .phase,
        "awaiting_human"
    );

    // A reverify of the same sha is a rework round with an empty increment, so the review
    // starts from the previous pass: give it a new attempt with a blocking item on the line
    // the rework adds.
    fixture.rework(&first, "two.txt", "two\n");
    let fake = FakeReview::new(
        "review-increment-blocks-second",
        &[review_object_message(
            "the rework adds two.txt",
            &[review_item(
                "critical",
                "two.txt",
                1,
                "the new line is wrong",
            )],
            &[],
        )],
        0,
    );
    let verified = review::verify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("the rework round runs");
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&format!("{}-2", fixture.task), Stage::Review);
    assert_eq!(verdict.reason.as_deref(), Some("review_blocking"));
    assert_eq!(verdict.review_pre_existing, Some(0));
    assert_eq!(
        verdict.review.expect("a report").blocking.len(),
        1,
        "the item on the increment line stayed blocking"
    );
}

/// Rule (b): an item in the same file as, and within 10 lines of, a blocking item of the
/// previous review stays blocking, so an unfixed earlier item is not waved through. Mutation:
/// drop rule (b) → the item is moved and the stage passes.
#[test]
fn a_blocking_item_near_an_unfixed_previous_item_still_blocks() {
    let fixture = Fixture::new("review-near-previous");
    fixture.add_review_stage();
    let first = fixture.publish(&fixture.base.clone(), "old.txt", &numbered(40));
    let first_fake = FakeReview::new(
        "review-near-previous-first",
        &[review_object_message(
            "the change adds old.txt",
            &[review_item(
                "major",
                "old.txt",
                10,
                "the tenth line is wrong",
            )],
            &[],
        )],
        0,
    );
    assert_eq!(
        fixture
            .verifying_with_review(&first, first_fake.program())
            .phase,
        "rework_pending"
    );

    fixture.rework(&first, "new.txt", "new\n");
    let fake = FakeReview::new(
        "review-near-previous-second",
        &[review_object_message(
            "the rework adds new.txt",
            &[review_item(
                "major",
                "old.txt",
                13,
                "still the same problem",
            )],
            &[],
        )],
        0,
    );
    let verified = review::verify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("the rework round runs");
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    let verdict = fixture.verdict(&format!("{}-2", fixture.task), Stage::Review);
    assert_eq!(verdict.review_pre_existing, Some(0));
    assert!(
        !verdict.review.expect("a report").blocking.is_empty(),
        "an unfixed earlier item stopped blocking"
    );
}

/// A main change merged in between is not part of the increment: the task's own diff leaves it
/// out, so a blocking item on it is pre-existing. Mutation: use `previous..sha` alone as the
/// increment → the stage blocks on main's file.
#[test]
fn a_main_change_merged_between_attempts_is_not_in_the_increment() {
    let fixture = Fixture::new("review-main-increment");
    fixture.add_review_stage();
    let first = fixture.publish(&fixture.base.clone(), "own.txt", "one\n");
    let first_fake = FakeReview::new(
        "review-main-increment-first",
        &[review_object_message(
            "the change adds own.txt",
            &[review_item("major", "own.txt", 1, "the line is wrong")],
            &[],
        )],
        0,
    );
    assert_eq!(
        fixture
            .verifying_with_review(&first, first_fake.program())
            .phase,
        "rework_pending"
    );

    // Main moves; the rework merges it in and changes the task's own file.
    fixture.repo.advance_main("main-only.txt", "main\n");
    plan_ops::add_rework(&fixture.store, &fixture.task, "fix the review").expect("a rework brief");
    let claim = attempt_ops::claim(&fixture.store, &fixture.task).expect("the next attempt");
    let branch = format!("work/{}", fixture.task);
    fixture.repo.git(&["checkout", &branch]);
    fixture.repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
    write(&fixture.repo.repo.join("own.txt"), "one fixed\n");
    fixture.repo.git(&["add", "own.txt"]);
    fixture.repo.git(&["commit", "-m", "fix own.txt"]);
    let second = fixture.repo.rev("HEAD");
    fixture.repo.git(&["push", "origin", &branch]);
    let finish = Finish {
        attempt_id: claim.attempt_id.clone(),
        token: claim.token.clone(),
        exit_code: 0,
        outcome: "completed".to_owned(),
        base_sha: Some(fixture.base.clone()),
        head_sha: Some(second.clone()),
        pushed: true,
        failure: None,
    };
    attempt_ops::apply(
        &fixture.store,
        &fixture.task,
        claim.attempt,
        Some(&finish),
        None,
        Some(0),
    )
    .expect("the rework result applies");

    let fake = FakeReview::new(
        "review-main-increment-second",
        &[review_object_message(
            "the rework fixes own.txt",
            &[review_item(
                "minor",
                "main-only.txt",
                1,
                "main's line is odd",
            )],
            &[],
        )],
        0,
    );
    let verified = review::verify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("the rework round runs");
    assert_eq!(verified.phase, "awaiting_human", "{verified:?}");
    let prompt = fake.prompt(1);
    assert!(prompt.contains("diff --git a/own.txt"), "{prompt}");
    assert!(
        !prompt.contains("diff --git a/main-only.txt"),
        "main's change reached the increment:\n{prompt}"
    );
    assert!(
        prompt.contains("main: main-only.txt"),
        "the commit list holds main's commit: {prompt}"
    );
    let verdict = fixture.verdict(&format!("{}-2", fixture.task), Stage::Review);
    assert_eq!(verdict.review_pre_existing, Some(1));
}

/// A review of the commit the previous review already reviewed is a first review: a `reverify`
/// of an unchanged head still blocks on the same item even when the session reports it more than
/// 10 lines from where it reported it before. Mutation: treat a same-sha review as a rework
/// round → the item is moved and the stage passes.
#[test]
fn a_reverify_of_the_same_head_is_a_first_review_and_still_blocks() {
    let fixture = Fixture::new("review-same-sha");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "old.txt", &numbered(40));
    let first = FakeReview::new(
        "review-same-sha-first",
        &[review_object_message(
            "the change adds old.txt",
            &[review_item(
                "major",
                "old.txt",
                1,
                "the first line is wrong",
            )],
            &[],
        )],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, first.program()).phase,
        "rework_pending"
    );

    let fake = FakeReview::new(
        "review-same-sha-second",
        &[review_object_message(
            "the same problem, reported from elsewhere",
            &[review_item("major", "old.txt", 30, "the same wrong line")],
            &[],
        )],
        0,
    );
    let reverified = review::reverify_with(&fixture.store, &fixture.task, Some(fake.program()))
        .expect("reverify runs");
    let verified = reverified.verification.expect("the verification runs");
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");

    let label = format!("{}-2", fixture.task);
    let verdict = fixture.verdict(&label, Stage::Review);
    assert_eq!(verdict.reason.as_deref(), Some("review_blocking"));
    assert_eq!(verdict.sha, head, "the same commit is reviewed again");
    assert_eq!(
        verdict.review_pre_existing, None,
        "a first review moves nothing"
    );
    assert_eq!(
        verdict.review.expect("a report").blocking.len(),
        1,
        "an item 29 lines from the previous one still blocks"
    );
    let prompt = fake.prompt(1);
    assert!(prompt.contains("## The diff"), "{prompt}");
    assert!(!prompt.contains("## The previous review"), "{prompt}");
    assert!(prompt.contains("diff --git a/old.txt"), "{prompt}");
}

/// A failed review on a plan at `auto` `merge` writes `## Rework <n> (review)` with the items
/// and moves the task to `todo.ready`, and counts one automatic rework. Mutation: write the
/// rework when `auto` is `off` → this test's `auto` `off` twin fails instead.
#[test]
fn a_failed_review_with_auto_merge_writes_its_own_rework() {
    let fixture = Fixture::new("review-auto-rework");
    fixture.add_review_stage();
    fixture.set_auto("merge");
    let head = fixture.publish(&fixture.base.clone(), "one.txt", "one\n");
    let fake = FakeReview::new(
        "review-auto-rework-fake",
        &[review_object_message(
            "the change adds one.txt",
            &[
                review_item("critical", "one.txt", 1, "the line is wrong"),
                review_item("minor", "one.txt", 1, "and the name is odd"),
            ],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("todo", "ready"),
        "{verified:?}"
    );
    let rework = verified.review.clone().expect("the review rework");
    assert_eq!(rework.items.len(), 2);
    assert_eq!(
        verified.line(),
        format!(
            "{} review blocking -> todo.ready (automatic rework)",
            fixture.task
        )
    );

    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert_eq!(stored.auto_rework, 1, "the rework counts");
    assert_eq!(
        stored.verified_sha.as_deref(),
        Some(head.as_str()),
        "the sha the review failed on is recorded, as the lead-rework path records it"
    );
    assert!(stored.body.starts_with("the brief"), "{}", stored.body);
    assert!(
        stored.body.contains("## Rework 2 (review)"),
        "{}",
        stored.body
    );
    assert!(
        stored.body.contains(
            "The review found these items. Fix them and nothing else, append commits to the \
             branch, and do not rebase."
        ),
        "{}",
        stored.body
    );
    assert!(
        stored
            .body
            .contains("- [critical] one.txt:1 the line is wrong"),
        "{}",
        stored.body
    );
    assert!(
        stored
            .body
            .contains("- [minor] one.txt:1 and the name is odd"),
        "{}",
        stored.body
    );
    let verdict_path = fixture
        .store
        .root()
        .join("runs")
        .join(&fixture.task)
        .join("attempts")
        .join(fixture.attempt_label())
        .join("verdicts/review.json");
    assert!(
        stored.body.contains(&format!(
            "Read the verdict for detail: `{}`",
            verdict_path.display()
        )),
        "{}",
        stored.body
    );
    assert_eq!(
        fixture
            .store
            .load_plan(&stored.plan)
            .expect("the plan")
            .item
            .auto,
        "merge",
        "the plan is not paused by the first rework"
    );
}

/// With `auto` `off` the task stays in `review.rework_pending` as before: no brief, no count.
#[test]
fn a_failed_review_with_auto_off_waits_for_the_lead() {
    let fixture = Fixture::new("review-auto-off");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "one.txt", "one\n");
    let fake = FakeReview::new(
        "review-auto-off-fake",
        &[review_object_message(
            "the change adds one.txt",
            &[review_item("major", "one.txt", 1, "the line is wrong")],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(verified.phase, "rework_pending", "{verified:?}");
    assert!(verified.review.is_none(), "no rework was written");
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "rework_pending")
    );
    assert_eq!(stored.auto_rework, 0);
    assert!(
        !stored.body.contains("## Rework"),
        "auto off wrote a brief: {}",
        stored.body
    );
    let reason = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(
        reason.contains("[major] one.txt:1 the line is wrong"),
        "{reason}"
    );
}

/// `max_auto_rework` still pauses the plan: the third automatic review rework sets `auto` to
/// `off`, the task stays in `review.rework_pending`, and `show` says why.
#[test]
fn a_review_rework_at_the_auto_rework_limit_pauses_the_plan() {
    let fixture = Fixture::new("review-auto-limit");
    fixture.add_review_stage();
    fixture.set_auto("merge");
    fixture
        .store
        .change_task(&fixture.task, |task| {
            task.auto_rework = 2;
            Ok(())
        })
        .expect("the count");
    let head = fixture.publish(&fixture.base.clone(), "one.txt", "one\n");
    let fake = FakeReview::new(
        "review-auto-limit-fake",
        &[review_object_message(
            "the change adds one.txt",
            &[review_item("major", "one.txt", 1, "the line is wrong")],
            &[],
        )],
        0,
    );

    let verified = fixture.verifying_with_review(&head, fake.program());
    assert_eq!(
        (verified.state, verified.phase.as_str()),
        ("review", "rework_pending"),
        "{verified:?}"
    );
    let paused = verified.paused.clone().expect("the pause");
    assert_eq!(paused.count, 3);
    assert_eq!(paused.max_auto_rework, 2);
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(stored.auto_rework, 3);
    assert_eq!(stored.verified_sha.as_deref(), Some(head.as_str()));
    assert!(
        !stored.body.contains("## Rework"),
        "a paused plan wrote a brief: {}",
        stored.body
    );
    assert_eq!(
        fixture
            .store
            .load_plan(&stored.plan)
            .expect("the plan")
            .item
            .auto,
        "off"
    );
    let reason = review::rework_reason(&fixture.store, &stored).expect("a reason");
    assert!(reason.contains("automatic reworks"), "{reason}");
}

// ---- the review's follow-up task (ADR 0048 decision 10) ------------------------------

/// A review verdict arc never writes itself: a pass whose report lists `blocking` items. A
/// non-empty `blocking` fails the stage, so this is the only way to reach a merge with a
/// blocking item on record, and it is what proves the follow-up takes none of them.
fn write_passing_review(fixture: &Fixture, head: &str, report: ReviewReport) {
    let path = verdict_path(
        fixture.store.repo(),
        &fixture.task,
        &fixture.attempt_label(),
        Stage::Review,
    )
    .expect("the verdict path");
    let at = "2026-01-01T00:00:00.000000Z".to_owned();
    Verdict {
        schema_version: VERDICT_SCHEMA_VERSION,
        stage: Stage::Review,
        sha: head.to_owned(),
        main_sha: Some(fixture.base.clone()),
        result: VerdictResult::Pass,
        reason: None,
        commands: Vec::new(),
        started_at: at.clone(),
        ended_at: at,
        base_sha: None,
        candidate_sha: Some(head.to_owned()),
        conflict_paths: Vec::new(),
        approver: None,
        review: Some(report),
        review_session_ids: vec!["hand-written".to_owned()],
        review_cost_micros: None,
        review_pre_existing: None,
    }
    .write_new(&path)
    .expect("the review verdict is written once");
}

/// A merge whose review suggested an item: the plan gains one `todo.proposed` follow-up,
/// `after` the merged task, whose brief lists the item and names the verdict.
#[test]
fn a_merge_with_review_suggestions_proposes_one_followup_task() {
    let fixture = Fixture::new("followup");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "followup-fake",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, fake.program()).phase,
        "awaiting_human"
    );

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    let followup = merged.followup.clone().expect("a follow-up task");
    assert_eq!(followup.from, fixture.task);
    let line = merged.line();
    let lines: Vec<&str> = line.lines().collect();
    assert!(
        lines[0].starts_with(&format!("{} done.merged", fixture.task)),
        "{line}"
    );
    assert_eq!(
        lines[1],
        format!(
            "{} proposed (follow-ups from {})",
            followup.task, fixture.task
        ),
        "{line}"
    );

    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    let plan = fixture.store.load_plan(&plan_id).expect("the plan").item;
    assert_eq!(plan.status, "confirmed", "the plan stays confirmed");
    assert_eq!(plan.auto, "off", "the plan's auto level does not change");
    assert_eq!(
        plan.tasks,
        vec![fixture.task.clone(), followup.task.clone()]
    );
    assert!(
        followup.task.starts_with("TK0002-") && followup.task.ends_with("-followups"),
        "the next id with the merged task's slug: {}",
        followup.task
    );

    let task = fixture
        .store
        .load_task(&followup.task)
        .expect("the task")
        .item;
    assert_eq!(
        (task.state.as_str(), task.phase.as_str()),
        ("todo", "proposed")
    );
    assert_eq!(task.plan, plan_id, "the same plan");
    assert_eq!(
        task.after,
        vec![fixture.task.clone()],
        "it waits on the merge"
    );
    assert_eq!(
        task.title,
        format!("Follow-ups from {}'s review", fixture.task)
    );
    assert_eq!(task.attempt, 0);
    let (verdict_path, _) =
        review::latest_review_verdict(&fixture.store, &fixture.task).expect("the review verdict");
    assert!(
        task.body.contains("[minor] feature.txt:3 a nicer name"),
        "the item is in the brief: {}",
        task.body
    );
    assert!(
        task.body.contains(&format!(
            "The items below are from the review of `{}`",
            fixture.task
        )),
        "{}",
        task.body
    );
    assert!(
        task.body.contains(&verdict_path.display().to_string()),
        "the verdict path is in the brief: {}",
        task.body
    );
}

/// The scheduling loop passes over a `todo.proposed` task, and `start` refuses it (exit 3,
/// naming the phase).
///
/// Mutation: let `start_ready` claim a proposed task (and the claim accept it) → the pass
/// moves the follow-up to `doing.starting` and this fails.
#[test]
fn the_loop_does_not_claim_a_proposed_task_and_start_refuses_it() {
    let fixture = Fixture::new("followup-loop");
    fixture.add_review_stage();
    fixture.set_auto("run");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "followup-loop-fake",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    fixture.verifying_with_review(&head, fake.program());
    let followup = review::merge(&fixture.store, &fixture.task)
        .expect("the merge runs")
        .followup
        .expect("a follow-up task")
        .task;

    let mut lines: Vec<String> = Vec::new();
    let report = radian_arc::run::run(&fixture.store, &mut |line| lines.push(line.to_owned()))
        .expect("the pass runs");
    assert!(
        report
            .transitions
            .iter()
            .all(|line| !line.contains(&followup)),
        "the loop claimed the proposed task: {:?}",
        report.transitions
    );
    let stored = fixture.store.load_task(&followup).expect("the task").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "proposed")
    );
    assert_eq!(stored.attempt, 0, "the loop claimed no attempt");
    assert!(
        report.waiting.iter().any(|line| line.contains(&followup)
            && line.contains("todo.proposed")
            && line.contains("waiting for you to confirm it")),
        "the summary does not say it waits: {:?}",
        report.waiting
    );

    let error = radian_arc::attempt::start(&fixture.store, &followup)
        .expect_err("start refuses a proposal");
    assert_eq!(error.exit(), Exit::State);
    assert!(
        error.message().contains("todo.proposed"),
        "{}",
        error.message()
    );
}

/// `confirm <task>` makes the task `todo.ready` and leaves the plan's `status` and `auto`
/// alone.
///
/// Mutation: move the plan back to draft on a task confirm → this fails.
#[test]
fn confirm_makes_a_proposed_task_ready_and_leaves_the_plan_alone() {
    let fixture = Fixture::new("followup-confirm");
    fixture.add_review_stage();
    fixture.set_auto("run");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "followup-confirm-fake",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    fixture.verifying_with_review(&head, fake.program());
    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    let followup = review::merge(&fixture.store, &fixture.task)
        .expect("the merge runs")
        .followup
        .expect("a follow-up task")
        .task;

    let confirmed = plan_ops::confirm(&fixture.store, &followup).expect("the task is confirmed");
    assert_eq!(confirmed.line(), format!("{followup} todo.ready"));
    let stored = fixture.store.load_task(&followup).expect("the task").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert_eq!(stored.attempt, 0);
    let plan = fixture.store.load_plan(&plan_id).expect("the plan").item;
    assert_eq!(plan.status, "confirmed");
    assert_eq!(plan.auto, "run");
}

/// A task in any other state is refused (exit 3), and an id that names both a plan and a task
/// is refused naming both files.
#[test]
fn confirm_refuses_a_task_that_is_not_proposed() {
    let fixture = Fixture::new("followup-confirm-ready");
    let error = plan_ops::confirm(&fixture.store, &fixture.task)
        .expect_err("a `todo.ready` task is not confirmed");
    assert_eq!(error.exit(), Exit::State);
    assert!(
        error.message().contains("todo.ready"),
        "{}",
        error.message()
    );
    assert_eq!(
        fixture.state(),
        ("todo".to_owned(), "ready".to_owned()),
        "a refusal writes nothing"
    );

    // An id that names both a plan and a task: the plan's id gets a task file too.
    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    let plan_path = fixture
        .store
        .root()
        .join("plans")
        .join(format!("{plan_id}.md"));
    fs::copy(
        fixture
            .store
            .root()
            .join("tasks")
            .join(format!("{}.md", fixture.task)),
        fixture
            .store
            .root()
            .join("tasks")
            .join(format!("{plan_id}.md")),
    )
    .expect("a task file that carries the plan's id");
    assert!(plan_path.is_file());
    let error = plan_ops::confirm(&fixture.store, &plan_id).expect_err("an id names one item");
    assert_eq!(error.exit(), Exit::State);
    assert!(
        error.message().contains("names both a plan and a task"),
        "{}",
        error.message()
    );
}

/// A review that suggested nothing adds no task.
#[test]
fn a_merge_with_no_review_items_adds_no_followup() {
    let fixture = Fixture::new("followup-none");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "followup-none-fake",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[],
        )],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, fake.program()).phase,
        "awaiting_human"
    );

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    assert!(merged.followup.is_none(), "{merged:?}");
    assert_eq!(merged.line().lines().count(), 1, "{}", merged.line());
    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    assert_eq!(
        fixture
            .store
            .load_plan(&plan_id)
            .expect("the plan")
            .item
            .tasks,
        vec![fixture.task.clone()]
    );
}

/// `gate.review.propose_followups = false` turns arc's follow-up off; the merge is unchanged.
#[test]
fn propose_followups_false_adds_no_task() {
    let fixture = Fixture::new("followup-off");
    fixture.add_review_stage();
    fixture.tune("propose_followups = true", "propose_followups = false");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "followup-off-fake",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, fake.program()).phase,
        "awaiting_human"
    );

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert!(merged.followup.is_none(), "{merged:?}");
    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    assert_eq!(
        fixture
            .store
            .load_plan(&plan_id)
            .expect("the plan")
            .item
            .tasks,
        vec![fixture.task.clone()]
    );
}

/// The follow-up takes the suggestions and nothing from `blocking`.
///
/// Mutation: include `blocking` items in the follow-up → a task appears here.
#[test]
fn a_blocking_item_is_never_carried_into_the_followup() {
    let fixture = Fixture::new("followup-blocking");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    fixture.set_verifying(&head);
    write_passing_review(
        &fixture,
        &head,
        ReviewReport {
            summary: "the change adds feature.txt".to_owned(),
            blocking: vec![ReviewItem {
                level: Some("major".to_owned()),
                file: Some("feature.txt".to_owned()),
                line: Some(1),
                text: "this must change".to_owned(),
                pre_existing: false,
            }],
            suggestions: Vec::new(),
            verified: Vec::new(),
        },
    );
    assert_eq!(
        review::verify(&fixture.store, &fixture.task)
            .expect("verification runs")
            .phase,
        "awaiting_human"
    );

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert!(
        merged.followup.is_none(),
        "a blocking item became a follow-up: {merged:?}"
    );
    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    assert_eq!(
        fixture
            .store
            .load_plan(&plan_id)
            .expect("the plan")
            .item
            .tasks,
        vec![fixture.task.clone()]
    );
}

/// A reverify, then a merge, still adds at most one follow-up.
#[test]
fn a_merge_after_a_reverify_adds_at_most_one_followup() {
    let fixture = Fixture::new("followup-reverify");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let first = FakeReview::new(
        "followup-reverify-first",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    assert_eq!(
        fixture.verifying_with_review(&head, first.program()).phase,
        "awaiting_human"
    );
    let again = FakeReview::new(
        "followup-reverify-again",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    let reverified = review::reverify_with(&fixture.store, &fixture.task, Some(again.program()))
        .expect("the reverify runs");
    assert!(reverified.verification.is_ok(), "{reverified:?}");

    let merged = review::merge(&fixture.store, &fixture.task).expect("the merge runs");
    let followup = merged.followup.expect("a follow-up task").task;
    let plan_id = fixture.store.load_task(&fixture.task).unwrap().item.plan;
    let plan = fixture.store.load_plan(&plan_id).expect("the plan").item;
    assert_eq!(plan.tasks, vec![fixture.task.clone(), followup]);
    let titled = plan
        .tasks
        .iter()
        .filter(|id| {
            fixture
                .store
                .load_task(id)
                .is_ok_and(|task| task.item.title.contains("'s review"))
        })
        .count();
    assert_eq!(titled, 1, "one follow-up: {:?}", plan.tasks);
}

/// A proposed task the owner does not want ends as `failed.cancelled`.
#[test]
fn cancel_ends_a_proposed_task_as_failed_cancelled() {
    let fixture = Fixture::new("followup-cancel");
    fixture.add_review_stage();
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    let fake = FakeReview::new(
        "followup-cancel-fake",
        &[review_object_message(
            "the change adds feature.txt",
            &[],
            &[review_item("minor", "feature.txt", 3, "a nicer name")],
        )],
        0,
    );
    fixture.verifying_with_review(&head, fake.program());
    let followup = review::merge(&fixture.store, &fixture.task)
        .expect("the merge runs")
        .followup
        .expect("a follow-up task")
        .task;

    radian_arc::attempt::cancel(&fixture.store, &followup).expect("the cancel runs");
    let stored = fixture.store.load_task(&followup).expect("the task").item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("failed", "cancelled")
    );
}

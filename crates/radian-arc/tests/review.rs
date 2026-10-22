//! Verification, merge and reject (ADR 0048 decisions 6, 10 and 11).
//!
//! The end-to-end cases run the real `radian` binary and a real `radian exec` child under
//! the mock model; the merge and refuse cases drive the library against a temp repository
//! with a bare `origin`, so real git decides every push and merge.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use radian_arc::gate::{Stage, Verdict, effective_stages, required_passed, verdict_set_digest};
use radian_arc::review;
use radian_arc::store::attempt_ops::{self, Finish};
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
        let base = self.base.clone();
        self.store
            .change_task(&self.task, |task| {
                task.state = TaskState::Review;
                task.phase = "verifying".to_owned();
                task.attempt = 1;
                task.attempt_id = Some(format!("{}-1", task.id));
                task.token = Some("token-1".to_owned());
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

/// A conflict in the integration stage sends the task to rework, with the path and the
/// verdict recorded, and leaves the remote main untouched.
#[test]
fn a_conflict_makes_rework_pending() {
    let fixture = Fixture::new("merge-conflict");
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

/// A changed verdict file changes the digest: the approval is invalid and `merge` refuses
/// without writing. Mutation: skip the digest check and the merge completes.
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
    assert_state(&fixture, ("review", "awaiting_human"));
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

/// A push that is not a fast-forward is never forced: the remote main keeps the commit that
/// moved it, and the task stays in `review.integrating` after three runs.
///
/// The clone's fetch refspec is limited to `work/*`, so `merge_ready` keeps reading the
/// stale `origin/main` it saw before the second pusher moved main. Without `--force` the
/// push is rejected every round; with `--force` the first push would overwrite main and the
/// task would reach `done.merged`. Mutation: push with `--force`.
#[test]
fn a_rejected_push_never_overwrites_the_remote_main() {
    let fixture = Fixture::new("merge-no-force");
    let head = fixture.publish(&fixture.base.clone(), "feature.txt", "feature\n");
    assert_eq!(fixture.verifying_with(&head).phase, "awaiting_human");
    // The tracking ref for main must stay at `base` while the second pusher moves the real
    // main, so run every fetch with a refspec that does not cover main.
    fixture.repo.git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/work/*:refs/remotes/origin/work/*",
    ]);
    let second = fixture.repo.second_clone("second");
    write(&second.join("other.txt"), "other\n");
    fixture.repo.git_in(&second, &["add", "other.txt"]);
    fixture
        .repo
        .git_in(&second, &["commit", "-m", "a second pusher"]);
    let moved = fixture.repo.git_in(&second, &["rev-parse", "HEAD"]);
    fixture.repo.git_in(&second, &["push", "origin", "main"]);
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(moved.as_str())
    );

    let error = review::merge(&fixture.store, &fixture.task).expect_err("the merge refuses");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("integrating"), "{error:?}");
    assert_state(&fixture, ("review", "integrating"));
    assert_eq!(
        fixture.repo.ls_remote("refs/heads/main").as_deref(),
        Some(moved.as_str()),
        "a rejected push must never overwrite main"
    );
    for label in [
        fixture.attempt_label(),
        format!("{}.i2", fixture.attempt_label()),
        format!("{}.i3", fixture.attempt_label()),
    ] {
        let verdict = fixture.verdict(&label, Stage::Integration);
        assert!(verdict.is_pass(), "{label}: {:?}", verdict.reason);
    }
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
    assert!(claim.resume, "a rework attempt resumes the session");
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

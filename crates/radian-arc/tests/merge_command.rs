//! `[integration] method = "command"`: a user command does the merge and arc checks what it
//! reports (ADR 0048 decision 6).
//!
//! Every case drives the library against a real repository with a bare `origin` and a real
//! shell script standing in for the user's merge command, so the remote decides what merged.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::{Repo, write};
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};

struct Fixture {
    repo: Repo,
    store: Store,
    task: String,
    base: String,
    head: String,
}

impl Fixture {
    /// A repository, arc set up, one confirmed plan with one task, the task's branch published
    /// at `feature.txt`, and the task verified to `review.awaiting_human`.
    fn new(label: &str) -> Self {
        let repo = Repo::new(label);
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
                task.token = Some("token-1".to_owned());
                task.base_sha = Some(base.clone());
                task.head_sha = Some(head.clone());
                Ok(())
            })
            .unwrap();
        let verified = review::verify(&store, &task).expect("verification runs");
        assert_eq!(verified.phase, "awaiting_human");
        Self {
            repo,
            store,
            task,
            base,
            head,
        }
    }

    /// Writes a shell script into the scratch root (outside the repository) and returns its
    /// path, for the `[integration] command` to run.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.repo.root.join(name);
        fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    /// Sets `[integration] method = "command"` with `command` naming a script. The value is a
    /// TOML literal string, so a path with no single quote needs no escaping.
    fn command_method(&self, script: &str) {
        let path = self.store.root().join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# method = \"push\""),
            "the default file has the method line:\n{text}"
        );
        let text = text.replace("# method = \"push\"", "method = \"command\"");
        let text = text.replace(
            "# command = \"./merge.sh\"     # the line `method = \"command\"` runs; refused with `push`",
            &format!("command = 'sh \"{script}\"'"),
        );
        fs::write(&path, text).unwrap();
        assert_eq!(
            self.store.config().unwrap().integration.method.as_str(),
            "command"
        );
    }

    fn key_commands(&self, lines: &[&str]) {
        let path = self.store.root().join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        let list = lines
            .iter()
            .map(|line| format!("'{line}'"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(text.contains("key_commands = []"), "{text}");
        fs::write(
            &path,
            text.replace("key_commands = []", &format!("key_commands = [{list}]")),
        )
        .unwrap();
    }

    fn command_timeout_secs(&self, secs: u32) {
        let path = self.store.root().join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("command_timeout_secs = 3600"), "{text}");
        fs::write(
            &path,
            text.replace(
                "command_timeout_secs = 3600",
                &format!("command_timeout_secs = {secs}"),
            ),
        )
        .unwrap();
    }

    fn state(&self) -> (String, String) {
        let task = self.store.load_task(&self.task).unwrap().item;
        (task.state.as_str().to_owned(), task.phase)
    }

    fn merge(&self) -> review::Merged {
        review::merge(&self.store, &self.task).expect("the merge runs")
    }

    fn remote_main(&self) -> Option<String> {
        Some(self.repo.remote_main_sha())
    }
}

/// A merge command that pushes the candidate and prints its sha merges the task.
#[test]
fn a_command_that_pushes_the_candidate_and_prints_its_sha_reaches_done_merged() {
    let fixture = Fixture::new("merge-command-sha");
    let candidate = fixture.head.clone();
    let env_file = fixture.repo.root.join("env.txt");
    let script = fixture.script(
        "merge.sh",
        &format!(
            "env | grep '^ARC_' | sort > \"{}\"\n\
             git push origin \"$ARC_CANDIDATE_SHA:refs/heads/main\" >/dev/null 2>&1\n\
             echo \"$ARC_CANDIDATE_SHA\"\n",
            env_file.display()
        ),
    );
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert_eq!(merged.candidate_sha.as_deref(), Some(candidate.as_str()));
    assert!(!merged.pending);
    assert_eq!(
        fixture.remote_main().as_deref(),
        Some(candidate.as_str()),
        "the command did the push"
    );
    let stored = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(stored.merged_sha.as_deref(), Some(candidate.as_str()));

    // The environment is the hooks' plus `ARC_CANDIDATE_SHA`, and no `ARC_MERGED_SHA`.
    let env = fs::read_to_string(&env_file).unwrap();
    for line in [
        format!("ARC_TASK={}", fixture.task),
        "ARC_PLAN=PL0001-p".to_owned(),
        format!("ARC_ATTEMPT={}-1", fixture.task),
        format!("ARC_BRANCH=work/{}", fixture.task),
        format!("ARC_CANDIDATE_SHA={candidate}"),
        "ARC_STATE=review.ready_to_merge".to_owned(),
        "ARC_HOOK=integration".to_owned(),
        "ARC_TARGET_BRANCH=main".to_owned(),
    ] {
        assert!(
            env.lines().any(|got| got == line),
            "missing `{line}`:\n{env}"
        );
    }
    assert!(env.lines().any(|line| line.starts_with("ARC_WORKTREE=")));
    assert!(
        !env.lines().any(|line| line.starts_with("ARC_MERGED_SHA=")),
        "ARC_MERGED_SHA belongs to after_merge:\n{env}"
    );
    assert!(
        !fixture
            .store
            .root()
            .join("runs")
            .join(&fixture.task)
            .join("hooks")
            .join("after_merge-01.log")
            .exists()
    );
}

/// A sha whose tree is not the candidate's tree never reaches `done.merged`.
#[test]
fn a_sha_whose_tree_differs_is_recorded_not_merged() {
    let fixture = Fixture::new("merge-command-wrong-tree");
    // The target has not moved, so the candidate is `head`; the base's tree is different. The
    // base is the remote target's tip, so it is "on the remote target" and only the tree fails.
    let base = fixture.base.clone();
    let script = fixture.script("merge.sh", &format!("echo \"{base}\"\n"));
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("review", "rework_pending")
    );
    let reason = merged.reason.clone().expect("a reason");
    assert!(reason.contains("is not the tree of candidate"), "{reason}");
    assert_eq!(fixture.remote_main().as_deref(), Some(base.as_str()));
    assert_eq!(
        fixture
            .store
            .load_task(&fixture.task)
            .unwrap()
            .item
            .merged_sha,
        None
    );
}

/// A sha that is not on the remote target branch is not merged.
#[test]
fn a_sha_that_is_not_on_the_remote_target_is_not_merged() {
    let fixture = Fixture::new("merge-command-absent");
    let script = fixture.script(
        "merge.sh",
        "git commit-tree \"$(git rev-parse \"$ARC_CANDIDATE_SHA^{tree}\")\" \
         -p \"$ARC_CANDIDATE_SHA\" -m local\n",
    );
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("review", "rework_pending")
    );
    let reason = merged.reason.clone().expect("a reason");
    assert!(reason.contains("is not on `origin` `main`"), "{reason}");
    assert_eq!(
        fixture.remote_main().as_deref(),
        Some(fixture.base.as_str()),
        "nothing was pushed"
    );
}

/// `pending` twice and a sha on the third run: the task merges on the third pass, and the
/// candidate is built once (the integration round counter does not move while pending).
#[test]
fn pending_runs_reuse_the_candidate_and_merge_on_a_later_pass() {
    let fixture = Fixture::new("merge-command-pending");
    let counter = fixture.repo.root.join("count");
    let keyed = fixture.repo.root.join("keyed.txt");
    let script = fixture.script(
        "merge.sh",
        &format!(
            "n=$(cat \"{count}\" 2>/dev/null || echo 0)\n\
             n=$((n + 1))\n\
             echo \"$n\" > \"{count}\"\n\
             if [ \"$n\" -lt 3 ]; then\n\
               echo pending\n\
             else\n\
               git push origin \"$ARC_CANDIDATE_SHA:refs/heads/main\" >/dev/null 2>&1\n\
               echo \"$ARC_CANDIDATE_SHA\"\n\
             fi\n",
            count = counter.display()
        ),
    );
    fixture.command_method(&script);
    fixture.key_commands(&[&format!("echo x >> \"{}\"", keyed.display())]);

    let first = fixture.merge();
    assert!(first.pending, "{first:?}");
    assert_eq!(
        (first.state, first.phase.as_str()),
        ("review", "integrating")
    );
    assert!(first.reason.as_deref().unwrap().contains("pending"));
    assert_eq!(first.rounds, 1);

    // A scheduling pass runs the command again with the same candidate: no new integration
    // stage, and no line for the pass.
    let second = review::merge_step(&fixture.store, &fixture.task, false).unwrap();
    assert!(second.line.is_none(), "{second:?}");
    assert_eq!(
        fixture.state(),
        ("review".to_owned(), "integrating".to_owned())
    );

    let third = review::merge_step(&fixture.store, &fixture.task, false).unwrap();
    let line = third.line.clone().expect("the third pass merges");
    assert!(line.contains("done.merged"), "{line}");
    assert_eq!(fixture.state(), ("done".to_owned(), "merged".to_owned()));

    assert_eq!(fs::read_to_string(&counter).unwrap().trim(), "3");
    let built = fs::read_to_string(&keyed).unwrap();
    assert_eq!(
        built.lines().count(),
        1,
        "the candidate was rebuilt while pending"
    );
}

/// A target branch that moved while pending is rebuilt: the old verdict is not reused and the
/// new candidate is built on the new target.
#[test]
fn a_moved_target_rebuilds_the_candidate_while_pending() {
    let fixture = Fixture::new("merge-command-target-moved");
    let count = fixture.repo.root.join("count");
    let keyed = fixture.repo.root.join("keyed.txt");
    let script = fixture.script(
        "merge.sh",
        &format!(
            "n=$(cat \"{count}\" 2>/dev/null || echo 0)\n\
             n=$((n + 1))\n\
             echo \"$n\" > \"{count}\"\n\
             if [ \"$n\" -lt 2 ]; then\n\
               echo pending\n\
             else\n\
               git push origin \"$ARC_CANDIDATE_SHA:refs/heads/main\" >/dev/null 2>&1\n\
               echo \"$ARC_CANDIDATE_SHA\"\n\
             fi\n",
            count = count.display()
        ),
    );
    fixture.command_method(&script);
    fixture.key_commands(&[&format!("echo x >> \"{}\"", keyed.display())]);

    let first = fixture.merge();
    assert!(first.pending, "{first:?}");
    assert_eq!(fs::read_to_string(&keyed).unwrap().lines().count(), 1);

    fixture.repo.git(&["checkout", "main"]);
    write(&fixture.repo.clone.join("other.txt"), "other\n");
    fixture.repo.git(&["add", "other.txt"]);
    fixture.repo.git(&["commit", "-m", "main: other"]);
    let moved = fixture.repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fixture.repo.git(&["push", "origin", "main"]);

    let second = fixture.merge();
    assert!(!second.pending, "{second:?}");
    assert_eq!((second.state, second.phase.as_str()), ("done", "merged"));
    assert_eq!(
        fs::read_to_string(&keyed).unwrap().lines().count(),
        2,
        "the moved target is not reused; a new candidate is built"
    );
    let candidate = second.candidate_sha.clone().expect("a candidate");
    let parents = fixture
        .repo
        .git(&["rev-list", "--parents", "-n", "1", &candidate]);
    assert!(
        parents.split_whitespace().any(|sha| sha == moved),
        "the new candidate is not built on the moved target: {parents}"
    );
    assert_eq!(fixture.remote_main().as_deref(), Some(candidate.as_str()));
}

/// `closed` sends the task to rework with that reason.
#[test]
fn closed_sends_the_task_to_rework_pending() {
    let fixture = Fixture::new("merge-command-closed");
    let script = fixture.script("merge.sh", "echo closed\n");
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("review", "rework_pending")
    );
    assert_eq!(
        merged.reason.as_deref(),
        Some("closed by the merge command")
    );
}

/// A non-zero exit wins over a sha on the last line.
#[test]
fn a_non_zero_exit_with_a_sha_last_is_a_failure() {
    let fixture = Fixture::new("merge-command-exit");
    let script = fixture.script(
        "merge.sh",
        "git push origin \"$ARC_CANDIDATE_SHA:refs/heads/main\" >/dev/null 2>&1\n\
         echo \"$ARC_CANDIDATE_SHA\"\n\
         exit 1\n",
    );
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("review", "rework_pending")
    );
    let reason = merged.reason.clone().expect("a reason");
    assert!(reason.contains("exited 1"), "{reason}");
    assert_eq!(
        fixture
            .store
            .load_task(&fixture.task)
            .unwrap()
            .item
            .merged_sha,
        None
    );
}

/// Trailing blank lines and a CRLF ending: the last non-empty line is the answer and its `\r`
/// is trimmed.
#[test]
fn the_last_non_empty_line_is_read_and_a_crlf_is_trimmed() {
    let fixture = Fixture::new("merge-command-crlf");
    let candidate = fixture.head.clone();
    let script = fixture.script(
        "merge.sh",
        "git push origin \"$ARC_CANDIDATE_SHA:refs/heads/main\" >/dev/null 2>&1\n\
         printf '%s\\r\\n\\r\\n   \\r\\n' \"$ARC_CANDIDATE_SHA\"\n",
    );
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!((merged.state, merged.phase.as_str()), ("done", "merged"));
    assert_eq!(merged.candidate_sha.as_deref(), Some(candidate.as_str()));
}

/// A command that hangs is killed at `[gate] command_timeout_secs` and fails the merge.
#[test]
fn a_hanging_command_is_killed_at_the_timeout() {
    let fixture = Fixture::new("merge-command-hang");
    let marker = fixture.repo.root.join("after-sleep.txt");
    let script = fixture.script(
        "merge.sh",
        &format!("sleep 30\necho late > \"{}\"\n", marker.display()),
    );
    fixture.command_method(&script);
    fixture.command_timeout_secs(1);

    let started = Instant::now();
    let merged = fixture.merge();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the timeout did not stop the command"
    );
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("review", "rework_pending")
    );
    let reason = merged.reason.clone().expect("a reason");
    assert!(reason.contains("timed out after 1 s"), "{reason}");
    assert!(!marker.exists(), "the killed command kept running");
}

/// `method = "command"` without `command` is a config load error that names the key.
#[test]
fn method_command_without_a_command_is_refused_by_name() {
    let error = radian_arc::store::config::parse(
        "schema_version = 1\n[integration]\nmethod = \"command\"\n",
    )
    .expect_err("a command method needs a command");
    assert!(error.contains("[integration] command"), "{error}");
    assert!(error.contains("command"), "{error}");
}

/// The CLI says the merge is pending and exits `3` (the state does not allow the operation
/// to finish now).
#[test]
fn the_cli_exits_three_and_says_pending() {
    let fixture = Fixture::new("merge-command-cli-pending");
    let script = fixture.script("merge.sh", "echo pending\n");
    fixture.command_method(&script);

    let run = fixture.repo.radian(&["arc", "merge", &fixture.task]);
    assert_eq!(
        run.code, 3,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert!(run.stdout.contains("pending"), "{}", run.stdout);
    assert!(run.stdout.contains("review.integrating"), "{}", run.stdout);
    assert_eq!(
        fixture.state(),
        ("review".to_owned(), "integrating".to_owned())
    );
}

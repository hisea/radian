//! `[integration] method = "command"`: a user command does the merge and arc checks what it
//! reports (ADR 0048 decision 6).
//!
//! Every case drives the library against a real repository with a bare `origin` and a real
//! shell script standing in for the user's merge command, so the remote decides what merged.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{RealGitConfig, Repo, write};
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};

struct Fixture {
    repo: Repo,
    store: Store,
    task: String,
    base: String,
    head: String,
    /// The real repository's git config, recorded before any of this fixture's git runs and
    /// compared after the test: nothing here may write the checkout the tests run from.
    real: RealGitConfig,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A test that is already failing has its own reason; asserting here would replace it
        // (and panic while unwinding aborts the process).
        if !std::thread::panicking() {
            self.real.assert_unchanged();
        }
    }
}

impl Fixture {
    /// A repository, arc set up, one confirmed plan with one task, the task's branch published
    /// at `feature.txt`, and the task verified to `review.awaiting_human`.
    fn new(label: &str) -> Self {
        Self::new_titled(label, "A task")
    }

    /// [`Fixture::new`] with the task's own title, for the quoting case of the `pr` preset.
    fn new_titled(label: &str, title: &str) -> Self {
        let real = RealGitConfig::record();
        let repo = Repo::new(label);
        Store::init(&repo.clone, false).expect("arc init");
        let store = Store::open(&repo.clone).expect("open");
        let plan = store.add_plan("p", "A plan", "the request\n").unwrap();
        let task = store
            .add_task(
                &plan,
                "work",
                title,
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
        let fixture = Self {
            repo,
            store,
            task,
            base,
            head,
            real,
        };
        // The fixture's own setup (a bare remote, a clone, a gate worktree) is git in
        // temporary repositories; it must not have touched the real one either.
        fixture.real.assert_unchanged();
        fixture
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
            "# command = \"./merge.sh\"     # the line `method = \"command\"` runs; refused with `push` and `pr`",
            &format!("command = 'sh \"{script}\"'"),
        );
        fs::write(&path, text).unwrap();
        assert_eq!(
            self.store.config().unwrap().integration.method.as_str(),
            "command"
        );
    }

    fn method(&self, method: &str) {
        let path = self.store.root().join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# method = \"push\""),
            "the default file has the method line:\n{text}"
        );
        let text = text.replace("# method = \"push\"", &format!("method = \"{method}\""));
        fs::write(&path, text).unwrap();
        assert_eq!(
            self.store.config().unwrap().integration.method.as_str(),
            method
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
             git config --local radian.guard-marker yes\n\
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
    // The command's git calls ran in the temporary repository: its local config carries the
    // write, the real repository's does not (the fixture's guard checks that after the test).
    let local = fixture.repo.clone.join(".git").join("config");
    assert!(
        fs::read_to_string(&local).unwrap().contains("guard-marker"),
        "the command did not run in the fixture repository: {}",
        local.display()
    );
    let real = fixture
        .real
        .path()
        .expect("the test crate is checked out in a repository")
        .to_path_buf();
    assert!(
        !real.starts_with(&fixture.repo.root),
        "the guard watches the fixture, not the real repository: {}",
        real.display()
    );
    fixture.real.assert_unchanged();

    // The environment is the hooks' plus `ARC_CANDIDATE_SHA`, and no `ARC_MERGED_SHA`.
    let env = fs::read_to_string(&env_file).unwrap();
    for line in [
        format!("ARC_TASK={}", fixture.task),
        "ARC_PLAN=PL0001-p".to_owned(),
        format!("ARC_ATTEMPT={}-1", fixture.task),
        format!("ARC_BRANCH=work/{}", fixture.task),
        format!("ARC_CANDIDATE_SHA={candidate}"),
        "ARC_REMOTE=origin".to_owned(),
        "ARC_TITLE=A task".to_owned(),
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
    assert!(reason.contains("the tree of candidate"), "{reason}");
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

// ---- method = "pr": the built-in `command` preset ----------------------------------

fn value_of<'a>(arguments: &'a [String], flag: &str) -> &'a str {
    let index = arguments
        .iter()
        .position(|argument| argument == flag)
        .unwrap_or_else(|| panic!("`{flag}` is missing from {arguments:?}"));
    &arguments[index + 1]
}

/// A fake `gh` on `PATH` for the `pr` preset: it records every argument block and answers
/// from a state file. `create` records its arguments and leaves the PR `OPEN`.
struct FakeGh {
    dir: PathBuf,
    calls: PathBuf,
    creates: PathBuf,
    state: PathBuf,
}

impl FakeGh {
    fn new(repo: &Repo) -> Self {
        let dir = repo.root.join("gh-bin");
        fs::create_dir_all(&dir).unwrap();
        let calls = repo.root.join("gh-calls.log");
        let creates = repo.root.join("gh-creates.log");
        let state = repo.root.join("gh-state");
        let bin = dir.join("gh");
        let body = format!(
            "#!/bin/sh\n\
             state='{state}'\n\
             calls='{calls}'\n\
             creates='{creates}'\n\
             {{ printf 'call\\n'; for arg in \"$@\"; do printf 'arg:%s\\n' \"$arg\"; done; }} >> \"$calls\"\n\
             if [ \"$1\" = pr ] && [ \"$2\" = view ]; then\n\
               [ -s \"$state\" ] || {{ echo \"no pull requests found for branch $3\" >&2; exit 1; }}\n\
               cat \"$state\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = pr ] && [ \"$2\" = list ]; then\n\
               [ -s \"$state\" ] && cat \"$state\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = pr ] && [ \"$2\" = create ]; then\n\
               {{ printf 'call\\n'; for arg in \"$@\"; do printf 'arg:%s\\n' \"$arg\"; done; }} >> \"$creates\"\n\
               echo OPEN > \"$state\"\n\
               exit 0\n\
             fi\n\
             echo \"fake gh: unexpected arguments: $*\" >&2\n\
             exit 1\n",
            state = state.display(),
            calls = calls.display(),
            creates = creates.display(),
        );
        fs::write(&bin, body).unwrap();
        make_executable(&bin);
        Self {
            dir,
            calls,
            creates,
            state,
        }
    }

    /// The `PATH` the merge command's child sees: this fake `gh` first, then the test's own.
    fn path(&self) -> String {
        format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn set_state(&self, state: &str) {
        fs::write(&self.state, state).unwrap();
    }

    fn calls(&self) -> Vec<Vec<String>> {
        parse_blocks(&fs::read_to_string(&self.calls).unwrap_or_default())
    }

    fn creates(&self) -> Vec<Vec<String>> {
        parse_blocks(&fs::read_to_string(&self.creates).unwrap_or_default())
    }
}

/// One `call` block of a fake program's log: the marker line starts it and `arg:` lines hold
/// its arguments.
fn parse_blocks(text: &str) -> Vec<Vec<String>> {
    let mut blocks: Vec<Vec<String>> = Vec::new();
    for line in text.lines() {
        if line == "call" {
            blocks.push(Vec::new());
        } else if let Some(argument) = line.strip_prefix("arg:")
            && let Some(block) = blocks.last_mut()
        {
            block.push(argument.to_owned());
        }
    }
    blocks
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}

/// A `PATH` with a `git` and a `sh`, and no `gh`: the `pr` preset's own lookup fails.
fn path_without_gh(repo: &Repo) -> String {
    let dir = repo.root.join("gh-missing-bin");
    fs::create_dir_all(&dir).unwrap();
    let found = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("the shell runs");
    let git = String::from_utf8_lossy(&found.stdout);
    let git = Path::new(git.trim());
    #[cfg(unix)]
    std::os::unix::fs::symlink(git, dir.join("git")).expect("the git symlink");
    format!("{}:/bin", dir.display())
}

/// A `gh` on `PATH` that always fails with `message`, like an unauthenticated one.
fn failing_gh(repo: &Repo, message: &str) -> String {
    let dir = repo.root.join("gh-failing-bin");
    fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("gh");
    fs::write(&bin, format!("#!/bin/sh\necho '{message}' >&2\nexit 1\n")).unwrap();
    make_executable(&bin);
    format!("{}:{}", dir.display(), path_without_gh(repo))
}

impl Fixture {
    fn merge_with_path(&self, path: &str) -> common::Run {
        self.repo
            .radian_with_env(&["arc", "merge", &self.task], &[("PATH", path.to_owned())])
    }

    fn merged_state(&self) -> (String, String) {
        let task = self.store.load_task(&self.task).unwrap().item;
        (task.state.as_str().to_owned(), task.phase)
    }
}

/// Edge 1: the first pass opens one pull request with the right base and head, and the task
/// stays pending. The branch is pushed so the pull request shows the verified head.
#[test]
fn the_pr_preset_opens_one_pull_request_and_stays_pending() {
    let fixture = Fixture::new("pr-create");
    let gh = FakeGh::new(&fixture.repo);
    fixture.method("pr");
    let branch = format!("work/{}", fixture.task);

    let run = fixture.merge_with_path(&gh.path());
    assert_eq!(
        run.code, 3,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert!(run.stdout.contains("pending"), "{}", run.stdout);
    assert_eq!(
        fixture.merged_state(),
        ("review".to_owned(), "integrating".to_owned())
    );

    let creates = gh.creates();
    assert_eq!(creates.len(), 1, "one `gh pr create`: {creates:?}");
    let arguments = &creates[0];
    assert_eq!(value_of(arguments, "--base"), "main");
    assert_eq!(value_of(arguments, "--head"), branch);
    assert_eq!(
        value_of(arguments, "--title"),
        format!("A task ({})", fixture.task),
        "{arguments:?}"
    );
    assert_eq!(
        fixture.repo.remote_branch(&fixture.task).as_deref(),
        Some(fixture.head.as_str()),
        "the preset pushes the verified head"
    );
}

/// Edge 2: a second pass with the pull request still open opens no second one and stays
/// pending.
#[test]
fn a_still_open_pull_request_stays_pending_without_a_second_create() {
    let fixture = Fixture::new("pr-open");
    let gh = FakeGh::new(&fixture.repo);
    fixture.method("pr");

    let first = fixture.merge_with_path(&gh.path());
    assert_eq!(first.code, 3, "{}", first.stderr);
    assert_eq!(gh.creates().len(), 1);

    // `create` left the state `OPEN`; the second pass reads it.
    let second = fixture.merge_with_path(&gh.path());
    assert_eq!(second.code, 3, "{}", second.stderr);
    assert!(second.stdout.contains("pending"), "{}", second.stdout);
    assert_eq!(
        fixture.merged_state(),
        ("review".to_owned(), "integrating".to_owned())
    );
    assert_eq!(
        gh.creates().len(),
        1,
        "no second `gh pr create`: {:?}",
        gh.calls()
    );
}

/// Edge 3: a merged pull request reports the target branch's sha and the task merges.
#[test]
fn a_merged_pull_request_reaches_done_merged() {
    let fixture = Fixture::new("pr-merged");
    let gh = FakeGh::new(&fixture.repo);
    fixture.method("pr");
    let branch = format!("work/{}", fixture.task);

    let first = fixture.merge_with_path(&gh.path());
    assert_eq!(first.code, 3, "{}", first.stderr);

    // The pull request merges: `work/<task>` goes into main, and main is pushed.
    fixture.repo.git(&["checkout", "main"]);
    fixture
        .repo
        .git(&["merge", "--no-ff", "--no-edit", "-m", "PR merge", &branch]);
    let merged_sha = fixture.repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fixture.repo.git(&["push", "origin", "main"]);
    gh.set_state("MERGED");

    let second = fixture.merge_with_path(&gh.path());
    assert_eq!(
        second.code, 0,
        "stdout: {}\nstderr: {}",
        second.stdout, second.stderr
    );
    assert_eq!(
        fixture.merged_state(),
        ("done".to_owned(), "merged".to_owned())
    );
    let task = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(task.merged_sha.as_deref(), Some(merged_sha.as_str()));
    assert_eq!(fixture.remote_main().as_deref(), Some(merged_sha.as_str()));
}

/// Edge 4 (through the `command` method, which the `pr` preset uses unchanged): the reported
/// sha is the target branch's current tip, and one commit in its history holds the candidate's
/// tree. The tip's own tree is larger, so the check is "the candidate tree is in the history
/// of the reported sha", not "the two trees are equal".
#[test]
fn a_target_that_gained_another_merge_after_the_candidate_still_merges() {
    let fixture = Fixture::new("merge-command-later-merge");
    let branch = format!("work/{}", fixture.task);
    // A merge of the task branch into the target, then someone else's commit. The merge commit
    // has the tree the integration candidate will have; the tip has more.
    fixture.repo.git(&["checkout", "-B", "prmerge", "main"]);
    fixture
        .repo
        .git(&["merge", "--no-ff", "--no-edit", "-m", "PR merge", &branch]);
    write(&fixture.repo.clone.join("later.txt"), "later\n");
    fixture.repo.git(&["add", "later.txt"]);
    fixture.repo.git(&["commit", "-m", "someone else"]);
    let tip = fixture.repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fixture.repo.git(&["checkout", &branch]);

    let script = fixture.script(
        "merge.sh",
        &format!(
            "git push origin prmerge:refs/heads/main >/dev/null 2>&1\n\
             echo \"{tip}\"\n"
        ),
    );
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("done", "merged"),
        "{merged:?}"
    );
    assert_eq!(merged.candidate_sha.as_deref(), Some(tip.as_str()));
    assert_eq!(
        fixture
            .store
            .load_task(&fixture.task)
            .unwrap()
            .item
            .merged_sha
            .as_deref(),
        Some(tip.as_str()),
        "the task records the reported tip"
    );
    assert_eq!(fixture.remote_main().as_deref(), Some(tip.as_str()));
}

/// Edge 4: a pull request merged before someone else's merge reports the target branch's
/// current tip, not the pull request's merge commit.
#[test]
fn a_pull_request_merged_before_another_merge_reports_the_current_tip() {
    let fixture = Fixture::new("pr-later-merge");
    let gh = FakeGh::new(&fixture.repo);
    fixture.method("pr");
    let branch = format!("work/{}", fixture.task);

    let first = fixture.merge_with_path(&gh.path());
    assert_eq!(first.code, 3, "{}", first.stderr);

    fixture.repo.git(&["checkout", "main"]);
    fixture
        .repo
        .git(&["merge", "--no-ff", "--no-edit", "-m", "PR merge", &branch]);
    let pr_merge = fixture.repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    write(&fixture.repo.clone.join("later.txt"), "later\n");
    fixture.repo.git(&["add", "later.txt"]);
    fixture.repo.git(&["commit", "-m", "someone else"]);
    let tip = fixture.repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fixture.repo.git(&["push", "origin", "main"]);
    gh.set_state("MERGED");

    let second = fixture.merge_with_path(&gh.path());
    assert_eq!(
        second.code, 0,
        "stdout: {}\nstderr: {}",
        second.stdout, second.stderr
    );
    assert_eq!(
        fixture.merged_state(),
        ("done".to_owned(), "merged".to_owned())
    );
    let task = fixture.store.load_task(&fixture.task).unwrap().item;
    assert_eq!(
        task.merged_sha.as_deref(),
        Some(tip.as_str()),
        "the current tip, not the pull request's merge commit"
    );
    assert_ne!(pr_merge, tip);
    assert_eq!(fixture.remote_main().as_deref(), Some(tip.as_str()));
}

/// Edge 5: a pull request closed without a merge sends the task to rework.
#[test]
fn a_closed_pull_request_sends_the_task_to_rework_pending() {
    let fixture = Fixture::new("pr-closed");
    let gh = FakeGh::new(&fixture.repo);
    gh.set_state("CLOSED");
    fixture.method("pr");

    let run = fixture.merge_with_path(&gh.path());
    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        fixture.merged_state(),
        ("review".to_owned(), "rework_pending".to_owned())
    );
    assert!(
        run.stdout.contains("closed by the merge command"),
        "{}",
        run.stdout
    );
    assert!(gh.creates().is_empty(), "{:?}", gh.creates());
}

/// Edge 6a: no `gh` on `PATH` fails the integration at once with the preset's message.
#[test]
fn a_missing_gh_fails_the_integration_with_its_message() {
    let fixture = Fixture::new("pr-no-gh");
    let path = path_without_gh(&fixture.repo);
    fixture.method("pr");

    let run = fixture.merge_with_path(&path);
    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        fixture.merged_state(),
        ("review".to_owned(), "rework_pending".to_owned())
    );
    assert!(
        run.stdout.contains("needs gh on PATH"),
        "stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
}

/// Edge 6b: `gh` that is not authenticated fails with gh's own message in the reason.
#[test]
fn an_unauthenticated_gh_fails_the_integration_with_its_message() {
    let fixture = Fixture::new("pr-no-auth");
    let path = failing_gh(
        &fixture.repo,
        "gh: authentication required; run gh auth login",
    );
    fixture.method("pr");

    let run = fixture.merge_with_path(&path);
    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        fixture.merged_state(),
        ("review".to_owned(), "rework_pending".to_owned())
    );
    assert!(
        run.stdout.contains("authentication required"),
        "stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
}

/// Edge 7: a title with spaces and both quote kinds reaches `gh pr create` as one argument.
#[test]
fn a_title_with_spaces_and_quotes_is_one_create_argument() {
    let fixture = Fixture::new_titled("pr-quotes", "Fix \"it's\" broken");
    let gh = FakeGh::new(&fixture.repo);
    fixture.method("pr");

    let run = fixture.merge_with_path(&gh.path());
    assert_eq!(run.code, 3, "{}", run.stderr);
    let creates = gh.creates();
    assert_eq!(creates.len(), 1, "{creates:?}");
    let title = format!("Fix \"it's\" broken ({})", fixture.task);
    assert!(
        creates[0].iter().any(|argument| argument == &title),
        "the title is one argument: {:?}",
        creates[0]
    );
}

/// The guard the command-method tests run against the real repository: a `git config --local`
/// against the repository it guards is seen, and the recorded content is put back, so the
/// failure leaves the checkout as it was.
#[test]
fn the_config_guard_fails_and_restores_a_changed_config() {
    let root = common::scratch("merge-command-guard");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let run = |args: &[&str]| {
        let mut command = Command::new("git");
        command.current_dir(&repo).args(args);
        let output = common::isolate_git(&mut command)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-q"]);
    let config = repo.join(".git").join("config");
    let recorded = fs::read_to_string(&config).unwrap();

    let guard = RealGitConfig::of(&repo);
    let expected = fs::canonicalize(&config).unwrap();
    assert_eq!(guard.path(), Some(expected.as_path()));
    assert!(!guard.changed(), "a fresh record is not a change");

    run(&["config", "--local", "x.y", "z"]);
    assert!(guard.changed(), "the guard did not see the change");
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        recorded,
        "the guard did not restore the config it recorded"
    );
}

/// A worker's environment can carry `GIT_DIR` and `GIT_WORK_TREE`; the merge command must
/// still run against the repository root it runs in, never the one those name.
#[test]
fn a_worker_environment_with_git_dir_does_not_redirect_the_merge_command() {
    let decoy = Repo::new("merge-command-gitdir-decoy");
    let decoy_main = decoy.main_sha();
    let decoy_config = decoy.clone.join(".git").join("config");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_merge_under_an_inherited_git_dir",
            "--nocapture",
        ])
        .env("ARC_MERGE_GIT_DIR_DECOY", &decoy.clone)
        .env("GIT_DIR", decoy.clone.join(".git"))
        .env("GIT_WORK_TREE", &decoy.clone)
        .output()
        .expect("the child test runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("MERGED"),
        "the child did not merge:\n{stdout}"
    );
    assert!(
        !fs::read_to_string(&decoy_config)
            .unwrap()
            .contains("guard-marker"),
        "the merge command ran in the repository `GIT_DIR` named"
    );
    assert_eq!(
        decoy.remote_main_sha(),
        decoy_main,
        "the merge command pushed to the repository `GIT_DIR` named"
    );
}

/// Child for [`a_worker_environment_with_git_dir_does_not_redirect_the_merge_command`]: a
/// whole merge in a temporary repository while this process's environment carries `GIT_DIR`
/// and `GIT_WORK_TREE` for another one (the parent sets them for this child only).
#[test]
fn child_merge_under_an_inherited_git_dir() {
    if std::env::var("ARC_MERGE_GIT_DIR_DECOY").is_err() {
        return;
    }
    let fixture = Fixture::new("merge-command-gitdir");
    let candidate = fixture.head.clone();
    let script = fixture.script(
        "merge.sh",
        "git config --local radian.guard-marker yes\n\
         git push origin \"$ARC_CANDIDATE_SHA:refs/heads/main\" >/dev/null 2>&1\n\
         echo \"$ARC_CANDIDATE_SHA\"\n",
    );
    fixture.command_method(&script);

    let merged = fixture.merge();
    assert_eq!(
        (merged.state, merged.phase.as_str()),
        ("done", "merged"),
        "{merged:?}"
    );
    assert_eq!(
        fixture.remote_main().as_deref(),
        Some(candidate.as_str()),
        "the command did not push to the fixture's own remote"
    );
    let local = fixture.repo.clone.join(".git").join("config");
    assert!(
        fs::read_to_string(&local).unwrap().contains("guard-marker"),
        "the command's git config write did not land in the fixture repository"
    );
    println!("MERGED {candidate}");
}

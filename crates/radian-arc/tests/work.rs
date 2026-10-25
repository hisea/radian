//! `radian arc work` end to end (ADR 0048 decisions 7 and 8): a real `radian` binary runs
//! a real `radian exec` child under the mock model, against a temp bare origin and a temp
//! main clone.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::Reply;
use common::*;
use radian_arc::store::Store;
use serde_json::{Value, json};

fn run(
    repo: &Repo,
    task: &str,
    attempt: u32,
    token: &str,
    flags: &[&str],
    extra: &[String],
    brief: &str,
) -> Run {
    let args = work_args(task, attempt, token, flags, extra);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.work(&refs, brief)
}

fn last_result(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("the child's jsonl stream");
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value.get("type").and_then(Value::as_str) == Some("Result"))
        .unwrap_or_else(|| panic!("no Result line in {}", path.display()))
}

fn wait_for_pid(identity: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(identity)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
            && let Some(pid) = value.get("pid").and_then(Value::as_u64)
        {
            return pid as u32;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            identity.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Arc set up for the repository, so the wrapper reads a `[gate] profile` and its
/// `before_attempt` steps (`init` writes the default profile, which lists `merge-main`).
fn init(repo: &Repo) {
    Store::init(&repo.clone, false).expect("arc init");
}

/// Replaces one piece of `config.toml`.
fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = std::fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    std::fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// Writes the repository's standing worker instructions (ADR 0048 decision 7).
fn write_worker_brief(repo: &Repo, text: &str) {
    std::fs::create_dir_all(repo.arc()).expect("the arc directory");
    std::fs::write(repo.arc().join("worker-brief.md"), text).expect("worker-brief.md");
}

/// A test-only fake worker (`radian arc work --worker-command`) that records the brief it
/// received on stdin in the returned path and exits 0 without a `Result`. The wrapper then
/// records the attempt as `terminated`, but the stdin it composed is exactly what the worker
/// read, which is what these tests compare.
fn recording_worker(repo: &Repo, name: &str) -> (std::path::PathBuf, String) {
    let path = repo.root.join(name);
    let command = format!("cat > '{}'", path.display());
    (path, command)
}

/// The merge commits on `work/<task>`, newest first.
fn merge_commits(repo: &Repo, task: &str) -> Vec<String> {
    let out = repo.git(&[
        "log",
        "--merges",
        "--format=%H",
        &format!("refs/heads/work/{task}"),
    ]);
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Commits `file` on main in a second clone and pushes it, returning the new main tip. The
/// main clone's `origin/main` stays stale, the way a remote that moved after this clone last
/// fetched leaves it.
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

/// The brief the worker got on stdin: the last user message of a request. A resumed session's
/// request repeats the earlier turns first.
fn brief_of(request: &agent_provider_openai_compatible::testing::RecordedRequest) -> String {
    request.json()["messages"]
        .as_array()
        .expect("the request has messages")
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .next_back()
        .unwrap_or_default()
        .to_owned()
}

/// The model commits a change; the wrapper pushes `work/<task>`, `ls-remote` equals
/// `head_sha`, `AttemptFinished.pushed` is true, and every record file exists.
#[test]
fn the_happy_path_commits_pushes_and_records_the_attempt() {
    let repo = Repo::new("happy");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0001-happy",
        1,
        "tok-happy",
        &[],
        &model_args(mock.base_url()),
        "make a change",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);

    let finished = run.finished();
    assert_eq!(finished["attempt_id"], json!("TK0001-happy-1"));
    assert_eq!(finished["token"], json!("tok-happy"));
    assert_eq!(finished["exit_code"], json!(0));
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(finished["pushed"], json!(true));
    assert_eq!(finished["failure"], json!(null));
    let base = repo.main_sha();
    assert_eq!(finished["base_sha"], json!(base.as_str()));
    let head = finished["head_sha"].as_str().expect("head_sha").to_owned();
    assert_ne!(head, base, "the model's commit moves HEAD");
    assert_eq!(finished["remote_sha"], json!(head.as_str()));
    assert_eq!(
        repo.remote_branch("TK0001-happy").as_deref(),
        Some(head.as_str()),
        "origin's work/<task> is head_sha"
    );

    for name in [
        "result.jsonl",
        "stderr.log",
        "execution.json",
        "attempt_finished.json",
    ] {
        assert!(
            repo.run_dir("TK0001-happy", 1).join(name).exists(),
            "runs/…/{name} exists"
        );
    }
    for name in ["identity.json", "status.json"] {
        assert!(
            repo.attempt_dir("TK0001-happy", 1).join(name).exists(),
            "attempts/…/{name} exists"
        );
    }
    let identity: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.attempt_dir("TK0001-happy", 1).join("identity.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(identity["token"], json!("tok-happy"));
    assert_eq!(identity["session_id"], json!("TK0001-happy"));
    assert_eq!(
        identity["worker"],
        json!({"kind": "radian"}),
        "the wrapper's identity record names the worker from the start"
    );
    assert_eq!(identity["branch"], json!("work/TK0001-happy"));
    assert!(identity["pid"].as_u64().unwrap() > 0);
    assert!(
        identity["process_started_at"].as_str().is_some(),
        "the worker's process start time is recorded"
    );
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir("TK0001-happy", 1).join("execution.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(execution["base_sha"], json!(base.as_str()));
    assert_eq!(execution["head_sha"], json!(head.as_str()));
    assert_eq!(execution["attempt_id"], json!("TK0001-happy-1"));

    let result = last_result(&repo.run_dir("TK0001-happy", 1).join("result.jsonl"));
    assert_eq!(result["outcome"], json!("completed"));
    assert_eq!(result["session_id"], json!("TK0001-happy"));
    let written = std::fs::read_to_string(
        repo.run_dir("TK0001-happy", 1)
            .join("attempt_finished.json"),
    )
    .unwrap();
    assert_eq!(
        written.trim(),
        run.stdout.trim(),
        "attempt_finished.json is the stdout line"
    );
}

/// While task B's worktree exists, task A's sandboxed worker moves B's ref back to the
/// base commit. B's next attempt sees that `work/B` no longer points at the tip its last
/// attempt ended on: the wrapper refuses it before the child runs and pushes nothing. B's
/// remote ref stays where B left it.
#[test]
fn another_tasks_moved_ref_fails_the_owners_next_attempt_at_start() {
    let repo = Repo::new("cross-ref");
    let base = repo.main_sha();
    let b_task = "TK0002-b";
    let mock_b1 = Mock::start(vec![
        shell_reply("c1", "echo b > b.txt && git add b.txt && git commit -m b"),
        text_reply("done"),
    ]);
    let b1 = run(
        &repo,
        b_task,
        1,
        "tok-b1",
        &[],
        &model_args(mock_b1.base_url()),
        "b",
    );
    assert_eq!(b1.code, 0, "{}", b1.stderr);
    let b_head = b1.finished()["head_sha"].as_str().unwrap().to_owned();
    assert_eq!(repo.remote_branch(b_task).as_deref(), Some(b_head.as_str()));

    // A's worker runs inside its own `workspace` sandbox; the ref it moves lives in the
    // shared common git directory. ADR 0038 lets the sandbox write refs.
    let command = format!(
        "git update-ref refs/heads/work/{b_task} {base} && echo a > a.txt && git add a.txt && \
         git commit -m a"
    );
    let mock_a = Mock::start(vec![shell_reply("c1", &command), text_reply("done")]);
    let a1 = run(
        &repo,
        "TK0001-a",
        1,
        "tok-a1",
        &[],
        &model_args(mock_a.base_url()),
        "a",
    );
    assert_eq!(
        a1.code, 0,
        "A's sandboxed worker could not move B's ref: {}",
        a1.stderr
    );
    assert_eq!(a1.finished()["pushed"], json!(true));
    assert_eq!(
        repo.local_branch(b_task).as_deref(),
        Some(base.as_str()),
        "A's worker moved B's ref inside the sandbox"
    );

    let mock_b2 = Mock::start(vec![text_reply("nothing")]);
    let b2 = run(
        &repo,
        b_task,
        2,
        "tok-b2",
        &["--resume"],
        &model_args(mock_b2.base_url()),
        "nothing",
    );
    let finished = b2.finished();
    assert_eq!(b2.code, 1, "{}", b2.stderr);
    assert_eq!(finished["pushed"], json!(false));
    assert_eq!(
        finished["failure"],
        json!(format!("work/{b_task} moved outside this task")),
        "the start check names the moved ref: {finished}"
    );
    assert!(
        mock_b2.requests().is_empty(),
        "no child ran for the refused attempt"
    );
    assert!(
        !repo.run_dir(b_task, 2).join("result.jsonl").exists(),
        "the refused attempt wrote no result stream"
    );
    assert!(
        !repo.attempt_dir(b_task, 2).join("identity.json").exists(),
        "the refused attempt started no worker"
    );
    assert_eq!(
        repo.remote_branch(b_task).as_deref(),
        Some(b_head.as_str()),
        "B's remote ref never moves"
    );
}

/// The residual gap of finding 1: another task moves B's ref to an empty commit on top of
/// B's head, so B's tree still matches the branch and `--allow-noop` would pass every
/// delivery check. The start check refuses B's next attempt before its child runs.
#[test]
fn a_moved_ref_is_refused_before_the_next_attempt_runs() {
    let repo = Repo::new("moved-ref");
    let task = "TK0013-moved";
    let branch = format!("work/{task}");
    let mock_b1 = Mock::start(vec![
        shell_reply("c1", "echo b > b.txt && git add b.txt && git commit -m b"),
        text_reply("done"),
    ]);
    let b1 = run(
        &repo,
        task,
        1,
        "tok-m1",
        &[],
        &model_args(mock_b1.base_url()),
        "b",
    );
    assert_eq!(b1.code, 0, "{}", b1.stderr);
    let first_head = b1.finished()["head_sha"].as_str().unwrap().to_owned();
    assert_eq!(
        repo.remote_branch(task).as_deref(),
        Some(first_head.as_str())
    );

    // A commit whose tree is B's tree and whose parent is B's head. Moving the ref to it
    // leaves the worktree clean and the change past the base non-empty; only the sha B's
    // attempt ended on is different. ADR 0038 lets another task's sandboxed worker do this
    // (see the test above); here the test process does it so the case stays independent of
    // the sandbox.
    let tree = repo.git(&["rev-parse", &format!("refs/heads/{branch}^{{tree}}")]);
    let moved = repo.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &format!("refs/heads/{branch}"),
        "-m",
        "moved",
    ]);
    let moved = moved.trim().to_owned();
    repo.git(&["update-ref", &format!("refs/heads/{branch}"), &moved]);
    assert_ne!(moved, first_head, "the ref moved to a new empty commit");

    let mock_b2 = Mock::start(vec![text_reply("nothing")]);
    let b2 = run(
        &repo,
        task,
        2,
        "tok-m2",
        &["--resume", "--allow-noop"],
        &model_args(mock_b2.base_url()),
        "nothing",
    );
    let finished = b2.finished();
    assert_eq!(b2.code, 1, "{}", b2.stderr);
    assert_eq!(finished["pushed"], json!(false));
    assert_eq!(
        finished["failure"],
        json!(format!("{branch} moved outside this task")),
        "{finished}"
    );
    assert!(
        mock_b2.requests().is_empty(),
        "no child ran for the refused attempt"
    );
    assert!(
        !repo.attempt_dir(task, 2).join("identity.json").exists(),
        "the refused attempt started no worker"
    );
    assert!(
        repo.attempt_dir(task, 2).join("status.json").exists(),
        "the refusal is recorded as the attempt's status"
    );
    assert_eq!(
        repo.local_branch(task).as_deref(),
        Some(moved.as_str()),
        "the moved ref stays where the other task put it"
    );
    assert_eq!(
        repo.remote_branch(task).as_deref(),
        Some(first_head.as_str()),
        "the moved sha is never pushed"
    );
}

/// A child killed before it writes a `Result` is `terminated`, not `unknown`, and nothing is
/// pushed.
#[test]
fn a_child_without_a_result_is_terminated_and_not_pushed() {
    let repo = Repo::new("terminated");
    let mock = Mock::start(vec![shell_reply("c1", "sleep 5"), text_reply("done")]);
    let args = work_args(
        "TK0003-kill",
        1,
        "tok-kill",
        &[],
        &model_args(mock.base_url()),
    );
    let finished = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            repo.work(&refs, "kill me")
        });
        let pid = wait_for_pid(&repo.attempt_dir("TK0003-kill", 1).join("identity.json"));
        let killed = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("run kill");
        assert!(killed.success(), "kill -9 {pid}");
        let run = worker.join().expect("the wrapper thread");
        let finished = run.finished();
        (run, finished)
    });
    let (run, finished) = finished;
    assert_eq!(finished["outcome"], json!("terminated"));
    assert_eq!(finished["pushed"], json!(false));
    assert_eq!(finished["exit_code"], json!(137), "SIGKILL is 128 + 9");
    assert!(
        finished["failure"]
            .as_str()
            .unwrap()
            .contains("outcome terminated")
    );
    assert_eq!(run.code, 137);
    assert_eq!(repo.remote_branch("TK0003-kill"), None);
    let result =
        std::fs::read_to_string(repo.run_dir("TK0003-kill", 1).join("result.jsonl")).unwrap();
    assert!(
        !result.contains("\"type\":\"Result\""),
        "the killed child wrote no Result: {result}"
    );
}

/// An outcome other than `completed` never pushes, even when the worker did commit.
#[test]
fn an_outcome_other_than_completed_never_pushes() {
    let repo = Repo::new("budget");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        tool_reply("c2", "read", "{}"),
    ]);
    let mut extra = model_args(mock.base_url());
    extra.push("--max-model-calls".to_owned());
    extra.push("2".to_owned());
    let run = run(
        &repo,
        "TK0004-budget",
        1,
        "tok-budget",
        &[],
        &extra,
        "spend the budget",
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["outcome"], json!("budget_exceeded"));
    assert_eq!(finished["exit_code"], json!(3));
    assert_ne!(
        finished["head_sha"], finished["base_sha"],
        "the worker committed before the budget ran out: {finished}"
    );
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"]
            .as_str()
            .unwrap()
            .contains("outcome budget_exceeded")
    );
    assert_eq!(repo.remote_branch("TK0004-budget"), None);
}

/// An uncommitted file fails the attempt with the dirty list, and nothing is pushed.
#[test]
fn a_dirty_tree_fails_with_its_reason() {
    let repo = Repo::new("dirty");
    let mock = Mock::start(vec![
        write_reply("c1", "dirty.txt", "x"),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0005-dirty",
        1,
        "tok-dirty",
        &[],
        &model_args(mock.base_url()),
        "leave a file",
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"]
            .as_str()
            .unwrap()
            .contains("uncommitted or untracked files"),
        "{finished}"
    );
    assert!(
        finished["dirty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "dirty.txt"),
        "{finished}"
    );
    assert_eq!(repo.remote_branch("TK0005-dirty"), None);
}

/// A branch with no commit past the base fails the delivery check unless the brief allows a
/// no-op; nothing is pushed.
#[test]
fn no_change_fails_with_its_reason() {
    let repo = Repo::new("noop");
    let mock = Mock::start(vec![text_reply("nothing")]);
    let run = run(
        &repo,
        "TK0006-noop",
        1,
        "tok-noop",
        &[],
        &model_args(mock.base_url()),
        "do nothing",
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"].as_str().unwrap().contains("no change"),
        "{finished}"
    );
    assert_eq!(finished["head_sha"], finished["base_sha"]);
    assert_eq!(repo.remote_branch("TK0006-noop"), None);
}

/// `--allow-noop` lets the same unchanged branch through: the checks pass and the ref is
/// pushed at the base commit.
#[test]
fn allow_noop_pushes_an_unchanged_branch() {
    let repo = Repo::new("allow-noop");
    let mock = Mock::start(vec![text_reply("nothing")]);
    let run = run(
        &repo,
        "TK0007-allow-noop",
        1,
        "tok-allow",
        &["--allow-noop"],
        &model_args(mock.base_url()),
        "a no-op is fine",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["pushed"], json!(true));
    assert_eq!(finished["head_sha"], finished["base_sha"]);
    assert_eq!(
        repo.remote_branch("TK0007-allow-noop").as_deref(),
        finished["head_sha"].as_str()
    );
}

/// A commit that changes a file under `.radian/arc/` fails the delivery check: a task can
/// never change arc's files through its branch.
#[test]
fn a_change_under_the_arc_directory_fails_with_its_reason() {
    let repo = Repo::new("arc-path");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "mkdir -p .radian/arc/plans && echo x > .radian/arc/plans/evil.md && git add -A && \
             git commit -m evil",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0008-arc-path",
        1,
        "tok-arc",
        &[],
        &model_args(mock.base_url()),
        "touch arc",
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"]
            .as_str()
            .unwrap()
            .contains("files under .radian/arc/"),
        "{finished}"
    );
    assert_eq!(repo.remote_branch("TK0008-arc-path"), None);
}

/// A `core.hooksPath` in the repository points at hooks that write markers. The wrapper's
/// own git commands (`worktree add`, `push`) must run none of them.
#[test]
fn the_wrappers_git_commands_run_no_repository_hooks() {
    let repo = Repo::new("hooks");
    let (pre_push, post_checkout) = repo.install_hooks();
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0009-hooks",
        1,
        "tok-hooks",
        &[],
        &model_args(mock.base_url()),
        "make a change",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.finished()["pushed"], json!(true));
    assert!(
        !pre_push.exists(),
        "the wrapper's push ran the repository's pre-push hook"
    );
    assert!(
        !post_checkout.exists(),
        "the wrapper's worktree add ran the repository's post-checkout hook"
    );
}

/// `--resume` on attempt 2 continues the same session id: the second `Result` reports two
/// session turns and one turn for its own process.
#[test]
fn resume_on_the_second_attempt_continues_the_same_session() {
    let repo = Repo::new("resume");
    let task = "TK0010-resume";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-r1",
        &[],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let result1 = last_result(&repo.run_dir(task, 1).join("result.jsonl"));
    assert_eq!(result1["session_id"], json!(task));
    assert_eq!(result1["turns"], json!(1));

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-r2",
        &["--resume"],
        &model_args(mock2.base_url()),
        "two",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    let finished = second.finished();
    assert_eq!(finished["pushed"], json!(true));
    let result2 = last_result(&repo.run_dir(task, 2).join("result.jsonl"));
    assert_eq!(result2["session_id"], json!(task), "the same session id");
    assert_eq!(result2["turns"], json!(2), "both attempts' turns");
    assert_eq!(result2["run_turns"], json!(1), "only this process's turn");
    assert_eq!(
        repo.remote_branch(task).as_deref(),
        finished["head_sha"].as_str()
    );
}

/// A first attempt that fails before the model starts (here `radian exec` refuses an unknown
/// worker argument) leaves no session file. The second attempt carries `--resume` but must
/// start the session with `--session-id`: resuming would fail with `no session` on every
/// remaining attempt.
#[test]
fn a_later_attempt_starts_the_session_when_no_earlier_attempt_created_it() {
    let repo = Repo::new("resume-none");
    let task = "TK0011-resume-none";
    let first = run(
        &repo,
        task,
        1,
        "tok-n1",
        &[],
        &["--no-such-worker-flag".to_owned()],
        "one",
    );
    assert_ne!(first.code, 0, "the bad argument fails the worker");
    assert_eq!(first.finished()["outcome"], json!("terminated"));
    assert!(
        !repo.sessions.join(format!("{task}.jsonl")).exists(),
        "the failed attempt created no session"
    );

    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-n2",
        &["--resume"],
        &model_args(mock.base_url()),
        "one",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.finished()["pushed"], json!(true));
    let result = last_result(&repo.run_dir(task, 2).join("result.jsonl"));
    assert_eq!(result["session_id"], json!(task), "the task's session id");
    assert_eq!(
        result["turns"],
        json!(1),
        "a new session, not a resumed one"
    );
}

/// ADR 0048 decision 7: a fresh rework starts a session of its own (`<task>-2`, no
/// `--resume`), its stdin opens with the hand-off section (the previous report, the commit
/// list and the diff stat), and the first attempt's journal is untouched. Mutation: pass
/// `--resume` and the second result reports a resumed session; drop the section and the
/// brief is the body alone.
#[test]
fn a_fresh_rework_starts_a_new_session_with_the_hand_off() {
    let repo = Repo::new("fresh-rework");
    init(&repo);
    // This test is about the hand-off alone: the standing footer has its own tests below.
    std::fs::remove_file(repo.arc().join("worker-brief.md")).expect("a default brief");
    let task = "TK0012-fresh";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m 'first commit on the branch'",
        ),
        text_reply("the first attempt is done"),
    ]);
    let first_session = format!("{task}-1");
    let first = run(
        &repo,
        task,
        1,
        "tok-f1",
        &["--session", first_session.as_str()],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let result1 = last_result(&repo.run_dir(task, 1).join("result.jsonl"));
    assert_eq!(result1["session_id"], json!(first_session));
    assert_eq!(result1["turns"], json!(1));
    let journal = repo.sessions.join(format!("{first_session}.jsonl"));
    assert!(journal.is_file(), "{} exists", journal.display());
    let before = std::fs::read(&journal).expect("the first attempt's journal");

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let second_session = format!("{task}-2");
    let second = run(
        &repo,
        task,
        2,
        "tok-f2",
        &["--session", second_session.as_str()],
        &model_args(mock2.base_url()),
        "second",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.finished()["pushed"], json!(true));
    let result2 = last_result(&repo.run_dir(task, 2).join("result.jsonl"));
    assert_eq!(
        result2["session_id"],
        json!(second_session),
        "the attempt's own session"
    );
    assert_eq!(
        result2["turns"],
        json!(1),
        "a new session, not a resumed one"
    );
    assert_eq!(
        std::fs::read(&journal).expect("the first attempt's journal"),
        before,
        "the first attempt's journal is untouched"
    );

    let brief = brief_of(&mock2.requests()[0]);
    assert!(
        brief.starts_with("## Where the previous attempt left off"),
        "the hand-off comes first:\n{brief}"
    );
    assert!(
        brief.contains("the first attempt is done"),
        "the previous attempt's final report:\n{brief}"
    );
    let commits = brief
        .split("### The commits on the branch")
        .nth(1)
        .expect("the commit list section");
    assert!(
        commits.contains("first commit on the branch"),
        "the commit list names the branch's commit:\n{commits}"
    );
    let stat = brief
        .split("### The change so far")
        .nth(1)
        .expect("the diff stat section");
    assert!(
        stat.contains("one.txt"),
        "the diff stat names the changed file:\n{stat}"
    );
    assert!(
        brief.contains("read the files you need rather than assume"),
        "the hand-off says to read the files:\n{brief}"
    );
    assert!(
        brief.trim_end().ends_with("second"),
        "the brief follows the hand-off:\n{brief}"
    );

    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir(task, 2).join("execution.json"))
            .expect("execution.json"),
    )
    .expect("execution.json parses");
    assert_eq!(
        execution["session_id"],
        json!(second_session),
        "the attempt record names its session"
    );
}

/// A `reverify` attempt in between writes no `result.jsonl`, so the hand-off walks back to
/// the newest attempt that wrote a final report (attempt 1) and says which one it is.
/// Mutation: read only attempt n-1, and the report is the "wrote no final report" line.
#[test]
fn the_hand_off_walks_back_past_a_reverify_attempt() {
    let repo = Repo::new("fresh-reverify");
    let task = "TK0015-reverify-between";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m 'first commit on the branch'",
        ),
        text_reply("the first attempt's report"),
    ]);
    let first_session = format!("{task}-1");
    let first = run(
        &repo,
        task,
        1,
        "tok-rb1",
        &["--session", first_session.as_str()],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);

    // Attempt 2 is the scheduling loop's own worker-less reverify: the attempt is claimed and
    // its records written, but no worker runs, so there is no `result.jsonl`.
    let reverify = repo.run_dir(task, 2);
    std::fs::create_dir_all(&reverify).expect("the reverify attempt's run directory");
    std::fs::write(
        reverify.join("attempt_result.json"),
        "{\"class\":\"reverify\"}\n",
    )
    .expect("the reverify attempt's record");
    assert!(
        !reverify.join("result.jsonl").exists(),
        "a reverify attempt has no worker stream"
    );

    let mock3 = Mock::start(vec![
        shell_reply(
            "c3",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let third_session = format!("{task}-3");
    let third = run(
        &repo,
        task,
        3,
        "tok-rb3",
        &["--session", third_session.as_str()],
        &model_args(mock3.base_url()),
        "third",
    );
    assert_eq!(third.code, 0, "{}", third.stderr);
    let result3 = last_result(&repo.run_dir(task, 3).join("result.jsonl"));
    assert_eq!(result3["session_id"], json!(third_session));

    let brief = brief_of(&mock3.requests()[0]);
    let report = brief
        .split("### The previous attempt's final report")
        .nth(1)
        .expect("the report section");
    assert!(
        report.contains("the first attempt's report"),
        "the newest report survives the reverify attempt:\n{report}"
    );
    assert!(
        report.contains("Attempt 1 is the newest attempt that wrote a final report"),
        "the section names the attempt it came from:\n{report}"
    );
    assert!(
        report.contains("attempt 2 ended without one"),
        "the section says the attempt before it wrote none:\n{report}"
    );
}

/// The hand-off cuts a long previous report at 8,000 characters and says so.
#[test]
fn a_long_previous_report_is_cut_at_the_cap_with_a_note() {
    let repo = Repo::new("fresh-cap");
    let task = "TK0013-cap";
    let long = "x".repeat(9_000);
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply(&long),
    ]);
    let first_session = format!("{task}-1");
    let first = run(
        &repo,
        task,
        1,
        "tok-cap1",
        &["--session", first_session.as_str()],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let second_session = format!("{task}-2");
    let second = run(
        &repo,
        task,
        2,
        "tok-cap2",
        &["--session", second_session.as_str()],
        &model_args(mock2.base_url()),
        "second",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);

    let brief = brief_of(&mock2.requests()[0]);
    assert!(
        brief.contains(&"x".repeat(8_000)),
        "the first 8,000 characters are there"
    );
    assert!(
        !brief.contains(&"x".repeat(8_001)),
        "the report is cut at 8,000 characters"
    );
    assert!(
        brief.contains("cut at 8,000 characters"),
        "the note says the report was cut:\n{brief}"
    );
}

/// A `merge-main` conflict section stays first when a fresh rework also gets the hand-off.
/// Mutation: put the hand-off first and the worker reads about the previous attempt before the
/// merge it must resolve.
#[test]
fn a_fresh_rework_keeps_the_conflict_section_first() {
    let repo = Repo::new("fresh-conflict");
    init(&repo);
    let task = "TK0014-fresh-conflict";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "printf 'task side\\n' > file.txt && git add file.txt && git commit -m task",
        ),
        text_reply("done"),
    ]);
    let first_session = format!("{task}-1");
    let first = run(
        &repo,
        task,
        1,
        "tok-fc1",
        &["--session", first_session.as_str()],
        &model_args(mock1.base_url()),
        "task side",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.finished()["pushed"], json!(true));

    let moved = advance_main(&repo, "file.txt", "main side\n");
    let short = &moved[..7];

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "printf 'resolved\\n' > file.txt && git add file.txt && git commit --no-edit",
        ),
        text_reply("done"),
    ]);
    let second_session = format!("{task}-2");
    let second = run(
        &repo,
        task,
        2,
        "tok-fc2",
        &["--session", second_session.as_str()],
        &model_args(mock2.base_url()),
        "resolve it",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.finished()["pushed"], json!(true));

    let brief = brief_of(&mock2.requests()[0]);
    assert!(
        brief.starts_with("## Before you start: the worktree is merging"),
        "the conflict section is first:\n{brief}"
    );
    assert!(
        brief.contains(&format!("origin/main ({short})")),
        "the conflict section names the merged main:\n{brief}"
    );
    assert!(brief.contains("- file.txt"), "{brief}");
    let conflict = brief.find("- file.txt").expect("the conflicted path");
    let handoff = brief
        .find("## Where the previous attempt left off")
        .expect("the hand-off section");
    assert!(
        conflict < handoff,
        "the conflict section comes before the hand-off:\n{brief}"
    );
    assert!(
        brief.contains("The full history is on the branch"),
        "the hand-off is there:\n{brief}"
    );
}

/// Two wrappers for the same task at once: the second fails on the worktree flock with exit
/// 4 and starts no worker.
#[test]
fn a_second_wrapper_for_the_same_task_fails_on_the_flock() {
    let repo = Repo::new("flock");
    let task = "TK0011-flock";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "sleep 3 && echo x > x.txt && git add x.txt && git commit -m x",
        ),
        text_reply("done"),
    ]);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            run(
                &repo,
                task,
                1,
                "tok-l1",
                &[],
                &model_args(mock1.base_url()),
                "slow",
            )
        });
        wait_for_pid(&repo.attempt_dir(task, 1).join("identity.json"));
        let second = run(&repo, task, 2, "tok-l2", &[], &[], "second");
        assert_eq!(second.code, 4, "{}", second.stderr);
        assert!(
            second.stderr.contains("another attempt holds the lock"),
            "{}",
            second.stderr
        );
        // The locked-out wrapper still writes its record, which is what frees the attempt's
        // concurrency slot in the next pass; it starts no worker.
        let locked_out = second.finished();
        assert_eq!(locked_out["exit_code"], json!(4), "{locked_out}");
        assert_eq!(locked_out["outcome"], json!("terminated"), "{locked_out}");
        assert!(
            locked_out["failure"]
                .as_str()
                .is_some_and(|failure| failure.contains("another attempt holds the lock")),
            "{locked_out}"
        );
        assert!(
            !repo.attempt_dir(task, 2).join("identity.json").exists(),
            "the locked-out wrapper started a worker"
        );
        let first = first.join().expect("the first wrapper");
        assert_eq!(first.code, 0, "{}", first.stderr);
        assert_eq!(first.finished()["pushed"], json!(true));
    });
}

/// ADR 0048 decision 9: `arc` cancels an attempt with SIGTERM on the wrapper. The wrapper
/// forwards it to the worker, waits, and still writes `AttemptFinished` (outcome `cancelled`
/// or `terminated`), with nothing pushed.
#[test]
fn a_sigterm_is_forwarded_and_the_wrapper_still_writes_its_record() {
    let repo = Repo::new("sigterm");
    let task = "TK0016-sigterm";
    let mock = Mock::start(vec![Reply::SseThenHang { pieces: Vec::new() }]);
    let args = work_args(task, 1, "tok-term", &[], &model_args(mock.base_url()));
    let run = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            repo.work(&refs, "wait for the signal")
        });
        let identity = wait_for_pid(&repo.attempt_dir(task, 1).join("identity.json"));
        assert!(identity > 0);
        let record: Value = serde_json::from_str(
            &std::fs::read_to_string(repo.attempt_dir(task, 1).join("identity.json")).unwrap(),
        )
        .unwrap();
        let wrapper = record["wrapper_pid"].as_u64().expect("the wrapper pid") as u32;
        let worker_pid = record["pid"].as_u64().expect("the worker pid") as u32;
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &wrapper.to_string()])
            .status()
            .expect("kill");
        assert!(sent.success(), "kill -TERM {wrapper}");
        let run = worker.join().expect("the wrapper thread");
        let finished = run.finished();
        (run, finished, worker_pid)
    });
    let (run, finished, worker_pid) = run;
    let outcome = finished["outcome"].as_str().unwrap_or_default();
    assert!(
        outcome == "cancelled" || outcome == "terminated",
        "the SIGTERM path ends cancelled or terminated: {finished}"
    );
    assert_eq!(finished["pushed"], json!(false), "{finished}");
    let alive = std::process::Command::new("kill")
        .args(["-0", &worker_pid.to_string()])
        .status()
        .expect("kill -0");
    assert!(
        !alive.success(),
        "the forwarded SIGTERM ended the worker too (code {})",
        run.code
    );
}

/// The worker cannot read the arc files outside its worktree (the deny list reaches both the
/// sandbox and the file tools), while its own worktree reads and writes normally.
#[test]
fn the_worker_cannot_read_the_arc_files_outside_its_worktree() {
    let repo = Repo::new("deny");
    let task = "TK0012-deny";
    let secret = repo.arc().join("runs").join("secret.txt");
    std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
    std::fs::write(&secret, "TOPSECRET-ARC-WORK").unwrap();
    let secret_path = secret.to_str().unwrap().to_owned();
    let mock = Mock::start(vec![
        read_reply("c1", &secret_path),
        shell_reply("c2", &format!("cat '{secret_path}'")),
        write_reply("c3", "worker.txt", "ok"),
        shell_reply("c4", "git add worker.txt && git commit -m worker"),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        task,
        1,
        "tok-deny",
        &[],
        &model_args(mock.base_url()),
        "read arc",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let bodies = mock.request_bodies();
    assert!(
        !bodies.contains("TOPSECRET-ARC-WORK"),
        "the worker's read reached the arc files"
    );
    assert!(
        bodies.contains("outside the workspace"),
        "the read tool was refused: {bodies}"
    );
    // The refusal wording belongs to the backend: the Linux backend masks the denied
    // directory (ENOENT) and classifies it as a sandbox deny, while the macOS backend answers
    // EPERM. Both mean the same thing, and the secret above never reaches the model.
    assert!(
        bodies.contains("sandbox denied: sensitive-read")
            || bodies.contains("Operation not permitted"),
        "the sandbox refused the shell's read: {bodies}"
    );
    let spec = format!("work/{task}:worker.txt");
    assert_eq!(
        repo.git(&["show", spec.as_str()]).trim(),
        "ok",
        "the worker wrote and committed inside its worktree"
    );
    assert_eq!(run.finished()["pushed"], json!(true));
}

/// A worker that rewrites its own branch back behind the tip this attempt started from can
/// never be delivered, even when `--allow-noop` would accept the now-empty change.
#[test]
fn a_worker_that_resets_behind_the_start_is_not_pushed() {
    let repo = Repo::new("reset");
    let task = "TK0014-reset";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-r1",
        &[],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let first_head = first.finished()["head_sha"].as_str().unwrap().to_owned();
    assert_eq!(
        repo.remote_branch(task).as_deref(),
        Some(first_head.as_str())
    );

    let main_ref = format!("{}/{}", repo.remote, repo.main_branch);
    let mock2 = Mock::start(vec![
        shell_reply("c2", &format!("git reset --hard {main_ref}")),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-r2",
        &["--resume", "--allow-noop"],
        &model_args(mock2.base_url()),
        "roll back",
    );
    assert_eq!(second.code, 1, "{}", second.stderr);
    let finished = second.finished();
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"]
            .as_str()
            .unwrap()
            .contains("does not descend from the start"),
        "{finished}"
    );
    assert_eq!(
        repo.remote_branch(task).as_deref(),
        Some(first_head.as_str()),
        "the rollback is not pushed"
    );
}

/// The wrapper uses the configured remote and main branch, never a hardcoded `origin`/`main`:
/// `origin` points at a path with no repository, the real remote is `upstream`, its main
/// branch is `trunk`, and another clone moved `trunk` after this clone's last fetch.
#[test]
fn the_wrapper_fetches_and_pushes_the_configured_remote_and_main_branch() {
    let repo = Repo::with("upstream", "upstream", "trunk");
    repo.dead_remote("origin");
    let other = repo.second_clone("other");
    repo.git_at(&other, &["config", "user.email", "arc@example.invalid"]);
    repo.git_at(&other, &["config", "user.name", "arc test"]);
    std::fs::write(other.join("later.txt"), "later\n").unwrap();
    repo.git_at(&other, &["add", "later.txt"]);
    repo.git_at(&other, &["commit", "-m", "later"]);
    repo.git_at(&other, &["push", "origin", "trunk"]);
    let newer = repo
        .git_at(&other, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    assert_ne!(
        repo.main_sha(),
        newer,
        "this clone's tracking ref is stale before the wrapper fetches"
    );

    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0015-upstream",
        1,
        "tok-upstream",
        &[],
        &model_args(mock.base_url()),
        "make a change",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["pushed"], json!(true));
    assert_eq!(
        finished["base_sha"],
        json!(newer.as_str()),
        "the first attempt starts from the fetched remote main: {finished}"
    );
    let head = finished["head_sha"].as_str().unwrap().to_owned();
    assert_eq!(finished["remote_sha"], json!(head.as_str()));
    assert_eq!(
        repo.remote_branch("TK0015-upstream").as_deref(),
        Some(head.as_str()),
        "the push goes to the configured remote"
    );
    assert_eq!(
        repo.main_sha(),
        newer,
        "the fetch updated the local tracking ref"
    );
}

/// A later attempt whose main moved runs `merge-main` before the worker: the branch holds the
/// merge commit when the worker starts, and the brief the worker gets is unchanged. Mutation:
/// skip the step and the worker's parent is the previous tip, not a merge commit.
#[test]
fn a_second_attempt_merges_main_before_the_worker_runs() {
    let repo = Repo::new("merge-main-clean");
    init(&repo);
    let task = "TK0101-merge-main";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-m1",
        &[],
        &model_args(mock1.base_url()),
        "first",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.finished()["pushed"], json!(true));

    let moved = advance_main(&repo, "main.txt", "main\n");

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-m2",
        &["--resume"],
        &model_args(mock2.base_url()),
        "second",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    let finished = second.finished();
    assert_eq!(finished["pushed"], json!(true));
    let head = finished["head_sha"].as_str().expect("head_sha").to_owned();

    // The worker's commit sits on the merge commit: `head^` has two parents.
    let parent = repo
        .git(&["rev-parse", &format!("{head}^")])
        .trim()
        .to_owned();
    let parents = repo.git(&["log", "-1", "--format=%P", &parent]);
    assert_eq!(
        parents.split_whitespace().count(),
        2,
        "the worker started on a merge commit (parents {parents})"
    );
    assert_eq!(
        repo.git(&["merge-base", &moved, &head]).trim(),
        moved,
        "the branch contains the moved main"
    );
    assert_eq!(merge_commits(&repo, task).len(), 1, "exactly one merge");

    // The brief is the one the task's body holds: no conflict section.
    let brief = brief_of(&mock2.requests()[0]);
    assert!(brief.contains("second"), "{brief}");
    assert!(!brief.contains("Before you start"), "{brief}");
    assert!(!brief.contains("mid-merge"), "{brief}");
}

/// Main moved with a change that conflicts with the task's: the wrapper leaves the worktree
/// mid-merge and the worker's stdin brief starts with the conflict section naming the path. A
/// worker that resolves the conflict and commits the merge delivers normally. Mutation: drop
/// the section and the worker commits without resolving.
#[test]
fn a_conflicting_main_starts_the_worker_mid_merge_with_the_path() {
    let repo = Repo::new("merge-main-conflict");
    init(&repo);
    let task = "TK0102-merge-conflict";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "printf 'task side\\n' > file.txt && git add file.txt && git commit -m task",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-c1",
        &[],
        &model_args(mock1.base_url()),
        "task side",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.finished()["pushed"], json!(true));

    let moved = advance_main(&repo, "file.txt", "main side\n");
    let short = &moved[..7];

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "printf 'resolved\\n' > file.txt && git add file.txt && git commit --no-edit",
        ),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-c2",
        &["--resume"],
        &model_args(mock2.base_url()),
        "resolve it",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    let finished = second.finished();
    assert_eq!(finished["pushed"], json!(true), "{finished}");
    let head = finished["head_sha"].as_str().expect("head_sha").to_owned();
    let parents = repo.git(&["log", "-1", "--format=%P", &head]);
    assert_eq!(
        parents.split_whitespace().count(),
        2,
        "the worker committed the merge (parents {parents})"
    );

    let brief = brief_of(&mock2.requests()[0]);
    assert!(
        brief.starts_with("## Before you start: the worktree is merging"),
        "the conflict section is first:\n{brief}"
    );
    assert!(
        brief.contains(&format!("origin/main ({short})")),
        "the section names the merged main:\n{brief}"
    );
    assert!(brief.contains("- file.txt"), "{brief}");
    assert!(brief.contains("git commit --no-edit"), "{brief}");
}

/// Main did not move: no merge commit, and the worker starts on the previous tip. Mutation:
/// merge unconditionally and this test sees a merge commit.
#[test]
fn main_that_did_not_move_merges_nothing() {
    let repo = Repo::new("merge-main-same");
    init(&repo);
    let task = "TK0103-merge-same";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-s1",
        &[],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let first_head = first.finished()["head_sha"].as_str().unwrap().to_owned();

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-s2",
        &["--resume"],
        &model_args(mock2.base_url()),
        "two",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert!(
        merge_commits(&repo, task).is_empty(),
        "a merge commit appeared"
    );
    let parent = repo
        .git(&["rev-parse", &format!("{first_head}^")])
        .trim()
        .to_owned();
    assert!(!parent.is_empty());
    // The second worker's commit has one parent: the first attempt's commit.
    let head = second.finished()["head_sha"].as_str().unwrap().to_owned();
    let parents = repo.git(&["log", "-1", "--format=%P", &head]);
    assert_eq!(parents.split_whitespace().count(), 1, "{parents}");
}

/// The first attempt starts from the fetched main, so it has nothing to merge even when main
/// moved since this clone last fetched.
#[test]
fn a_first_attempt_merges_nothing() {
    let repo = Repo::new("merge-main-first");
    init(&repo);
    let task = "TK0104-first";
    let moved = advance_main(&repo, "main.txt", "main\n");
    assert_ne!(repo.main_sha(), moved, "the clone's tracking ref is stale");

    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        task,
        1,
        "tok-f1",
        &[],
        &model_args(mock.base_url()),
        "change",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.finished()["base_sha"], json!(moved.as_str()));
    assert!(
        merge_commits(&repo, task).is_empty(),
        "a first attempt merged"
    );
}

/// `before_attempt = []` runs nothing: no fetch, no merge. Mutation: run the step whatever the
/// profile says and the local main ref moves after the remote did.
#[test]
fn an_empty_before_attempt_does_not_fetch_or_merge() {
    let repo = Repo::new("merge-main-off");
    init(&repo);
    tune(
        &repo,
        "before_attempt = [\"merge-main\"]",
        "before_attempt = []",
    );
    let task = "TK0105-off";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-o1",
        &[],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let stale = repo.main_sha();
    advance_main(&repo, "main.txt", "main\n");
    assert_ne!(repo.remote_main_sha(), stale);

    let mock2 = Mock::start(vec![
        shell_reply(
            "c2",
            "echo two > two.txt && git add two.txt && git commit -m two",
        ),
        text_reply("done"),
    ]);
    let second = run(
        &repo,
        task,
        2,
        "tok-o2",
        &["--resume"],
        &model_args(mock2.base_url()),
        "two",
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert!(merge_commits(&repo, task).is_empty(), "it merged main");
    assert_eq!(repo.main_sha(), stale, "it fetched the remote main");
}

/// A clean merge with no worker commit is a no-op attempt: the merge did not move the branch
/// past the tip the attempt started on. The attempt still records that tip, so the next
/// attempt's moved-ref check passes. Mutations: count the merge as delivery (the first attempt
/// below pushes), or record no tip (the second refuses).
#[test]
fn a_clean_merge_with_no_worker_commit_is_a_no_op_attempt() {
    let repo = Repo::new("merge-main-noop");
    init(&repo);
    let task = "TK0106-noop";
    let mock1 = Mock::start(vec![
        shell_reply(
            "c1",
            "echo one > one.txt && git add one.txt && git commit -m one",
        ),
        text_reply("done"),
    ]);
    let first = run(
        &repo,
        task,
        1,
        "tok-n1",
        &[],
        &model_args(mock1.base_url()),
        "one",
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    advance_main(&repo, "main.txt", "main\n");

    let mock2 = Mock::start(vec![text_reply("nothing")]);
    let second = run(
        &repo,
        task,
        2,
        "tok-n2",
        &["--resume"],
        &model_args(mock2.base_url()),
        "nothing",
    );
    let finished = second.finished();
    assert_eq!(second.code, 1, "{}", second.stderr);
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"]
            .as_str()
            .unwrap_or_default()
            .contains("no change"),
        "{finished}"
    );
    assert_eq!(merge_commits(&repo, task).len(), 1, "the merge did happen");

    // The merge commit is the recorded tip: the next attempt's guard passes and its worker
    // delivers normally.
    let mock3 = Mock::start(vec![
        shell_reply(
            "c3",
            "echo three > three.txt && git add three.txt && git commit -m three",
        ),
        text_reply("done"),
    ]);
    let third = run(
        &repo,
        task,
        3,
        "tok-n3",
        &["--resume"],
        &model_args(mock3.base_url()),
        "three",
    );
    assert_eq!(third.code, 0, "{}", third.stderr);
    assert_eq!(third.finished()["pushed"], json!(true));
}

/// `radian arc init` followed by one attempt leaves the main checkout clean: the exclude
/// lines cover every file the wrapper wrote, including the parent directories of
/// `attempts/<task>/{base_sha,last_head}`. Before the join, a run left `.radian/` untracked.
#[test]
fn init_and_an_attempt_leave_the_main_checkout_clean() {
    let repo = Repo::new("clean");
    let init = repo.radian(&["arc", "init"]);
    assert_eq!(init.code, 0, "arc init failed: {}", init.stderr);
    assert_eq!(
        repo.git(&["status", "--porcelain"]).trim(),
        "",
        "init left files the exclude lines do not cover"
    );

    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0001-clean",
        1,
        "tok-clean",
        &[],
        &model_args(mock.base_url()),
        "make a change",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.finished()["pushed"], json!(true));
    assert_eq!(
        repo.git(&["status", "--porcelain"]).trim(),
        "",
        "the exclude lines cover everything the wrapper wrote"
    );
    for name in ["base_sha", "last_head"] {
        assert!(
            repo.arc()
                .join("attempts")
                .join("TK0001-clean")
                .join(name)
                .is_file(),
            "attempts/TK0001-clean/{name} exists under the excluded attempts/ line"
        );
    }
    assert!(
        repo.arc().join("worktrees").join("TK0001-clean").is_dir(),
        "the worktree lives under the excluded worktrees/ line"
    );
}

/// A 1-second stall window gives a 333 ms heartbeat; the gap between two beats must be that
/// interval itself, so three beats fit in the window. Mutation: sleep a whole 100 ms poll
/// slice inside the interval and the gap becomes 400 ms.
#[test]
fn a_one_second_stall_window_keeps_three_beats_inside_it() {
    let repo = Repo::new("heartbeat-gap");
    init(&repo);
    let task = "TK0001-beat";
    // A worker that writes every 100 ms: the stall timer may still fire under load, but the
    // heartbeat keeps beating until the wrapper writes its records.
    let worker = "i=0; while [ $i -lt 25 ]; do echo tick; i=$((i+1)); sleep 0.1; done";
    let mut child = repo.spawn(&[
        "arc",
        "work",
        "--task",
        task,
        "--attempt",
        "1",
        "--token",
        "tok-beat",
        "--worker-command",
        worker,
        "--stall-timeout-secs",
        "1",
        "--max-wall-time-secs",
        "0",
        "--cancel-grace-secs",
        "1",
    ]);
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(b"the brief\n")
            .expect("write the brief");
    }
    let heartbeat = repo.attempt_dir(task, 1).join("heartbeat.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !heartbeat.is_file() {
        assert!(Instant::now() < deadline, "the wrapper wrote no heartbeat");
        std::thread::sleep(Duration::from_millis(10));
    }

    let first_seen = Instant::now();
    let mut beats: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    while first_seen.elapsed() < Duration::from_millis(1600) {
        if let Ok(text) = std::fs::read_to_string(&heartbeat)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
            && let Some(last) = value["last_beat"].as_str()
            && let Ok(at) = chrono::DateTime::parse_from_rfc3339(last)
        {
            let at = at.with_timezone(&chrono::Utc);
            if beats.last() != Some(&at) {
                beats.push(at);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait_with_output().expect("the wrapper ends");

    assert!(
        beats.len() >= 3,
        "only {} heartbeat(s) in 1.6 s: {beats:?}",
        beats.len()
    );
    let gaps: Vec<i64> = beats
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).num_milliseconds())
        .collect();
    // The old loop slept four 100 ms slices for a 333 ms interval: every gap was 400 ms. One
    // late beat under load stretches a single gap, so at most one may be late; the rest stay
    // at the interval itself.
    let on_time = gaps.iter().filter(|gap| **gap <= 375).count();
    assert!(
        on_time + 1 >= gaps.len(),
        "more than one heartbeat gap is late for the 333 ms interval: {gaps:?}"
    );
}

// ---- the standing worker brief (ADR 0048 decision 7) ----------------------------------

/// The repository's standing instructions are the last section of a first attempt's brief,
/// after the task body. Mutation: put the footer in front of the body.
#[test]
fn the_worker_brief_is_the_last_section_of_a_first_attempt() {
    let repo = Repo::new("worker-brief-first");
    init(&repo);
    write_worker_brief(&repo, "## Standing\n\nstand here\n");
    let (captured, worker) = recording_worker(&repo, "brief-first.txt");
    let run = run(
        &repo,
        "TK0001-brief",
        1,
        "tok-brief",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    assert_eq!(
        std::fs::read_to_string(&captured).expect("the worker read stdin"),
        "the body\n\n## Standing\n\nstand here",
        "{}",
        run.stderr
    );
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir("TK0001-brief", 1).join("execution.json"))
            .expect("execution.json"),
    )
    .expect("execution.json parses");
    assert_eq!(execution["notes"], json!([]), "the footer was appended");
}

/// A fresh rework's brief is, in order: the hand-off, the task body, then the footer. Mutation:
/// put the footer before the body, or skip it on a later attempt.
#[test]
fn a_fresh_rework_puts_the_worker_brief_after_the_hand_off_and_the_body() {
    let repo = Repo::new("worker-brief-rework");
    init(&repo);
    write_worker_brief(&repo, "stand here\n");
    let (first_capture, first_worker) = recording_worker(&repo, "brief-rework-1.txt");
    let first = run(
        &repo,
        "TK0002-rework",
        1,
        "tok-r1",
        &["--worker-command", first_worker.as_str()],
        &[],
        "the first body\n",
    );
    assert!(first_capture.is_file(), "{}", first.stderr);

    let (second_capture, second_worker) = recording_worker(&repo, "brief-rework-2.txt");
    let second = run(
        &repo,
        "TK0002-rework",
        2,
        "tok-r2",
        &["--worker-command", second_worker.as_str()],
        &[],
        "the second body\n",
    );
    let brief = std::fs::read_to_string(&second_capture).expect("the worker read stdin");
    let handoff = brief
        .find("## Where the previous attempt left off")
        .unwrap_or_else(|| panic!("no hand-off:\n{brief}\n{}", second.stderr));
    let body = brief.find("the second body").expect("the body");
    let footer = brief.find("stand here").expect("the footer");
    assert!(
        handoff < body && body < footer,
        "the order is hand-off, body, footer:\n{brief}"
    );
    assert!(
        brief.trim_end().ends_with("stand here"),
        "the footer is last:\n{brief}"
    );
}

/// The wrapper reads the file when it composes each attempt's brief, so an edit between two
/// attempts reaches the second worker.
#[test]
fn an_edited_worker_brief_reaches_the_next_attempt() {
    let repo = Repo::new("worker-brief-edit");
    init(&repo);
    write_worker_brief(&repo, "the first standing instructions\n");
    let (first_capture, first_worker) = recording_worker(&repo, "brief-edit-1.txt");
    run(
        &repo,
        "TK0003-edit",
        1,
        "tok-e1",
        &["--worker-command", first_worker.as_str()],
        &[],
        "the body\n",
    );
    assert!(
        std::fs::read_to_string(&first_capture)
            .expect("the first worker read stdin")
            .ends_with("the first standing instructions"),
    );

    write_worker_brief(&repo, "the second standing instructions\n");
    let (second_capture, second_worker) = recording_worker(&repo, "brief-edit-2.txt");
    run(
        &repo,
        "TK0003-edit",
        2,
        "tok-e2",
        &["--worker-command", second_worker.as_str()],
        &[],
        "the body\n",
    );
    let brief = std::fs::read_to_string(&second_capture).expect("the second worker read stdin");
    assert!(
        brief
            .trim_end()
            .ends_with("the second standing instructions"),
        "the edit did not reach the second attempt:\n{brief}"
    );
    assert!(
        !brief.contains("the first standing instructions"),
        "the old text stayed:\n{brief}"
    );
}

/// A missing file and one that is only whitespace leave stdin exactly as it was before the
/// footer existed.
#[test]
fn a_missing_or_blank_worker_brief_leaves_stdin_alone() {
    let missing = Repo::new("worker-brief-missing");
    let (missing_capture, missing_worker) = recording_worker(&missing, "brief-missing.txt");
    run(
        &missing,
        "TK0004-missing",
        1,
        "tok-m1",
        &["--worker-command", missing_worker.as_str()],
        &[],
        "the body\n",
    );
    assert_eq!(
        std::fs::read_to_string(&missing_capture).expect("the worker read stdin"),
        "the body"
    );

    let blank = Repo::new("worker-brief-blank");
    init(&blank);
    write_worker_brief(&blank, "  \n\t\n");
    let (blank_capture, blank_worker) = recording_worker(&blank, "brief-blank.txt");
    run(
        &blank,
        "TK0005-blank",
        1,
        "tok-b1",
        &["--worker-command", blank_worker.as_str()],
        &[],
        "the body\n",
    );
    assert_eq!(
        std::fs::read_to_string(&blank_capture).expect("the worker read stdin"),
        "the body"
    );
}

/// A file over 32 KiB is skipped: the attempt runs with the body alone, the attempt record
/// carries one note line, and `show` prints it. Mutation: drop the size check.
#[test]
fn an_oversized_worker_brief_is_skipped_with_a_note() {
    let repo = Repo::new("worker-brief-large");
    init(&repo);
    write_worker_brief(&repo, &"x".repeat(33 * 1024));
    let store = Store::open(&repo.clone).expect("the store opens");
    let plan = store.add_plan("p", "P", "request").expect("a plan");
    let task = store
        .add_task(
            &plan,
            "large",
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the body\n",
        )
        .expect("a task");
    let (captured, worker) = recording_worker(&repo, "brief-large.txt");
    let run = run(
        &repo,
        &task,
        1,
        "tok-l1",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    assert_eq!(
        std::fs::read_to_string(&captured).expect("the worker read stdin"),
        "the body",
        "the oversized footer was appended: {}",
        run.stderr
    );
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir(&task, 1).join("execution.json"))
            .expect("execution.json"),
    )
    .expect("execution.json parses");
    let note = execution["notes"][0]
        .as_str()
        .unwrap_or_else(|| panic!("no note: {execution}"));
    assert!(
        note.starts_with("worker-brief.md skipped: "),
        "the note names the file: {note}"
    );
    assert!(note.contains("32768"), "the note names the limit: {note}");

    // `show` prints the current attempt's notes, and the wrapper ran outside a claim here: the
    // task file has to name attempt 1 for `show` to find the record the wrapper wrote.
    store
        .change_task(&task, |task| {
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            Ok(())
        })
        .expect("the task names attempt 1");
    let shown = repo.radian(&["arc", "show", &task]);
    assert_eq!(shown.code, 0, "{}", shown.stderr);
    assert!(
        shown
            .stdout
            .contains("attempt_note: worker-brief.md skipped: "),
        "show prints the note:\n{}",
        shown.stdout
    );
}

/// A file that is not UTF-8 is skipped the same way: the attempt still runs, with one note.
#[test]
fn a_worker_brief_that_is_not_utf8_is_skipped_with_a_note() {
    let repo = Repo::new("worker-brief-binary");
    init(&repo);
    std::fs::write(repo.arc().join("worker-brief.md"), [0xff, 0xfe, 0xfd])
        .expect("a binary worker-brief.md");
    let (captured, worker) = recording_worker(&repo, "brief-binary.txt");
    let run = run(
        &repo,
        "TK0006-binary",
        1,
        "tok-n1",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    assert_eq!(
        std::fs::read_to_string(&captured).expect("the worker read stdin"),
        "the body",
        "the non-UTF-8 footer was appended: {}",
        run.stderr
    );
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir("TK0006-binary", 1).join("execution.json"))
            .expect("execution.json"),
    )
    .expect("execution.json parses");
    assert_eq!(
        execution["notes"],
        json!(["worker-brief.md skipped: not UTF-8"])
    );
}

/// Writes a verdict file the way the gate does: `runs/<task>/attempts/<task>-<n>/verdicts/<stage>.json`.
fn write_verdict(store: &Store, task: &str, attempt: u32, stage: &str, bytes: &[u8]) -> PathBuf {
    let path = store
        .root()
        .join("runs")
        .join(task)
        .join("attempts")
        .join(format!("{task}-{attempt}"))
        .join("verdicts")
        .join(format!("{stage}.json"));
    std::fs::create_dir_all(path.parent().expect("the verdict directory"))
        .expect("the verdict directory");
    std::fs::write(&path, bytes).expect("the verdict");
    path
}

/// One plan and one task whose body is `body`, in `store`.
fn plan_and_task(store: &Store, slug: &str, body: &str) -> String {
    let plan = store.add_plan("p", "P", "request").expect("a plan");
    store
        .add_task(&plan, slug, "A task", Vec::new(), false, Vec::new(), body)
        .expect("a task")
}

/// The brief of a review rework (ADR 0048 decision 9) naming `verdict`.
fn review_rework_body(verdict: &Path) -> String {
    format!(
        "## Rework 1 (review)\n\nThe review found these items. Fix them and nothing else, append \
         commits to the branch, and do not rebase.\n\n- [major] one.txt:1 the line is wrong\n\n\
         Read the verdict for detail: `{}`",
        verdict.display()
    )
}

/// A review rework's stdin names the verdict's copy in the worktree, the copy holds the
/// verdict's bytes, and the task file keeps the absolute path (ADR 0048 decision 7).
/// Mutations: copy but keep the absolute path in the text; rewrite the path without copying.
#[test]
fn a_review_rework_reads_a_copy_of_its_verdict() {
    let repo = Repo::new("verdict-copy-review");
    init(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let task = plan_and_task(&store, "work", "the brief\n");
    let bytes =
        b"{\"summary\":\"the verdict\",\"blocking\":[],\"suggestions\":[],\"verified\":[]}\n";
    let verdict = write_verdict(&store, &task, 1, "review", bytes);
    let body = review_rework_body(&verdict);
    store
        .change_task(&task, |task| {
            task.body = body;
            Ok(())
        })
        .expect("the rework brief");
    let body = store.load_task(&task).expect("the task").item.body;

    let (captured, worker) = recording_worker(&repo, "verdict-review-stdin.txt");
    let run = run(
        &repo,
        &task,
        2,
        "tok-v1",
        &["--worker-command", worker.as_str()],
        &[],
        &body,
    );
    let captured = std::fs::read_to_string(&captured).expect("the worker read stdin");
    let relative = format!(".radian/arc-verdicts/{task}/{task}-1/review.json");
    assert!(
        captured.contains(&relative),
        "the brief names the copy:\n{captured}\nstderr: {}",
        run.stderr
    );
    assert!(
        !captured.contains(".radian/arc/runs/"),
        "the absolute path is still in the brief:\n{captured}"
    );
    let copy = repo.worktree(&task).join(&relative);
    assert_eq!(
        std::fs::read(&copy).expect("the copy exists in the worktree"),
        bytes,
        "the copy is the verdict's bytes"
    );
    let stored = store.load_task(&task).expect("the task").item;
    assert!(
        stored.body.contains(&verdict.display().to_string()),
        "the task file keeps the absolute path:\n{}",
        stored.body
    );
}

/// A follow-up brief names two verdicts of another task: both are copied under that task's
/// own ids and both paths are rewritten (ADR 0048 decision 7).
#[test]
fn a_followup_brief_copies_two_verdicts_of_another_task() {
    let repo = Repo::new("verdict-copy-followup");
    init(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let plan = store.add_plan("p", "P", "request").expect("a plan");
    let other = store
        .add_task(
            &plan,
            "other",
            "Other",
            Vec::new(),
            false,
            Vec::new(),
            "the other brief\n",
        )
        .expect("the other task");
    let first_bytes = b"{\"summary\":\"first\"}\n";
    let second_bytes = b"{\"summary\":\"second\"}\n";
    let first = write_verdict(&store, &other, 1, "review", first_bytes);
    let second = write_verdict(&store, &other, 2, "review", second_bytes);
    let followup = store
        .add_task(
            &plan,
            "work",
            "Follow-ups",
            vec![other.clone()],
            false,
            Vec::new(),
            "placeholder\n",
        )
        .expect("the follow-up task");
    let body = format!(
        "The items below are from the review of `{other}`, which is now merged; fix each one, or \
         leave it with a one-line reason in the final answer.\n\nFrom review 1 (attempt 1):\n- \
         [minor] one.txt:1 a nicer name\n\nRead the verdicts for detail: `{}`, `{}`",
        first.display(),
        second.display()
    );
    store
        .change_task(&followup, |task| {
            task.body = body;
            Ok(())
        })
        .expect("the follow-up brief");
    let body = store.load_task(&followup).expect("the task").item.body;

    let (captured, worker) = recording_worker(&repo, "verdict-followup-stdin.txt");
    let run = run(
        &repo,
        &followup,
        1,
        "tok-v2",
        &["--worker-command", worker.as_str()],
        &[],
        &body,
    );
    let captured = std::fs::read_to_string(&captured).expect("the worker read stdin");
    for (attempt, bytes) in [(1, first_bytes as &[u8]), (2, second_bytes)] {
        let relative = format!(".radian/arc-verdicts/{other}/{other}-{attempt}/review.json");
        assert!(
            captured.contains(&relative),
            "the brief names the copy:\n{captured}\nstderr: {}",
            run.stderr
        );
        assert_eq!(
            std::fs::read(repo.worktree(&followup).join(&relative)).expect("the copy exists"),
            bytes
        );
    }
    assert!(
        !captured.contains(".radian/arc/runs/"),
        "an absolute path is still in the brief:\n{captured}"
    );
}

/// A verdict the brief names that does not exist stays in the text; the attempt runs with one
/// note line, and `show` prints it (ADR 0048 decision 7). Mutation: fail the attempt instead.
#[test]
fn a_missing_verdict_stays_in_the_brief_and_is_noted() {
    let repo = Repo::new("verdict-copy-missing");
    init(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let task = plan_and_task(&store, "work", "the brief\n");
    let missing = store
        .root()
        .join("runs")
        .join(&task)
        .join("attempts")
        .join(format!("{task}-1"))
        .join("verdicts/review.json");
    let body = review_rework_body(&missing);
    store
        .change_task(&task, |task| {
            task.body = body;
            Ok(())
        })
        .expect("the rework brief");
    let body = store.load_task(&task).expect("the task").item.body;

    let (captured, worker) = recording_worker(&repo, "verdict-missing-stdin.txt");
    let run = run(
        &repo,
        &task,
        1,
        "tok-v3",
        &["--worker-command", worker.as_str()],
        &[],
        &body,
    );
    let captured = std::fs::read_to_string(&captured).expect("the attempt ran");
    assert!(
        captured.contains(&missing.display().to_string()),
        "the missing path stays in the brief:\n{captured}\nstderr: {}",
        run.stderr
    );
    assert!(!captured.contains(".radian/arc-verdicts/"), "{captured}");
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir(&task, 1).join("execution.json"))
            .expect("execution.json"),
    )
    .expect("execution.json parses");
    let note = execution["notes"][0]
        .as_str()
        .unwrap_or_else(|| panic!("no note: {execution}"));
    assert!(
        note.starts_with(&format!("verdict copy skipped: {}: ", missing.display())),
        "the note names the path: {note}"
    );

    // `show` prints the current attempt's notes; the wrapper ran outside a claim here.
    store
        .change_task(&task, |task| {
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            Ok(())
        })
        .expect("the task names attempt 1");
    let shown = repo.radian(&["arc", "show", &task]);
    assert_eq!(shown.code, 0, "{}", shown.stderr);
    assert!(
        shown
            .stdout
            .contains("attempt_note: verdict copy skipped: "),
        "show prints the note:\n{}",
        shown.stdout
    );
}

/// A link the worker planted in its worktree in an earlier attempt does not redirect the
/// copy: the wrapper writes nothing outside the worktree, and the path stays in the brief
/// with a note (ADR 0048 decision 7).
#[cfg(unix)]
#[test]
fn a_verdict_copy_never_follows_a_link_the_worktree_already_has() {
    let repo = Repo::new("verdict-copy-worktree-link");
    init(&repo);
    let store = Store::open(&repo.clone).expect("the store opens");
    let task = plan_and_task(&store, "work", "the brief\n");
    let outside = repo.root.join("outside");
    std::fs::create_dir_all(&outside).expect("the outside directory");

    // Attempt 1's worker plants the copy directory of attempt 1 as a link out of the
    // worktree. The next attempt is the review rework that would write the copy there.
    let (first_capture, _) = recording_worker(&repo, "verdict-link-first-stdin.txt");
    let worktree = repo.worktree(&task);
    let command = format!(
        "mkdir -p '{wt}/.radian/arc-verdicts/{task}' && ln -s '{}' \
         '{wt}/.radian/arc-verdicts/{task}/{task}-1' && cat > '{}'",
        outside.display(),
        first_capture.display(),
        wt = worktree.display(),
    );
    let first = run(
        &repo,
        &task,
        1,
        "tok-link-1",
        &["--worker-command", command.as_str()],
        &[],
        "the brief\n",
    );
    assert!(
        first_capture.is_file(),
        "the first worker ran: {}",
        first.stderr
    );

    let bytes = b"{\"summary\":\"the verdict\"}\n";
    let verdict = write_verdict(&store, &task, 1, "review", bytes);
    let body = review_rework_body(&verdict);
    store
        .change_task(&task, |task| {
            task.body = body;
            Ok(())
        })
        .expect("the rework brief");
    let body = store.load_task(&task).expect("the task").item.body;

    let (captured, worker) = recording_worker(&repo, "verdict-link-second-stdin.txt");
    let run = run(
        &repo,
        &task,
        2,
        "tok-link-2",
        &["--worker-command", worker.as_str()],
        &[],
        &body,
    );
    let captured = std::fs::read_to_string(&captured).expect("the attempt ran");
    assert!(
        captured.contains(&verdict.display().to_string()),
        "the path stays in the brief:\n{captured}\nstderr: {}",
        run.stderr
    );
    assert!(!captured.contains(".radian/arc-verdicts/"), "{captured}");
    assert!(
        !outside.join("review.json").exists(),
        "nothing was written through the link"
    );
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir(&task, 2).join("execution.json"))
            .expect("execution.json"),
    )
    .expect("execution.json parses");
    let note = execution["notes"][0]
        .as_str()
        .unwrap_or_else(|| panic!("no note: {execution}"));
    assert!(
        note.starts_with(&format!("verdict copy skipped: {}: ", verdict.display()))
            && note.contains("symbolic link"),
        "the note names the link: {note}"
    );
}

/// The main clone's common git directory.
fn common_git_dir(repo: &Repo) -> PathBuf {
    PathBuf::from(repo.git(&["rev-parse", "--absolute-git-dir"]).trim())
}

/// Appends one git configuration section to the common `config` from a `--worker-command`
/// line, the way a worker with a wider sandbox could.
fn append_to_common_config(repo: &Repo, lines: &str) -> String {
    let config = common_git_dir(repo).join("config");
    format!("cat >> '{}' <<'EOF'\n{lines}\nEOF", config.display())
}

/// The copy is ignored in the worktree, so it is not committed, `git status` stays clean, and
/// the delivery check pushes the attempt (ADR 0048 decisions 7 and 8).
#[test]
fn a_verdict_copy_never_reaches_the_branch() {
    let repo = Repo::new("verdict-copy-clean");
    init(&repo);
    // The repository's `.gitignore` ignores `/.radian/`; the test clone needs the same rule.
    let exclude = repo.clone.join(".git/info/exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str("\n/.radian/\n");
    std::fs::write(&exclude, text).expect("the clone's exclude file");
    let store = Store::open(&repo.clone).expect("the store opens");
    let task = plan_and_task(&store, "work", "the brief\n");
    let verdict = write_verdict(
        &store,
        &task,
        1,
        "review",
        b"{\"summary\":\"the verdict\"}\n",
    );
    let body = review_rework_body(&verdict);
    store
        .change_task(&task, |task| {
            task.body = body;
            Ok(())
        })
        .expect("the rework brief");
    let body = store.load_task(&task).expect("the task").item.body;

    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        &task,
        1,
        "tok-v4",
        &[],
        &model_args(mock.base_url()),
        &body,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(finished["pushed"], json!(true));
    assert_eq!(finished["dirty"], json!([]), "the copy is not dirty");

    let worktree = repo.worktree(&task);
    let copy = worktree.join(format!(".radian/arc-verdicts/{task}/{task}-1/review.json"));
    assert!(copy.is_file(), "the copy is in the worktree");
    assert_eq!(repo.git_at(&worktree, &["status", "--short"]), "");
    let tracked = repo.git_at(&worktree, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tracked.contains(".radian"),
        "the copies are not committed:\n{tracked}"
    );
}

/// The wrapper's git status runs after the control check: a worker cannot hide a dirty file
/// behind a config it wrote (ADR 0048 decisions 7 and 8).
#[test]
fn the_worker_cannot_hide_a_dirty_file_from_the_check() {
    let repo = Repo::new("control-excludes");
    let config = common_git_dir(&repo).join("config");
    let excludes = repo.root.join("worker-excludes");
    std::fs::write(&excludes, "change.txt\n").expect("the excludes file");
    let worktree = repo.worktree("TK0006-excludes");
    let worker = format!(
        "printf 'change.txt\\n' > '{worktree}/change.txt' \
         && cat >> '{config}' <<'EOF'\n[core]\n\texcludesFile = {excludes}\nEOF",
        worktree = worktree.display(),
        config = config.display(),
        excludes = excludes.display(),
    );
    let run = run(
        &repo,
        "TK0006-excludes",
        1,
        "tok-excludes",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(failure.contains("config"), "{failure}");
    assert_eq!(
        finished["dirty"],
        json!(["change.txt"]),
        "the wrapper's status ran after the control check, without the worker's config"
    );
}

/// The wrapper's worktree setup writes no branch section into the common config, which another
/// attempt's control check would read as the worker's change (ADR 0048 decision 7).
#[test]
fn the_wrapper_creates_the_branch_without_tracking() {
    let repo = Repo::new("control-no-track");
    run(
        &repo,
        "TK0007-notrack",
        1,
        "tok-notrack",
        &["--worker-command", "true"],
        &[],
        "the body\n",
    );
    let config =
        std::fs::read_to_string(common_git_dir(&repo).join("config")).expect("the common config");
    assert!(
        !config.contains("branch \"work/TK0007-notrack\""),
        "the wrapper wrote a branch section:\n{config}"
    );
}

/// A worker that appends to the common `config` fails the attempt, the file is put back, and
/// nothing is pushed (ADR 0048 decisions 7 and 8).
#[test]
fn a_worker_that_changes_the_common_config_fails_and_is_restored() {
    let repo = Repo::new("control-config");
    // The first attempt creates the worktree: the second attempt then records exactly the
    // config a reused worktree sees, and compares against that.
    let first = run(
        &repo,
        "TK0001-config",
        1,
        "tok-first",
        &["--worker-command", "true"],
        &[],
        "the body\n",
    );
    assert_eq!(first.finished()["pushed"], json!(false));
    let config = common_git_dir(&repo).join("config");
    let before = std::fs::read(&config).expect("the common config");
    let worker = append_to_common_config(&repo, "[arc-test]\n\tchanged = true");
    let run = run(
        &repo,
        "TK0001-config",
        2,
        "tok-config",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(
        failure.starts_with("the worker changed git control files: "),
        "{failure}"
    );
    assert!(
        failure.contains("config"),
        "the reason names config: {failure}"
    );
    assert_eq!(finished["pushed"], json!(false));
    assert_eq!(
        std::fs::read(&config).expect("the config is back"),
        before,
        "the recorded content is restored"
    );
    assert_eq!(
        repo.remote_branch("TK0001-config"),
        None,
        "nothing was pushed"
    );
}

/// The check runs before any git command of the wrapper after the worker: none of them may
/// execute a `core.fsmonitor` the worker wrote (ADR 0048 decisions 7 and 8).
#[test]
fn no_wrapper_git_command_runs_the_workers_fsmonitor() {
    let repo = Repo::new("control-fsmonitor");
    let config = common_git_dir(&repo).join("config");
    let marker = repo.root.join("fsmonitor-ran");
    let script = repo.root.join("fsmonitor.sh");
    let worker = format!(
        "printf '#!/bin/sh\\necho ran > \"{marker}\"\\nexit 1\\n' > '{script}' \
         && chmod +x '{script}' \
         && cat >> '{config}' <<'EOF'\n[core]\n\tfsmonitor = {script}\nEOF",
        marker = marker.display(),
        script = script.display(),
        config = config.display(),
    );
    let run = run(
        &repo,
        "TK0002-fsmonitor",
        1,
        "tok-fsmonitor",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(failure.contains("config"), "{failure}");
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        !marker.exists(),
        "a wrapper git command ran the worker's fsmonitor"
    );
}

/// A worker that adds a hook fails the attempt, and the hook is removed (ADR 0048 decisions
/// 7 and 8).
#[test]
fn a_worker_that_adds_a_hook_fails_and_the_hook_is_removed() {
    let repo = Repo::new("control-hook");
    let hook = common_git_dir(&repo).join("hooks/pre-commit");
    let worker = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > '{hook}' && chmod +x '{hook}'",
        hook = hook.display()
    );
    let run = run(
        &repo,
        "TK0003-hook",
        1,
        "tok-hook",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(failure.contains("hooks/pre-commit"), "{failure}");
    assert_eq!(finished["pushed"], json!(false));
    assert!(!hook.exists(), "the hook the worker added is removed");
}

/// A control file too large to read whole fails the attempt and is removed instead of being
/// loaded into the wrapper's memory (ADR 0048 decisions 7 and 8).
#[test]
fn an_oversized_control_file_fails_the_attempt_and_is_removed() {
    let repo = Repo::new("control-oversized");
    let hook = common_git_dir(&repo).join("hooks/big");
    let worker = format!(
        "dd if=/dev/zero of='{hook}' bs=1048576 count=2 2>/dev/null",
        hook = hook.display()
    );
    let run = run(
        &repo,
        "TK0008-oversized",
        1,
        "tok-oversized",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(failure.contains("hooks/big"), "{failure}");
    assert_eq!(finished["pushed"], json!(false));
    assert!(!hook.exists(), "the oversized file is removed");
}

/// A worker that only changes tracked files and commits leaves the git control files alone,
/// so the attempt passes as before (ADR 0048 decisions 7 and 8).
#[test]
fn a_worker_that_only_commits_tracked_files_still_passes() {
    let repo = Repo::new("control-clean");
    let mock = Mock::start(vec![
        shell_reply(
            "c1",
            "echo change > change.txt && git add change.txt && git commit -m change",
        ),
        text_reply("done"),
    ]);
    let run = run(
        &repo,
        "TK0004-clean",
        1,
        "tok-clean",
        &[],
        &model_args(mock.base_url()),
        "make a change\n",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let finished = run.finished();
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(finished["failure"], json!(null));
    assert_eq!(finished["pushed"], json!(true));
    assert_eq!(
        repo.remote_branch("TK0004-clean").as_deref(),
        finished["head_sha"].as_str(),
        "the wrapper pushed the attempt"
    );
}

/// A worker that writes `config.worktree` in the worktree's own git dir fails the attempt and
/// the file is removed (ADR 0048 decisions 7 and 8).
#[test]
fn a_worker_that_changes_its_own_config_worktree_fails() {
    let repo = Repo::new("control-config-worktree");
    let worktree = repo.worktree("TK0005-config-worktree");
    let worker = format!(
        "d=$(git -C '{worktree}' rev-parse --absolute-git-dir) \
         && printf '[worktree]\\n\\tmarker = 1\\n' > \"$d/config.worktree\"",
        worktree = worktree.display()
    );
    let run = run(
        &repo,
        "TK0005-config-worktree",
        1,
        "tok-worktree",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(failure.contains("config.worktree"), "{failure}");
    assert_eq!(finished["pushed"], json!(false));
    let git_dir = PathBuf::from(
        repo.git_at(&worktree, &["rev-parse", "--absolute-git-dir"])
            .trim(),
    );
    assert!(
        !git_dir.join("config.worktree").exists(),
        "the worker's config.worktree is removed"
    );
}

/// A process the worker leaves in its own process group cannot change a control file after the
/// direct child exits: the wrapper stops the whole group before it compares (ADR 0048
/// decisions 7 and 8).
#[test]
fn a_background_process_cannot_change_a_control_file_after_the_worker_exits() {
    let repo = Repo::new("control-background");
    let config = common_git_dir(&repo).join("config");
    let before = std::fs::read(&config).expect("the common config");
    let trigger = repo.root.join("background-trigger");
    let changed = repo.root.join("background-changed");
    // The background process waits for the test to let it write, so it acts after the wrapper
    // has observed the direct child's exit, whatever the poll timing.
    let worker = format!(
        "(while [ ! -e '{trigger}' ]; do sleep 0.02; done \
          && printf '[arc-test]\\n\\tchanged = true\\n' >> '{config}' \
          && printf changed > '{changed}') &",
        trigger = trigger.display(),
        config = config.display(),
        changed = changed.display(),
    );
    let run = run(
        &repo,
        "TK0009-background",
        1,
        "tok-background",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    // A process that outlived the wrapper is still there to see the trigger. Nothing is there
    // when the wrapper stopped the group, as it must.
    std::fs::write(&trigger, "").expect("the trigger");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !changed.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !changed.exists(),
        "a process the worker left behind outlived the attempt"
    );
    assert_eq!(
        std::fs::read(&config).expect("the common config"),
        before,
        "the background process changed the common config after the check"
    );
    assert_eq!(run.finished()["pushed"], json!(false));
}

/// A worker that makes a recorded control file unreadable changed it, as far as the check can
/// tell: the attempt fails with the changed-files reason and the recorded content comes back
/// (ADR 0048 decisions 7 and 8).
#[test]
fn an_unreadable_control_file_is_restored_and_fails_the_attempt() {
    let repo = Repo::new("control-unreadable");
    let config = common_git_dir(&repo).join("config");
    let before = std::fs::read(&config).expect("the common config");
    let worker = format!(
        "cat >> '{config}' <<'EOF'\n[arc-test]\n\tchanged = true\nEOF\n\
         chmod 000 '{config}'",
        config = config.display(),
    );
    let run = run(
        &repo,
        "TK0010-unreadable",
        1,
        "tok-unreadable",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(
        failure.starts_with("the worker changed git control files: "),
        "{failure}"
    );
    assert!(failure.contains("config"), "{failure}");
    assert_eq!(finished["pushed"], json!(false));
    assert_eq!(
        std::fs::read(&config).expect("the config is back"),
        before,
        "the recorded content is restored"
    );
}

/// A control directory the wrapper cannot read back is a change too; when the restore cannot
/// repair it, the reason carries both the changed paths and the restore error, and no further
/// git command runs (ADR 0048 decisions 7 and 8).
#[test]
fn an_unreadable_hooks_directory_reports_both_reasons() {
    let repo = Repo::new("control-unreadable-hooks");
    let hooks = common_git_dir(&repo).join("hooks");
    let worker = format!("chmod 000 '{}'", hooks.display());
    let run = run(
        &repo,
        "TK0011-unreadable-hooks",
        1,
        "tok-unreadable-hooks",
        &["--worker-command", worker.as_str()],
        &[],
        "the body\n",
    );
    let finished = run.finished();
    let failure = finished["failure"]
        .as_str()
        .unwrap_or_else(|| panic!("the attempt failed: {finished}"));
    assert!(
        failure.starts_with("the worker changed git control files: "),
        "{failure}"
    );
    assert!(failure.contains("hooks"), "{failure}");
    assert!(failure.contains("restoring them failed"), "{failure}");
    assert_eq!(finished["pushed"], json!(false));
    // Leave the temporary repository removable; the wrapper could not repair the directory.
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o755));
}

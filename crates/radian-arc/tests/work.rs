//! `radian arc work` end to end (ADR 0048 decisions 7 and 8): a real `radian` binary runs
//! a real `radian exec` child under the mock model, against a temp bare origin and a temp
//! main clone.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
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
        "0001-happy",
        1,
        "tok-happy",
        &[],
        &model_args(mock.base_url()),
        "make a change",
    );
    assert_eq!(run.code, 0, "{}", run.stderr);

    let finished = run.finished();
    assert_eq!(finished["attempt_id"], json!("0001-happy-1"));
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
        repo.remote_branch("0001-happy").as_deref(),
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
            repo.run_dir("0001-happy", 1).join(name).exists(),
            "runs/…/{name} exists"
        );
    }
    for name in ["identity.json", "status.json"] {
        assert!(
            repo.attempt_dir("0001-happy", 1).join(name).exists(),
            "attempts/…/{name} exists"
        );
    }
    let identity: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.attempt_dir("0001-happy", 1).join("identity.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(identity["token"], json!("tok-happy"));
    assert_eq!(identity["session_id"], json!("0001-happy"));
    assert_eq!(identity["branch"], json!("work/0001-happy"));
    assert!(identity["pid"].as_u64().unwrap() > 0);
    assert!(
        identity["process_started_at"].as_str().is_some(),
        "the worker's process start time is recorded"
    );
    let execution: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.run_dir("0001-happy", 1).join("execution.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(execution["base_sha"], json!(base.as_str()));
    assert_eq!(execution["head_sha"], json!(head.as_str()));
    assert_eq!(execution["attempt_id"], json!("0001-happy-1"));

    let result = last_result(&repo.run_dir("0001-happy", 1).join("result.jsonl"));
    assert_eq!(result["outcome"], json!("completed"));
    assert_eq!(result["session_id"], json!("0001-happy"));
    let written =
        std::fs::read_to_string(repo.run_dir("0001-happy", 1).join("attempt_finished.json"))
            .unwrap();
    assert_eq!(
        written.trim(),
        run.stdout.trim(),
        "attempt_finished.json is the stdout line"
    );
}

/// While task B's worktree exists, task A's sandboxed worker moves B's ref back to the
/// base commit. B's next attempt reads the moved ref and its tree no longer matches HEAD:
/// the wrapper fails it and pushes nothing. B's remote ref stays where B left it.
#[test]
fn another_tasks_moved_ref_fails_the_owners_next_check_without_a_push() {
    let repo = Repo::new("cross-ref");
    let base = repo.main_sha();
    let b_task = "0002-b";
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
        "0001-a",
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
    assert_eq!(finished["outcome"], json!("completed"));
    assert_eq!(finished["pushed"], json!(false));
    assert!(
        finished["failure"]
            .as_str()
            .unwrap()
            .contains("uncommitted or untracked"),
        "the moved ref left B's tree dirty: {finished}"
    );
    assert_eq!(
        repo.remote_branch(b_task).as_deref(),
        Some(b_head.as_str()),
        "B's remote ref never moves"
    );
}

/// A child killed before it writes a `Result` is `terminated`, not `unknown`, and nothing is
/// pushed.
#[test]
fn a_child_without_a_result_is_terminated_and_not_pushed() {
    let repo = Repo::new("terminated");
    let mock = Mock::start(vec![shell_reply("c1", "sleep 5"), text_reply("done")]);
    let args = work_args(
        "0003-kill",
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
        let pid = wait_for_pid(&repo.attempt_dir("0003-kill", 1).join("identity.json"));
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
    assert_eq!(repo.remote_branch("0003-kill"), None);
    let result =
        std::fs::read_to_string(repo.run_dir("0003-kill", 1).join("result.jsonl")).unwrap();
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
        "0004-budget",
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
    assert_eq!(repo.remote_branch("0004-budget"), None);
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
        "0005-dirty",
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
    assert_eq!(repo.remote_branch("0005-dirty"), None);
}

/// A branch with no commit past the base fails the delivery check unless the brief allows a
/// no-op; nothing is pushed.
#[test]
fn no_change_fails_with_its_reason() {
    let repo = Repo::new("noop");
    let mock = Mock::start(vec![text_reply("nothing")]);
    let run = run(
        &repo,
        "0006-noop",
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
    assert_eq!(repo.remote_branch("0006-noop"), None);
}

/// `--allow-noop` lets the same unchanged branch through: the checks pass and the ref is
/// pushed at the base commit.
#[test]
fn allow_noop_pushes_an_unchanged_branch() {
    let repo = Repo::new("allow-noop");
    let mock = Mock::start(vec![text_reply("nothing")]);
    let run = run(
        &repo,
        "0007-allow-noop",
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
        repo.remote_branch("0007-allow-noop").as_deref(),
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
        "0008-arc-path",
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
    assert_eq!(repo.remote_branch("0008-arc-path"), None);
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
        "0009-hooks",
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
    let task = "0010-resume";
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

/// Two wrappers for the same task at once: the second fails on the worktree flock with exit
/// 4 and starts no worker.
#[test]
fn a_second_wrapper_for_the_same_task_fails_on_the_flock() {
    let repo = Repo::new("flock");
    let task = "0011-flock";
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
        assert!(
            !second.stdout.contains("AttemptFinished"),
            "the locked-out wrapper starts no worker: {}",
            second.stdout
        );
        let first = first.join().expect("the first wrapper");
        assert_eq!(first.code, 0, "{}", first.stderr);
        assert_eq!(first.finished()["pushed"], json!(true));
    });
}

/// The worker cannot read the arc files outside its worktree (the deny list reaches both the
/// sandbox and the file tools), while its own worktree reads and writes normally.
#[test]
fn the_worker_cannot_read_the_arc_files_outside_its_worktree() {
    let repo = Repo::new("deny");
    let task = "0012-deny";
    let secret = repo.arc().join("runs").join("secret.txt");
    std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
    std::fs::write(&secret, "TOPSECRET-ARC-WORK").unwrap();
    let secret_path = secret.to_str().unwrap().to_owned();
    let mock = Mock::start(vec![
        read_reply("c1", &secret_path),
        write_reply("c2", "worker.txt", "ok"),
        shell_reply("c3", "git add worker.txt && git commit -m worker"),
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
        "the read was refused: {bodies}"
    );
    let spec = format!("work/{task}:worker.txt");
    assert_eq!(
        repo.git(&["show", spec.as_str()]).trim(),
        "ok",
        "the worker wrote and committed inside its worktree"
    );
    assert_eq!(run.finished()["pushed"], json!(true));
}

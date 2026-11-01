mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::TestRepo;
use radian_arc::gate::{
    CommandRun, ExclusiveLock, GateError, Stage, Verdict, VerdictResult, verdict_path,
};
use serde_json::{Value, json};

#[test]
fn a_passing_profile_runs_every_command_in_order() {
    let repo = TestRepo::new("commands-pass");
    let base = repo.rev("main");
    let head = repo.commit_on("work/pass", &base, "pass.txt", "pass\n");
    let commands = vec![
        "true".to_string(),
        "echo hello > made.txt".to_string(),
        "test -f made.txt".to_string(),
    ];
    let run = CommandRun {
        repository: &repo.repo,
        remote: "origin",
        main_ref: "refs/remotes/origin/main",
        task: "TK0001-pass",
        attempt: "TK0001-pass-1",
        sha: &head,
        commands: &commands,
        profile: "default",
        previous_sha: None,
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass);
    assert_eq!(verdict.stage, Stage::Commands);
    assert_eq!(verdict.sha, head);
    assert!(verdict.reason.is_none());
    assert_eq!(verdict.commands.len(), 3);
    assert_eq!(verdict.commands[0].line, "true");
    assert_eq!(verdict.commands[0].exit_code, Some(0));
    assert!(verdict.started_at <= verdict.ended_at);

    let path = verdict_path(&repo.repo, "TK0001-pass", "TK0001-pass-1", Stage::Commands).unwrap();
    assert_eq!(Verdict::read(&path).unwrap(), verdict);
    let checkout = repo.repo.join(".radian/arc/gates/commands");
    assert!(
        checkout.join("made.txt").exists(),
        "commands ran outside the gate checkout"
    );
}

#[test]
fn a_failing_command_stops_the_stage() {
    let repo = TestRepo::new("commands-fail");
    let base = repo.rev("main");
    let head = repo.commit_on("work/fail", &base, "fail.txt", "fail\n");
    let commands = vec!["exit 3".to_string(), "touch should-not-run".to_string()];
    let run = CommandRun {
        repository: &repo.repo,
        remote: "origin",
        main_ref: "refs/remotes/origin/main",
        task: "TK0001-fail",
        attempt: "TK0001-fail-1",
        sha: &head,
        commands: &commands,
        profile: "default",
        previous_sha: None,
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Fail);
    assert_eq!(verdict.reason.as_deref(), Some("command_failed"));
    assert_eq!(
        verdict.commands.len(),
        1,
        "the stage ran past the first failure"
    );
    assert_eq!(verdict.commands[0].exit_code, Some(3));
    let checkout = repo.repo.join(".radian/arc/gates/commands");
    assert!(!checkout.join("should-not-run").exists());
}

#[test]
fn a_timeout_kills_the_sleep_and_records_it() {
    let repo = TestRepo::new("commands-timeout");
    let base = repo.rev("main");
    let head = repo.commit_on("work/timeout", &base, "timeout.txt", "timeout\n");
    let commands = vec!["sleep 5; touch after-timeout".to_string()];
    let run = CommandRun {
        repository: &repo.repo,
        remote: "origin",
        main_ref: "refs/remotes/origin/main",
        task: "TK0001-timeout",
        attempt: "TK0001-timeout-1",
        sha: &head,
        commands: &commands,
        profile: "default",
        previous_sha: None,
        timeout: Duration::from_secs(1),
    };

    let started = Instant::now();
    let verdict = run.run().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the timeout did not stop the command"
    );
    assert_eq!(verdict.result, VerdictResult::Fail);
    assert_eq!(verdict.reason.as_deref(), Some("timeout"));
    assert_eq!(verdict.commands.len(), 1);
    assert_eq!(verdict.commands[0].exit_code, None);
    assert!(verdict.commands[0].duration_ms >= 900);
    let checkout = repo.repo.join(".radian/arc/gates/commands");
    assert!(
        !checkout.join("after-timeout").exists(),
        "the killed command kept running"
    );
}

#[test]
fn two_exclusive_stages_in_two_threads_never_overlap() {
    let repo = TestRepo::new("commands-exclusive");
    let base = repo.rev("main");
    let head = repo.commit_on("work/exclusive", &base, "exclusive.txt", "exclusive\n");
    let order = repo.repo.join("exclusive-order.log");
    let line = format!(
        "echo start >> {path}; sleep 0.3; echo end >> {path}",
        path = order.display()
    );

    let mut handles = Vec::new();
    for (task, attempt) in [("TK0001-a", "TK0001-a-1"), ("TK0002-b", "TK0002-b-1")] {
        let repository = repo.repo.clone();
        let sha = head.clone();
        let commands = vec![line.clone()];
        handles.push(std::thread::spawn(move || {
            CommandRun {
                repository: &repository,
                remote: "origin",
                main_ref: "refs/remotes/origin/main",
                task,
                attempt,
                sha: &sha,
                commands: &commands,
                profile: "default",
                previous_sha: None,
                timeout: Duration::from_secs(30),
            }
            .run()
            .unwrap()
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let log = fs::read_to_string(&order).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec!["start", "end", "start", "end"],
        "two exclusive stages overlapped"
    );
}

#[test]
fn a_commands_stage_waits_for_the_exclusive_lock() {
    let repo = TestRepo::new("commands-waits");
    let base = repo.rev("main");
    let head = repo.commit_on("work/waits", &base, "waits.txt", "waits\n");
    let marker = repo.repo.join("stage-ran");
    let line = format!("touch {}", marker.display());

    let lock = ExclusiveLock::acquire(&repo.repo).unwrap();
    let (repository, sha, commands) = (repo.repo.clone(), head.clone(), vec![line]);
    let handle = std::thread::spawn(move || {
        CommandRun {
            repository: &repository,
            remote: "origin",
            main_ref: "refs/remotes/origin/main",
            task: "TK0001-waits",
            attempt: "TK0001-waits-1",
            sha: &sha,
            commands: &commands,
            profile: "default",
            previous_sha: None,
            timeout: Duration::from_secs(30),
        }
        .run()
        .unwrap()
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "a commands stage ran while another exclusive stage held the lock"
    );
    drop(lock);
    let verdict = handle.join().unwrap();
    assert!(verdict.is_pass());
    assert!(marker.exists());
}

#[test]
fn a_timeout_kills_the_whole_command_group() {
    let repo = TestRepo::new("commands-group");
    let base = repo.rev("main");
    let head = repo.commit_on("work/group", &base, "group.txt", "group\n");
    let commands = vec!["(sleep 2; touch after-group-timeout) & wait".to_string()];
    let run = CommandRun {
        repository: &repo.repo,
        remote: "origin",
        main_ref: "refs/remotes/origin/main",
        task: "TK0001-group",
        attempt: "TK0001-group-1",
        sha: &head,
        commands: &commands,
        profile: "default",
        previous_sha: None,
        timeout: Duration::from_secs(1),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.reason.as_deref(), Some("timeout"));
    std::thread::sleep(Duration::from_millis(2200));
    let checkout = repo.repo.join(".radian/arc/gates/commands");
    assert!(
        !checkout.join("after-group-timeout").exists(),
        "a grandchild of the timed-out command survived the kill"
    );
}

#[test]
fn a_verdict_cannot_be_written_twice() {
    let repo = TestRepo::new("commands-twice");
    let base = repo.rev("main");
    let head = repo.commit_on("work/twice", &base, "twice.txt", "twice\n");
    let commands = vec!["true".to_string()];
    let run = CommandRun {
        repository: &repo.repo,
        remote: "origin",
        main_ref: "refs/remotes/origin/main",
        task: "TK0001-twice",
        attempt: "TK0001-twice-1",
        sha: &head,
        commands: &commands,
        profile: "default",
        previous_sha: None,
        timeout: Duration::from_secs(30),
    };

    let first = run.run().unwrap();
    assert!(first.is_pass());
    let error = run.run().unwrap_err();
    assert!(
        matches!(error, GateError::VerdictExists(_)),
        "a second write for the same verdict path did not fail: {error}"
    );
}

/// The `diffstat.json` of one attempt.
fn diffstat(repo: &TestRepo, task: &str, attempt: &str) -> Option<Value> {
    let path = repo
        .repo
        .join(".radian/arc/runs")
        .join(task)
        .join("attempts")
        .join(attempt)
        .join("diffstat.json");
    read_json(&path)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Runs the `commands` stage for one attempt in `repo`.
fn run_commands(
    repo: &TestRepo,
    task: &str,
    attempt: &str,
    head: &str,
    previous_sha: Option<String>,
) {
    let commands = vec!["true".to_string()];
    CommandRun {
        repository: &repo.repo,
        remote: "origin",
        main_ref: "refs/remotes/origin/main",
        task,
        attempt,
        sha: head,
        commands: &commands,
        profile: "default",
        previous_sha,
        timeout: Duration::from_secs(30),
    }
    .run()
    .expect("the commands stage runs");
}

/// A branch with a renamed text file and a binary file: the diff counts two files, and the
/// binary adds and removes no line. Mutation: count `-` as a line or drop a rename record, and
/// this test's counts change.
#[test]
fn the_diffstat_counts_a_binary_file_and_a_rename() {
    let repo = TestRepo::new("diffstat-binary");
    let base = repo.rev("main");
    repo.git(&["checkout", "-B", "work/bin", &base]);
    repo.git(&["mv", "file.txt", "renamed.txt"]);
    fs::write(repo.repo.join("blob.bin"), [0u8, 1, 2]).expect("the binary file");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "rename and binary"]);
    let head = repo.rev("HEAD");

    run_commands(&repo, "TK0001-bin", "TK0001-bin-1", &head, None);
    let stat = diffstat(&repo, "TK0001-bin", "TK0001-bin-1").expect("diffstat.json");
    assert_eq!(stat["diff"]["files"], json!(2), "{stat}");
    assert_eq!(
        stat["diff"]["added"],
        json!(0),
        "a pure rename and a binary add no line: {stat}"
    );
    assert_eq!(stat["diff"]["removed"], json!(0), "{stat}");
    assert!(
        stat.get("increment").is_none(),
        "a first attempt has no increment: {stat}"
    );
}

/// A second attempt's increment is the rework's own change: the file the first attempt added
/// is in `diff`, not in `increment`. A first attempt has no increment at all. Mutation: use
/// the whole-task range for the increment, and it holds both files.
#[test]
fn the_increment_covers_only_the_rework_change() {
    let repo = TestRepo::new("diffstat-increment");
    let base = repo.rev("main");
    let head1 = repo.commit_on("work/inc", &base, "one.txt", "one\n");
    let head2 = repo.commit_on("work/inc", &head1, "two.txt", "two\n");

    run_commands(&repo, "TK0001-inc", "TK0001-inc-1", &head1, None);
    let first = diffstat(&repo, "TK0001-inc", "TK0001-inc-1").expect("first diffstat.json");
    assert_eq!(first["diff"]["files"], json!(1), "{first}");
    assert!(
        first.get("increment").is_none(),
        "the first attempt has no increment: {first}"
    );

    run_commands(&repo, "TK0001-inc", "TK0001-inc-2", &head2, Some(head1));
    let second = diffstat(&repo, "TK0001-inc", "TK0001-inc-2").expect("second diffstat.json");
    assert_eq!(second["diff"]["files"], json!(2), "{second}");
    assert_eq!(second["diff"]["added"], json!(2), "{second}");
    assert_eq!(
        second["increment"]["files"],
        json!(1),
        "only the rework's file is in the increment: {second}"
    );
    assert_eq!(
        second["increment"]["added"],
        json!(1),
        "only the rework's line is in the increment: {second}"
    );
}

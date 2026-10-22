mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::TestRepo;
use radian_arc::gate::{CommandRun, GateError, Stage, Verdict, VerdictResult, verdict_path};

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
        task: "0001-pass",
        attempt: "0001-pass-1",
        sha: &head,
        commands: &commands,
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

    let path = verdict_path(&repo.repo, "0001-pass", "0001-pass-1", Stage::Commands).unwrap();
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
        task: "0001-fail",
        attempt: "0001-fail-1",
        sha: &head,
        commands: &commands,
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
        task: "0001-timeout",
        attempt: "0001-timeout-1",
        sha: &head,
        commands: &commands,
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
    for (task, attempt) in [("0001-a", "0001-a-1"), ("0002-b", "0002-b-1")] {
        let repository = repo.repo.clone();
        let sha = head.clone();
        let commands = vec![line.clone()];
        handles.push(std::thread::spawn(move || {
            CommandRun {
                repository: &repository,
                task,
                attempt,
                sha: &sha,
                commands: &commands,
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
fn a_verdict_cannot_be_written_twice() {
    let repo = TestRepo::new("commands-twice");
    let base = repo.rev("main");
    let head = repo.commit_on("work/twice", &base, "twice.txt", "twice\n");
    let commands = vec!["true".to_string()];
    let run = CommandRun {
        repository: &repo.repo,
        task: "0001-twice",
        attempt: "0001-twice-1",
        sha: &head,
        commands: &commands,
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

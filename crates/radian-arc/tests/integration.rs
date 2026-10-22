mod common;

use std::fs;
use std::time::Duration;

use common::TestRepo;
use radian_arc::gate::{IntegrationRun, Stage, Verdict, VerdictResult, merge_ready};

fn setup(label: &str) -> (TestRepo, String, String) {
    let repo = TestRepo::new(label);
    let base = repo.rev("main");
    let head = repo.commit_on("work/0001-feature", &base, "feature.txt", "feature\n");
    repo.push("work/0001-feature");
    (repo, base, head)
}

#[test]
fn integration_with_main_unchanged_gives_candidate_head() {
    let (repo, base, head) = setup("integration-unchanged");
    let key_commands = vec![
        "test -f feature.txt".to_string(),
        "echo keyed > keyed.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "0001-feature",
        attempt: "0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass);
    assert_eq!(verdict.stage, Stage::Integration);
    assert_eq!(verdict.sha, head);
    assert_eq!(verdict.base_sha.as_deref(), Some(base.as_str()));
    assert_eq!(verdict.candidate_sha.as_deref(), Some(head.as_str()));
    assert_eq!(verdict.commands.len(), 2);
    assert!(verdict.reason.is_none());
    assert!(
        repo.repo
            .join(".radian/arc/gates/integration/keyed.txt")
            .exists(),
        "key commands ran outside the integration checkout"
    );
    assert!(
        repo.repo
            .join(".radian/arc/runs/0001-feature/attempts/0001-feature-1/verdicts/integration.json")
            .exists()
    );
}

#[test]
fn integration_with_main_moved_merges_and_runs_key_commands_on_the_merge() {
    let (repo, base, head) = setup("integration-moved");
    let moved = repo.advance_main("other.txt", "other\n");
    assert_ne!(moved, base);
    let key_commands = vec![
        "test -f feature.txt".to_string(),
        "test -f other.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "0001-feature",
        attempt: "0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    assert_eq!(verdict.base_sha.as_deref(), Some(moved.as_str()));
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    assert_ne!(candidate, head, "integration used head instead of a merge");
    assert_ne!(candidate, moved);

    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &candidate]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "candidate is not a merge commit: {parents}");
    assert!(
        parts.contains(&moved.as_str()),
        "main is not a parent: {parents}"
    );
    assert!(
        parts.contains(&head.as_str()),
        "head is not a parent: {parents}"
    );

    let subject = repo.git(&["log", "-1", "--format=%s", &candidate]);
    assert_eq!(subject, format!("Merge work/0001-feature ({})", &head[..7]));
    let identity = repo.git(&["log", "-1", "--format=%cn <%ce>", &candidate]);
    assert_eq!(identity, "arc <arc@localhost>");

    assert_eq!(verdict.commands.len(), 2);
    let checkout = repo.repo.join(".radian/arc/gates/integration");
    assert!(checkout.join("feature.txt").exists());
    assert!(checkout.join("other.txt").exists());
    assert_eq!(
        repo.rev("HEAD"),
        moved,
        "the main checkout must stay where the test put it"
    );
}

#[test]
fn a_conflict_gives_a_fail_verdict_with_the_path() {
    let repo = TestRepo::new("integration-conflict");
    let base = repo.rev("main");
    let head = repo.commit_on("work/0001-conflict", &base, "file.txt", "work side\n");
    repo.push("work/0001-conflict");
    let moved = repo.advance_main("file.txt", "main side\n");
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "0001-conflict",
        attempt: "0001-conflict-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &[],
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Fail);
    assert_eq!(verdict.reason.as_deref(), Some("conflict"));
    assert_eq!(verdict.conflict_paths, vec!["file.txt".to_string()]);
    assert!(verdict.candidate_sha.is_none());
    assert_eq!(verdict.base_sha.as_deref(), Some(moved.as_str()));
    assert!(
        verdict.commands.is_empty(),
        "key commands ran on a conflicted tree"
    );

    let checkout = repo.repo.join(".radian/arc/gates/integration");
    let checkout_head = repo.git(&["-C", common::path(&checkout), "rev-parse", "HEAD"]);
    assert_eq!(checkout_head, moved, "the conflicted merge was not aborted");
}

#[test]
fn merge_ready_is_false_after_main_moves_again() {
    let (repo, base, head) = setup("integration-ready");
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "0001-feature",
        attempt: "0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &["true".to_string()],
        timeout: Duration::from_secs(30),
    };
    let verdict = run.run().unwrap();
    assert!(verdict.is_pass());

    let main_ref = "refs/remotes/origin/main";
    assert!(merge_ready(&repo.repo, "origin", main_ref, &verdict).unwrap());
    repo.advance_main("later.txt", "later\n");
    assert!(
        !merge_ready(&repo.repo, "origin", main_ref, &verdict).unwrap(),
        "merge_ready stayed true after main moved"
    );

    let human = Verdict::human(&head, true, None);
    assert!(!merge_ready(&repo.repo, "origin", main_ref, &human).unwrap());
}

#[test]
fn integration_uses_the_named_remote() {
    let repo = TestRepo::new("integration-remote");
    let base = repo.rev("main");
    let head = repo.commit_on("work/0001-remote", &base, "remote.txt", "remote\n");
    repo.push("work/0001-remote");
    // The repository's remote is `upstream`; a leftover `origin` points nowhere.
    repo.rename_origin_to("upstream");
    let moved = repo.advance_main_on("upstream", "other.txt", "other\n");

    let main_ref = "refs/remotes/upstream/main";
    let key_commands = vec![
        "test -f remote.txt".to_string(),
        "test -f other.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "upstream",
        task: "0001-remote",
        attempt: "0001-remote-1",
        head_sha: &head,
        base_sha: &base,
        main_ref,
        key_commands: &key_commands,
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    assert_eq!(verdict.base_sha.as_deref(), Some(moved.as_str()));
    assert!(
        verdict.candidate_sha.as_deref() != Some(head.as_str()),
        "integration used head instead of a merge"
    );

    assert!(merge_ready(&repo.repo, "upstream", main_ref, &verdict).unwrap());
    repo.advance_main_on("upstream", "later.txt", "later\n");
    assert!(
        !merge_ready(&repo.repo, "upstream", main_ref, &verdict).unwrap(),
        "merge_ready stayed true after main moved"
    );
}

#[test]
fn two_integration_runs_never_overlap() {
    let (repo, base, head) = setup("integration-lock");
    let order = repo.repo.join("integration-order.log");
    let line = format!(
        "echo start >> {path}; sleep 0.3; echo end >> {path}",
        path = order.display()
    );

    let mut handles = Vec::new();
    for (task, attempt) in [("0001-a", "0001-a-1"), ("0002-b", "0002-b-1")] {
        let repository = repo.repo.clone();
        let sha = head.clone();
        let base_sha = base.clone();
        let key_commands = vec![line.clone()];
        handles.push(std::thread::spawn(move || {
            IntegrationRun {
                repository: &repository,
                remote: "origin",
                task,
                attempt,
                head_sha: &sha,
                base_sha: &base_sha,
                main_ref: "refs/remotes/origin/main",
                key_commands: &key_commands,
                timeout: Duration::from_secs(30),
            }
            .run()
            .unwrap()
        }));
    }
    for handle in handles {
        let verdict = handle.join().unwrap();
        assert!(verdict.is_pass(), "{:?}", verdict.reason);
    }

    let log = fs::read_to_string(&order).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec!["start", "end", "start", "end"],
        "two integration runs overlapped"
    );
}

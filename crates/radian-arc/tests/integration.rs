mod common;

use std::fs;
use std::time::Duration;

use common::TestRepo;
use radian_arc::gate::{IntegrationRun, Stage, Verdict, VerdictResult, merge_ready};
use radian_arc::store::config::IntegrationStyle;

fn setup(label: &str) -> (TestRepo, String, String) {
    let repo = TestRepo::new(label);
    let base = repo.rev("main");
    let head = repo.commit_on("work/TK0001-feature", &base, "feature.txt", "feature\n");
    repo.push("work/TK0001-feature");
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
        task: "TK0001-feature",
        title: "A task",
        attempt: "TK0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        style: IntegrationStyle::Merge,
        profile: "default",
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
            .join(".radian/arc/runs/TK0001-feature/attempts/TK0001-feature-1/verdicts/integration.json")
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
        task: "TK0001-feature",
        title: "A task",
        attempt: "TK0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        style: IntegrationStyle::Merge,
        profile: "default",
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
    assert_eq!(
        subject,
        format!("Merge work/TK0001-feature ({})", &head[..7])
    );
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
    let head = repo.commit_on("work/TK0001-conflict", &base, "file.txt", "work side\n");
    repo.push("work/TK0001-conflict");
    let moved = repo.advance_main("file.txt", "main side\n");
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "TK0001-conflict",
        title: "A task",
        attempt: "TK0001-conflict-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &[],
        style: IntegrationStyle::Merge,
        profile: "default",
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
        task: "TK0001-feature",
        title: "A task",
        attempt: "TK0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &["true".to_string()],
        style: IntegrationStyle::Merge,
        profile: "default",
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
    let head = repo.commit_on("work/TK0001-remote", &base, "remote.txt", "remote\n");
    repo.push("work/TK0001-remote");
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
        task: "TK0001-remote",
        title: "A task",
        attempt: "TK0001-remote-1",
        head_sha: &head,
        base_sha: &base,
        main_ref,
        key_commands: &key_commands,
        style: IntegrationStyle::Merge,
        profile: "default",
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
    for (task, attempt) in [("TK0001-a", "TK0001-a-1"), ("TK0002-b", "TK0002-b-1")] {
        let repository = repo.repo.clone();
        let sha = head.clone();
        let base_sha = base.clone();
        let key_commands = vec![line.clone()];
        handles.push(std::thread::spawn(move || {
            IntegrationRun {
                repository: &repository,
                remote: "origin",
                task,
                title: "A task",
                attempt,
                head_sha: &sha,
                base_sha: &base_sha,
                main_ref: "refs/remotes/origin/main",
                key_commands: &key_commands,
                style: IntegrationStyle::Merge,
                profile: "default",
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

/// The tree a `--no-ff` merge of `left` and `right` has, built in a second clone so the
/// integration checkout and the test's main checkout stay untouched.
fn merge_tree(repo: &TestRepo, label: &str, left: &str, right: &str) -> String {
    let clone = repo.second_clone(label);
    repo.git_in(&clone, &["config", "user.name", "Test"]);
    repo.git_in(&clone, &["config", "user.email", "test@example.com"]);
    repo.git_in(&clone, &["checkout", "--detach", left]);
    repo.git_in(&clone, &["merge", "--no-ff", "--no-edit", right]);
    repo.git_in(&clone, &["rev-parse", "HEAD^{tree}"])
}

/// Edge 1: `squash` with main unchanged makes one new commit whose parent is the old main and
/// whose tree is the task head's tree. Mutation: return `head` for `squash` and the candidate
/// is the head (no new commit).
#[test]
fn squash_with_main_unchanged_makes_one_commit_with_the_head_tree() {
    let (repo, base, head) = setup("integration-squash-unchanged");
    let key_commands = vec![
        "test -f feature.txt".to_string(),
        "echo keyed > keyed.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "TK0001-feature",
        title: "A task",
        attempt: "TK0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        style: IntegrationStyle::Squash,
        profile: "default",
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    assert_ne!(candidate, head, "squash kept the head's sha");
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &candidate]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 2, "not one commit on the old main: {parents}");
    assert_eq!(parts[1], base, "the parent is not the old main: {parents}");
    let tree = repo.git(&["rev-parse", &format!("{candidate}^{{tree}}")]);
    let head_tree = repo.git(&["rev-parse", &format!("{head}^{{tree}}")]);
    assert_eq!(tree, head_tree, "the squashed tree is not the head's");
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s", &candidate]),
        "A task (TK0001-feature)"
    );
    assert_eq!(
        repo.git(&["log", "-1", "--format=%cn <%ce>", &candidate]),
        "arc <arc@localhost>"
    );
    assert_eq!(verdict.commands.len(), 2);
    assert!(
        repo.repo
            .join(".radian/arc/gates/integration/keyed.txt")
            .exists(),
        "key commands did not run on the squashed candidate"
    );
}

/// Edge 2: `squash` with main moved makes one commit on the new main, with the tree a merge of
/// the two would have, and the key commands run on it.
#[test]
fn squash_with_main_moved_makes_one_commit_on_the_new_main() {
    let (repo, base, head) = setup("integration-squash-moved");
    let moved = repo.advance_main("other.txt", "other\n");
    assert_ne!(moved, base);
    let key_commands = vec![
        "test -f feature.txt".to_string(),
        "test -f other.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "TK0001-feature",
        title: "A task",
        attempt: "TK0001-feature-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        style: IntegrationStyle::Squash,
        profile: "default",
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    assert_eq!(verdict.base_sha.as_deref(), Some(moved.as_str()));
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &candidate]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 2, "not one commit on the new main: {parents}");
    assert_eq!(parts[1], moved, "the parent is not the new main: {parents}");
    let tree = repo.git(&["rev-parse", &format!("{candidate}^{{tree}}")]);
    let expected = merge_tree(&repo, "squash-moved-tree", &moved, &head);
    assert_eq!(tree, expected, "the squashed tree is not the merge's");
    assert_eq!(verdict.commands.len(), 2);
}

/// Edge 3: `rebase` of a 3-commit branch makes 3 new commits on main, with the tree a merge
/// of the two would have. Mutation: fall back to `head` for `rebase` and the branch is not
/// replayed (the candidate contains none of main's new commit).
#[test]
fn rebase_replays_three_commits_onto_main() {
    let repo = TestRepo::new("integration-rebase-three");
    let base = repo.rev("main");
    let branch = "work/TK0001-rebase";
    let first = repo.commit_on(branch, &base, "one.txt", "one\n");
    let second = repo.commit_on(branch, &first, "two.txt", "two\n");
    let head = repo.commit_on(branch, &second, "three.txt", "three\n");
    repo.push(branch);
    let moved = repo.advance_main("other.txt", "other\n");
    let key_commands = vec![
        "test -f one.txt".to_string(),
        "test -f two.txt".to_string(),
        "test -f three.txt".to_string(),
        "test -f other.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "TK0001-rebase",
        title: "A task",
        attempt: "TK0001-rebase-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        style: IntegrationStyle::Rebase,
        profile: "default",
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    assert_ne!(candidate, head);
    assert_eq!(
        repo.git(&["rev-list", "--count", &format!("{moved}..{candidate}")]),
        "3",
        "the branch was not replayed commit by commit"
    );
    assert!(
        repo.git(&["rev-list", "--merges", &format!("{moved}..{candidate}")])
            .is_empty(),
        "the rebased branch contains a merge commit"
    );
    assert_eq!(
        repo.git(&["rev-parse", &format!("{candidate}^^^")]),
        moved,
        "the replayed branch does not sit on the new main"
    );
    let tree = repo.git(&["rev-parse", &format!("{candidate}^{{tree}}")]);
    let expected = merge_tree(&repo, "rebase-three-tree", &moved, &head);
    assert_eq!(tree, expected, "the replayed tree is not the merge's");
}

/// Edge 4: a conflict in the second rebased commit aborts the rebase, leaves the checkout
/// clean and on the task head, and reports `conflict` with the path.
#[test]
fn a_rebase_conflict_is_aborted_and_leaves_the_checkout_clean() {
    let repo = TestRepo::new("integration-rebase-conflict");
    let base = repo.rev("main");
    let branch = "work/TK0001-conflict";
    let first = repo.commit_on(branch, &base, "first.txt", "first\n");
    let head = repo.commit_on(branch, &first, "file.txt", "work side\n");
    repo.push(branch);
    let moved = repo.advance_main("file.txt", "main side\n");
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "TK0001-conflict",
        title: "A task",
        attempt: "TK0001-conflict-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &[],
        style: IntegrationStyle::Rebase,
        profile: "default",
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
        "key commands ran on a conflict"
    );

    let checkout = repo.repo.join(".radian/arc/gates/integration");
    let checkout_head = repo.git(&["-C", common::path(&checkout), "rev-parse", "HEAD"]);
    assert_eq!(checkout_head, head, "the conflicted rebase was not aborted");
    let status = repo.git(&["-C", common::path(&checkout), "status", "--porcelain"]);
    assert!(status.is_empty(), "the checkout is not clean: {status}");
}

/// Edge 6: a branch with a merge commit inside it is linearized by `rebase`: git drops the
/// merge commit and replays the commits it brought in, so no change is lost and the tree is
/// the merge's. Mutation: pass `--rebase-merges` and the branch keeps its merge commit.
#[test]
fn rebase_of_a_branch_with_a_merge_commit_is_linearized() {
    let repo = TestRepo::new("integration-rebase-merge");
    let base = repo.rev("main");
    let branch = "work/TK0001-merge";
    repo.commit_on(branch, &base, "a.txt", "a\n");
    let side = repo.commit_on("side", &base, "side.txt", "side\n");
    repo.git(&["checkout", branch]);
    repo.git(&["merge", "--no-ff", "--no-edit", &side]);
    let head = repo.rev("HEAD");
    repo.push(branch);
    let moved = repo.advance_main("other.txt", "other\n");
    let key_commands = vec![
        "test -f a.txt".to_string(),
        "test -f side.txt".to_string(),
        "test -f other.txt".to_string(),
    ];
    let run = IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task: "TK0001-merge",
        title: "A task",
        attempt: "TK0001-merge-1",
        head_sha: &head,
        base_sha: &base,
        main_ref: "refs/remotes/origin/main",
        key_commands: &key_commands,
        style: IntegrationStyle::Rebase,
        profile: "default",
        timeout: Duration::from_secs(30),
    };

    let verdict = run.run().unwrap();
    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    assert!(
        repo.git(&["rev-list", "--merges", &format!("{moved}..{candidate}")])
            .is_empty(),
        "the merge commit was not dropped"
    );
    assert_eq!(
        repo.git(&["rev-list", "--count", &format!("{moved}..{candidate}")]),
        "2",
        "the two sides' commits were not replayed"
    );
    let tree = repo.git(&["rev-parse", &format!("{candidate}^{{tree}}")]);
    let expected = merge_tree(&repo, "rebase-merge-tree", &moved, &head);
    assert_eq!(tree, expected, "a change was lost in the linearization");
}

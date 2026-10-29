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

/// Edge 1: `rebase` with the target unchanged and a merge commit in the branch is refused
/// before the fast path, which would push the branch unchanged. The reason names the merge
/// commit and asks for a linear branch. Mutation: drop the refusal check and the fast path
/// returns `head`.
#[test]
fn rebase_refuses_a_branch_with_a_merge_commit_when_the_target_is_unchanged() {
    let repo = TestRepo::new("integration-rebase-merge-unchanged");
    let base = repo.rev("main");
    let branch = "work/TK0001-merge";
    repo.commit_on(branch, &base, "a.txt", "a\n");
    let side = repo.commit_on("side", &base, "side.txt", "side\n");
    repo.git(&["checkout", branch]);
    repo.git(&["merge", "--no-ff", "--no-edit", &side]);
    let head = repo.rev("HEAD");
    repo.push(branch);
    let key_commands = vec!["echo ran > ran.txt".to_string()];

    let verdict = rebase(
        &repo,
        "TK0001-merge",
        "TK0001-merge-1",
        &base,
        &head,
        &key_commands,
    );

    assert_eq!(verdict.result, VerdictResult::Fail);
    let reason = verdict.reason.clone().expect("a reason");
    assert!(reason.contains(&head[..7]), "{reason}");
    assert!(reason.contains("linear"), "{reason}");
    assert!(reason.contains("`merge` or `squash`"), "{reason}");
    assert!(
        verdict.candidate_sha.is_none(),
        "a refused rebase built a candidate"
    );
    assert!(
        verdict.commands.is_empty(),
        "key commands ran on a refused branch"
    );
    assert_eq!(repo.ls_remote("refs/heads/main"), Some(base));
    assert!(
        !repo
            .repo
            .join(".radian/arc/gates/integration/ran.txt")
            .exists(),
        "the refused branch reached the integration checkout"
    );
}

/// Edge 2: the target moved and the branch contains a merge whose resolution added a file.
/// `git rebase` would drop the merge and lose that file; the refusal keeps every change and
/// nothing is pushed. Mutation: drop the refusal check and the rebase loses the file.
#[test]
fn rebase_refuses_a_branch_whose_merge_resolution_added_a_file() {
    let repo = TestRepo::new("integration-rebase-merge-resolution");
    let base = repo.rev("main");
    let branch = "work/TK0001-resolution";
    repo.commit_on(branch, &base, "a.txt", "a\n");
    let side = repo.commit_on("side", &base, "side.txt", "side\n");
    repo.git(&["checkout", branch]);
    repo.git(&["merge", "--no-ff", "--no-commit", &side]);
    common::write(
        &repo.repo.join("resolution.txt"),
        "only in the merge resolution\n",
    );
    repo.git(&["add", "resolution.txt"]);
    repo.git(&["commit", "-m", "merge side"]);
    let head = repo.rev("HEAD");
    repo.push(branch);
    let moved = repo.advance_main("other.txt", "other\n");

    let verdict = rebase(
        &repo,
        "TK0001-resolution",
        "TK0001-resolution-1",
        &base,
        &head,
        &[],
    );

    assert_eq!(verdict.result, VerdictResult::Fail);
    let reason = verdict.reason.clone().expect("a reason");
    assert!(reason.contains(&head[..7]), "{reason}");
    assert!(
        verdict.candidate_sha.is_none(),
        "a refused rebase built a candidate"
    );
    assert_eq!(
        repo.ls_remote("refs/heads/main"),
        Some(moved),
        "the refused rebase moved the target"
    );
    // No silent loss: the merge and its resolution are still on the task branch.
    assert_eq!(
        repo.git(&["show", &format!("{head}:resolution.txt")]),
        "only in the merge resolution"
    );
    assert!(
        repo.git(&["rev-list", "--merges", &format!("{base}..{head}")])
            .contains(&head),
        "the branch lost its merge commit"
    );
}

/// Edge 3: the user's git configuration cannot change a rebase. With
/// `rebase.rebaseMerges`, `rebase.autoSquash` and `rebase.updateRefs` set in the repository,
/// a linear branch is replayed exactly as without them, and the task branch stays where it
/// was. Mutation: run plain `git rebase` and `rebase.updateRefs` moves the task branch.
#[test]
fn rebase_ignores_the_users_rebase_configuration() {
    let repo = TestRepo::new("integration-rebase-config");
    let base = repo.rev("main");
    let branch = "work/TK0001-config";
    let head = repo.commit_on(branch, &base, "one.txt", "one\n");
    repo.push(branch);
    let moved = repo.advance_main("other.txt", "other\n");

    let plain = rebase(
        &repo,
        "TK0001-config",
        "TK0001-config-1",
        &base,
        &head,
        &["test -f other.txt".to_string()],
    );
    assert_eq!(plain.result, VerdictResult::Pass, "{:?}", plain.reason);
    let plain_candidate = plain.candidate_sha.clone().expect("a candidate");

    repo.git(&["config", "rebase.rebaseMerges", "true"]);
    repo.git(&["config", "rebase.autoSquash", "true"]);
    repo.git(&["config", "rebase.updateRefs", "true"]);
    let configured = rebase(
        &repo,
        "TK0001-config",
        "TK0001-config-2",
        &base,
        &head,
        &["test -f other.txt".to_string()],
    );

    assert_eq!(
        configured.result,
        VerdictResult::Pass,
        "{:?}",
        configured.reason
    );
    let candidate = configured.candidate_sha.clone().expect("a candidate");
    assert_eq!(
        repo.git(&["rev-list", "--count", &format!("{moved}..{candidate}")]),
        repo.git(&[
            "rev-list",
            "--count",
            &format!("{moved}..{plain_candidate}")
        ]),
        "ambient config changed how many commits were replayed"
    );
    assert_eq!(
        repo.git(&["rev-parse", &format!("{candidate}^{{tree}}")]),
        repo.git(&["rev-parse", &format!("{plain_candidate}^{{tree}}")]),
        "ambient config changed the replayed tree"
    );
    assert!(
        repo.git(&["rev-list", "--merges", &format!("{moved}..{candidate}")])
            .is_empty(),
        "ambient rebase.rebaseMerges kept a merge commit"
    );
    assert_eq!(
        repo.git(&["rev-parse", branch]),
        head,
        "ambient rebase.updateRefs moved the task branch"
    );
}

/// Edge 5: `merge` is not refused for a branch that contains a merge commit; it still makes
/// one merge commit of the branch onto the moved target.
#[test]
fn merge_of_a_branch_with_a_merge_commit_is_not_refused() {
    let repo = TestRepo::new("integration-merge-merge-commit");
    let base = repo.rev("main");
    let branch = "work/TK0001-merge";
    repo.commit_on(branch, &base, "a.txt", "a\n");
    let side = repo.commit_on("side", &base, "side.txt", "side\n");
    repo.git(&["checkout", branch]);
    repo.git(&["merge", "--no-ff", "--no-edit", &side]);
    let head = repo.rev("HEAD");
    repo.push(branch);
    let moved = repo.advance_main("other.txt", "other\n");

    let verdict = style(
        &repo,
        "TK0001-merge",
        "TK0001-merge-1",
        &base,
        &head,
        &["test -f side.txt".to_string()],
        IntegrationStyle::Merge,
    );

    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &candidate]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "not a merge commit: {parents}");
    assert!(parts.contains(&moved.as_str()), "{parents}");
    assert!(parts.contains(&head.as_str()), "{parents}");
}

/// Edge 5: `squash` is not refused for a branch that contains a merge commit; it still makes
/// one commit on the moved target with the merge's tree.
#[test]
fn squash_of_a_branch_with_a_merge_commit_is_not_refused() {
    let repo = TestRepo::new("integration-squash-merge-commit");
    let base = repo.rev("main");
    let branch = "work/TK0001-merge";
    repo.commit_on(branch, &base, "a.txt", "a\n");
    let side = repo.commit_on("side", &base, "side.txt", "side\n");
    repo.git(&["checkout", branch]);
    repo.git(&["merge", "--no-ff", "--no-edit", &side]);
    let head = repo.rev("HEAD");
    repo.push(branch);
    let moved = repo.advance_main("other.txt", "other\n");

    let verdict = style(
        &repo,
        "TK0001-merge",
        "TK0001-merge-1",
        &base,
        &head,
        &["test -f side.txt".to_string()],
        IntegrationStyle::Squash,
    );

    assert_eq!(verdict.result, VerdictResult::Pass, "{:?}", verdict.reason);
    let candidate = verdict.candidate_sha.clone().expect("a candidate");
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &candidate]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 2, "not one commit: {parents}");
    assert_eq!(parts[1], moved, "not on the new main: {parents}");
    let tree = repo.git(&["rev-parse", &format!("{candidate}^{{tree}}")]);
    let expected = merge_tree(&repo, "squash-merge-commit-tree", &moved, &head);
    assert_eq!(tree, expected, "the squashed tree is not the merge's");
}

/// One integration run with `style`, for the refusal and config cases.
fn style(
    repo: &TestRepo,
    task: &str,
    attempt: &str,
    base: &str,
    head: &str,
    key_commands: &[String],
    style: IntegrationStyle,
) -> Verdict {
    IntegrationRun {
        repository: &repo.repo,
        remote: "origin",
        task,
        title: "A task",
        attempt,
        head_sha: head,
        base_sha: base,
        main_ref: "refs/remotes/origin/main",
        key_commands,
        style,
        profile: "default",
        timeout: Duration::from_secs(30),
    }
    .run()
    .unwrap()
}

fn rebase(
    repo: &TestRepo,
    task: &str,
    attempt: &str,
    base: &str,
    head: &str,
    key_commands: &[String],
) -> Verdict {
    style(
        repo,
        task,
        attempt,
        base,
        head,
        key_commands,
        IntegrationStyle::Rebase,
    )
}

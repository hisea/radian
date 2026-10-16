//! ADR 0030, slice 10: the lead's integration worktree and the merge path.
//!
//! These drive the host side directly ([`radian_host::integration`] and
//! [`radian_host::TaskRegistry::ensure_integration`]): the location beside the task worktrees,
//! lazy creation from the lead's HEAD, the refusal of an existing branch/directory, the merge
//! (`--no-ff` through the `git_commit` path with `radian/integration` as the only allowed branch,
//! no hook and no filter), and a conflict aborting cleanly.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use radian_host::{SessionQueue, TaskBuilder, TaskRegistry};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

/// A private `RADIAN_HOME` for this test binary (the worktree root resolves through it).
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-integ-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs once, before any registry is built.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-integ-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit_all(dir: &Path, message: &str) {
    assert!(git(dir, &["add", "-A"]));
    assert!(git(
        dir,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            message,
        ]
    ));
}

/// A lead repository with one commit on `main`.
fn lead_repo(label: &str) -> PathBuf {
    let _ = isolated_home();
    let lead = scratch(label).join("lead");
    std::fs::create_dir_all(&lead).unwrap();
    assert!(git(&lead, &["init", "-q", "-b", "main"]));
    std::fs::write(lead.join("seed.txt"), "seed\n").unwrap();
    commit_all(&lead, "init");
    std::fs::canonicalize(&lead).unwrap()
}

/// A registry over `lead` whose builder is never called (these tests never spawn a task). The
/// `label` names its journal file: two registries over one lead must not open the same journal
/// (it holds a flock).
fn registry_for(lead: &Path, label: &str) -> TaskRegistry {
    let session_dir = lead.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let journal = Arc::new(
        SessionJournal::create(
            session_dir.join(format!("{label}.jsonl")),
            "lead",
            SessionCreatedData::new(lead.to_string_lossy(), json!({})),
        )
        .expect("the lead journal"),
    );
    let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
    let builder: TaskBuilder = Arc::new(|_ctx| {
        Err(radian_host::SessionError::Message(
            "no task session here".into(),
        ))
    });
    TaskRegistry::new(lead.to_path_buf(), "lead", journal, queue, builder)
}

/// A task branch `radian/<id>` off the lead's HEAD, with one commit that changes `file`.
fn task_branch(lead: &Path, id: &str, file: &str, content: &str) -> PathBuf {
    let worktree = lead.join(format!("task-{id}"));
    assert!(git(
        lead,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("radian/{id}"),
            worktree.to_str().unwrap(),
        ]
    ));
    std::fs::write(worktree.join(file), content).unwrap();
    commit_all(&worktree, &format!("task {id}"));
    assert!(git(
        lead,
        &["worktree", "remove", "--force", worktree.to_str().unwrap()]
    ));
    worktree
}

/// ADR 0030/2: the worktree lives below `<RADIAN_HOME>/worktrees/<key>`,
/// on `radian/integration`, created from the lead's HEAD; a second create refuses.
#[tokio::test]
async fn the_integration_worktree_is_created_lazily_from_the_lead_head_and_never_reset() {
    let home = isolated_home();
    let lead = lead_repo("location");
    let registry = registry_for(&lead, "lead");

    // Nothing exists until the first integration.
    assert!(registry.integration().is_none());

    let integration = registry.ensure_integration().await.expect("create");
    assert_eq!(
        integration.worktree.parent().unwrap().parent().unwrap(),
        home.join("worktrees"),
        "{}",
        integration.worktree.display()
    );
    assert_eq!(integration.worktree.file_name().unwrap(), "integration");
    assert_eq!(integration.branch, "radian/integration");
    assert!(integration.worktree.exists());
    let head = git_output(&lead, &["rev-parse", "HEAD"]);
    assert_eq!(integration.base.as_deref(), Some(head.as_str()));

    // A second ensure reuses the cached worktree (no refusal inside one registry).
    let again = registry.ensure_integration().await.expect("cached");
    assert_eq!(again, integration);

    // A *different* registry (a second lead/process) refuses with **our** refusal, and the branch
    // is not reset behind the human's back (ADR 0030).
    let other = registry_for(&lead, "other");
    let error = other
        .ensure_integration()
        .await
        .expect_err("an existing integration branch must be refused");
    assert!(
        error
            .to_string()
            .contains("refusing to create the integration worktree"),
        "the refusal must be ours: {error}"
    );
    assert_eq!(
        git_output(&lead, &["rev-parse", "radian/integration"]),
        head,
        "an existing integration branch must never be reset"
    );

    // With the worktree gone but the branch left (a previous run that removed its worktree), the
    // refusal is still ours and names the branch (not git's own `worktree add` failure).
    assert!(git(
        &lead,
        &[
            "worktree",
            "remove",
            "--force",
            integration.worktree.to_str().unwrap()
        ]
    ));
    assert!(!integration.worktree.exists());
    let third = registry_for(&lead, "third");
    let error = third
        .ensure_integration()
        .await
        .expect_err("an existing integration branch must be refused");
    assert!(
        error
            .to_string()
            .contains("the branch `radian/integration`"),
        "the branch refusal must name the branch: {error}"
    );
}

/// ADR 0030: an existing integration **directory** (with no branch, e.g. a
/// previous run that left it) is refused too, with our refusal.
#[tokio::test]
async fn an_existing_integration_directory_is_refused() {
    let _ = isolated_home();
    let lead = lead_repo("existing-dir");
    // Create the integration worktree once, then drop the branch and the worktree registration so
    // only a plain directory is left at the path.
    let registry = registry_for(&lead, "lead");
    let integration = registry.ensure_integration().await.expect("create");
    assert!(git(
        &lead,
        &[
            "worktree",
            "remove",
            "--force",
            integration.worktree.to_str().unwrap()
        ]
    ));
    assert!(git(&lead, &["branch", "-D", "radian/integration"]));
    assert!(git(&lead, &["worktree", "prune"]));
    std::fs::create_dir_all(&integration.worktree).unwrap();

    let other = registry_for(&lead, "other");
    let error = other
        .ensure_integration()
        .await
        .expect_err("an existing integration directory must be refused");
    assert!(
        error
            .to_string()
            .contains("refusing to create the integration worktree"),
        "the refusal must be ours: {error}"
    );
}

/// ADR 0030, the ADR test "no tool can move the user branch": the
/// integration worktree holds `radian/integration`, and `git push . radian/integration:main` into
/// the lead's checked-out `main` is refused by `receive.denyCurrentBranch`. So even a raw push
/// from the integration worktree cannot move the user's branch.
#[tokio::test]
async fn a_push_from_the_integration_worktree_cannot_move_the_user_branch() {
    let lead = lead_repo("push");
    let registry = registry_for(&lead, "lead");
    let integration = registry.ensure_integration().await.expect("create");
    let before = git_output(&lead, &["rev-parse", "main"]);
    // Move the integration branch ahead of the user's branch, so there is something to push.
    std::fs::write(integration.worktree.join("integrated.txt"), "x\n").unwrap();
    commit_all(&integration.worktree, "integration ahead");

    let output = std::process::Command::new("git")
        .current_dir(&integration.worktree)
        .args(["push", ".", "radian/integration:main"])
        .output()
        .expect("git push runs");
    assert!(
        !output.status.success(),
        "a push onto the checked-out user branch must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("denyCurrentBranch") || stderr.contains("checked out"),
        "the refusal must be denyCurrentBranch: {stderr}"
    );
    assert_eq!(git_output(&lead, &["rev-parse", "main"]), before);
}

/// ADR 0030: a task branch merges with `--no-ff` and no workspace hook
/// runs; the commit lands on `radian/integration`. (That no repository **filter** runs is the
/// shared host `git` overrides, proven by `radian-coding/tests/host_git.rs`; the merge goes
/// through the same `host_git_command`, and `git_commit` refuses a non-allowed driver outright.)
#[tokio::test]
async fn a_task_branch_merges_without_running_a_hook() {
    let lead = lead_repo("merge");
    let registry = registry_for(&lead, "lead");
    let integration = registry.ensure_integration().await.expect("create");
    let task = task_branch(&lead, "t1", "t1.txt", "from the task\n");

    // A `pre-merge-commit` hook that leaves a marker if it ever runs.
    let hook = lead.join(".git/hooks/pre-merge-commit");
    std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let pin = radian_host::git_commit::RepoPin::resolve(&integration.worktree).expect("pin");
    let outcome = radian_host::integration::merge_task_branch(
        &integration.worktree,
        &integration.branch,
        "t1",
        "radian/t1",
        &pin,
        &[],
        &radian_host::git_commit::default_allowed_filters(),
    )
    .expect("merge t1");
    let committed = outcome.merged().expect("a merge commit").clone();
    assert_eq!(committed.branch, "radian/integration");

    // The merge is a merge commit (the two sides are its parents) on the integration branch.
    let parents = git_output(
        &integration.worktree,
        &["rev-list", "--parents", "-n", "1", "HEAD"],
    );
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    assert!(integration.worktree.join("t1.txt").exists());

    for marker in [integration.worktree.join("hook-ran"), lead.join("hook-ran")] {
        assert!(!marker.exists(), "a hook ran: {}", marker.display());
    }
    let _ = task;
}

/// ADR 0030: a conflict aborts, leaves no `MERGE_HEAD` and a clean tree,
/// and reports the conflicted paths.
#[tokio::test]
async fn a_conflicting_merge_aborts_cleanly_and_reports_the_paths() {
    let lead = lead_repo("conflict");
    std::fs::write(lead.join("f.txt"), "base\n").unwrap();
    commit_all(&lead, "base");

    let registry = registry_for(&lead, "lead");
    let integration = registry.ensure_integration().await.expect("create");
    // A task branch that changes `f.txt` one way.
    let task = task_branch(&lead, "t1", "f.txt", "task\n");
    // The integration branch changes the same file another way and commits.
    std::fs::write(integration.worktree.join("f.txt"), "integration\n").unwrap();
    commit_all(&integration.worktree, "integration side");

    let pin = radian_host::git_commit::RepoPin::resolve(&integration.worktree).expect("pin");
    let outcome = radian_host::integration::merge_task_branch(
        &integration.worktree,
        &integration.branch,
        "t1",
        "radian/t1",
        &pin,
        &[],
        &radian_host::git_commit::default_allowed_filters(),
    )
    .expect("the merge attempt");
    match outcome {
        radian_host::integration::MergeOutcome::Conflict { paths, .. } => {
            assert!(paths.iter().any(|path| path == "f.txt"), "{paths:?}");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    // No merge in progress and a clean tree.
    assert!(
        !git(
            &integration.worktree,
            &["rev-parse", "-q", "--verify", "MERGE_HEAD"]
        ),
        "MERGE_HEAD must be gone"
    );
    assert_eq!(
        git_output(&integration.worktree, &["status", "--porcelain"]),
        "",
        "the worktree must be clean"
    );
    let _ = task;
}

/// ADR 0030 / the ADR test: an integration commit is refused unless HEAD
/// is exactly `radian/integration`.
#[tokio::test]
async fn an_integration_commit_accepts_only_the_integration_branch() {
    let lead = lead_repo("branch");
    let registry = registry_for(&lead, "lead");
    let integration = registry.ensure_integration().await.expect("create");
    std::fs::write(integration.worktree.join("new.txt"), "hello\n").unwrap();
    let pin = radian_host::git_commit::RepoPin::resolve(&integration.worktree).expect("pin");

    // The right branch commits.
    let committed = radian_host::git_commit::commit(
        &integration.worktree,
        &pin,
        "ok",
        &[],
        Some("radian/integration"),
        &[],
        &radian_host::git_commit::default_allowed_filters(),
        true,
        true,
    )
    .expect("a commit on radian/integration");
    assert_eq!(committed.branch, "radian/integration");

    // Any other branch is refused with `wrong_branch`.
    std::fs::write(integration.worktree.join("other.txt"), "hello\n").unwrap();
    let error = radian_host::git_commit::commit(
        &integration.worktree,
        &pin,
        "no",
        &[],
        Some("radian/something-else"),
        &[],
        &radian_host::git_commit::default_allowed_filters(),
        true,
        true,
    )
    .expect_err("a commit off radian/integration must be refused");
    assert_eq!(error.code, "wrong_branch", "{error:?}");
}

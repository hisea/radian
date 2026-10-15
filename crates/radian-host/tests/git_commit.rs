//! ADR 0030 R10: the host-side `git_commit` tool. The blocking `commit()` is the exact code the
//! tool runs, so these tests drive it directly; one async test drives the full `run()` for the
//! audit event.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agent_core::{ToolCall, ToolProvider, ToolSession};
use radian_host::git_commit::{Committed, GitCommitTools, RepoPin, commit, run};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

/// A fresh, uniquely named scratch directory.
fn temp(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "radian-git-commit-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

/// A repo on `feature/x` with one staged-worthy change.
fn repo(label: &str) -> PathBuf {
    let dir = temp(label);
    assert!(git(&dir, &["init", "-q", "-b", "feature/x"]), "git init");
    assert!(
        git(
            &dir,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ),
        "the initial commit"
    );
    change(&dir);
    dir
}

/// One uncommitted change.
fn change(dir: &Path) {
    std::fs::write(
        dir.join("a.txt"),
        format!("{:?}", std::time::Instant::now()),
    )
    .unwrap();
}

/// Resolves the pin the way `open_session` does, then commits — the exact path the tool runs.
fn commit_in(
    dir: &Path,
    message: &str,
    paths: &[String],
    allowed: Option<&str>,
    protected: &[String],
) -> Result<Committed, agent_core::ToolFailure> {
    let pin = RepoPin::resolve(dir).expect("a git repository");
    commit(
        dir,
        &pin,
        message,
        paths,
        allowed,
        protected,
        &radian_host::git_commit::default_allowed_filters(),
    )
}

/// [`commit_in`] with a custom filter allowlist (ADR 0030).
fn commit_with_filters(
    dir: &Path,
    message: &str,
    paths: &[String],
    allowed_filters: &[String],
) -> Result<Committed, agent_core::ToolFailure> {
    let pin = RepoPin::resolve(dir).expect("a git repository");
    commit(dir, &pin, message, paths, None, &[], allowed_filters)
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A workspace-write sandbox, or `None` when this machine has no working backend.
fn workspace_sandbox(ws: &Path) -> Option<radian_sandbox::Sandbox> {
    let policy =
        radian_sandbox::SandboxPolicy::new(radian_sandbox::SandboxMode::WorkspaceWrite, ws).ok()?;
    radian_sandbox::Sandbox::from_policy(policy).ok()
}

/// ADR 0030: the host's git never runs a workspace hook — neither `.git/hooks` nor
/// `core.hooksPath` (which a project can point at a directory inside the writable worktree).
#[test]
fn workspace_hooks_do_not_run() {
    let dir = repo("hooks");
    let marker = dir.join("HOOK-RAN");
    let custom_marker = dir.join("CUSTOM-HOOK-RAN");
    let hook = "#!/bin/sh\ntouch \"$(git rev-parse --show-toplevel)/HOOK-RAN\"\n";
    let custom = "#!/bin/sh\ntouch \"$(git rev-parse --show-toplevel)/CUSTOM-HOOK-RAN\"\n";
    write_executable(&dir.join(".git/hooks/pre-commit"), hook);
    let custom_dir = dir.join("custom-hooks");
    std::fs::create_dir_all(&custom_dir).unwrap();
    write_executable(&custom_dir.join("pre-commit"), custom);
    assert!(git(&dir, &["config", "core.hooksPath", "custom-hooks"]));

    let committed = commit_in(&dir, "message", &[], None, &[]).expect("the commit");
    assert_eq!(committed.branch, "feature/x");
    assert!(
        !marker.exists(),
        "the .git/hooks/pre-commit must not run in the host"
    );
    assert!(
        !custom_marker.exists(),
        "a core.hooksPath hook must not run in the host"
    );
}

/// Paths outside the workspace, `..` and absolute paths are refused before git runs.
#[test]
fn outside_paths_are_refused() {
    let dir = repo("paths");
    let outside = temp("outside");
    let file = outside.join("x.txt");
    std::fs::write(&file, "x").unwrap();

    let absolute = commit_in(&dir, "m", &[file.to_string_lossy().into_owned()], None, &[]);
    assert_eq!(absolute.unwrap_err().code, "outside_workspace");

    let dotdot = commit_in(&dir, "m", &["../x.txt".to_string()], None, &[]);
    assert_eq!(dotdot.unwrap_err().code, "outside_workspace");

    // A path inside the workspace is fine.
    let inside = commit_in(&dir, "m", &["a.txt".to_string()], None, &[]).expect("inside");
    assert_eq!(inside.files, 1);
}

/// The branch rules: `main`/`master`, a configured protected branch, a detached HEAD, and any
/// branch but the allowed one are refused.
#[test]
fn branch_rules_are_enforced() {
    let dir = repo("branches");
    assert!(git(&dir, &["checkout", "-q", "-b", "main"]));
    assert_eq!(
        commit_in(&dir, "m", &[], None, &[]).unwrap_err().code,
        "protected_branch"
    );

    assert!(git(&dir, &["checkout", "-q", "-b", "release"]));
    change(&dir);
    assert_eq!(
        commit_in(&dir, "m", &[], None, &["release".to_string()])
            .unwrap_err()
            .code,
        "protected_branch"
    );

    assert!(git(&dir, &["checkout", "-q", "-b", "radian/t1"]));
    change(&dir);
    assert_eq!(
        commit_in(&dir, "m", &[], Some("radian/t1"), &[])
            .unwrap()
            .branch,
        "radian/t1"
    );
    change(&dir);
    assert_eq!(
        commit_in(&dir, "m", &[], Some("radian/other"), &[])
            .unwrap_err()
            .code,
        "wrong_branch"
    );

    change(&dir);
    assert!(git(&dir, &["checkout", "-q", "--detach"]));
    assert_eq!(
        commit_in(&dir, "m", &[], None, &[]).unwrap_err().code,
        "detached_head"
    );
}

/// The full `run()` appends the non-critical `GitCommitted` audit event.
#[tokio::test]
async fn the_commit_is_audited() {
    let dir = repo("audit");
    let journal_dir = temp("audit-journal");
    let path = journal_dir.join("s.jsonl");
    let journal = Arc::new(
        SessionJournal::create(
            &path,
            "sid",
            SessionCreatedData::new(dir.to_str().unwrap(), json!({"provider": "example"})),
        )
        .expect("a journal"),
    );
    let pin = RepoPin::resolve(&dir).expect("a git repository");
    let tools = GitCommitTools::new(dir.clone(), journal, pin);
    let call = ToolCall {
        id: "t".into(),
        name: "git_commit".into(),
        arguments: json!({ "message": "audited" }),
    };
    let output = run(&tools, &call).await.expect("the commit");
    let text = format!("{:?}", output.content);
    assert!(text.contains("committed"), "{text}");

    let journal_text = std::fs::read_to_string(&path).expect("the journal");
    assert!(
        journal_text.contains("\"event\":\"GitCommitted\""),
        "no GitCommitted audit:\n{journal_text}"
    );
    assert!(
        journal_text.contains("\"branch\":\"feature/x\""),
        "{journal_text}"
    );
    assert!(journal_text.contains("\"files\":1"), "{journal_text}");
}

/// The `workspace` tier keeps `.git` read-only, so a shell `git commit` fails there — while
/// `git_commit`, running in the host, succeeds. Skips when this machine has no backend.
#[tokio::test]
async fn the_sandbox_blocks_shell_commit_but_git_commit_succeeds() {
    let dir = repo("sandbox");
    let Some(sandbox) = workspace_sandbox(&dir) else {
        return; // no working backend here: skip rather than pretend
    };
    let shell = radian_coding::run_user_bash(
        &dir,
        &sandbox,
        "git add -A && git commit -m from-the-sandbox",
        &agent_core::CancellationToken::new(),
    )
    .await
    .expect("the sandboxed command ran (and failed)");
    assert_ne!(
        shell.exit_code,
        Some(0),
        "the sandbox keeps .git read-only: {}",
        shell.stderr
    );

    let committed = commit_in(&dir, "from the host", &[], None, &[]).expect("the host commit");
    assert_eq!(committed.branch, "feature/x");
    assert_eq!(committed.files, 1);
}

/// The threat is real: `core.fsmonitor` (a repo config the model can write when the workspace
/// is not a repo, or is a subdirectory of one) runs a command on `git add`. This is a
/// documented canary for the defense below — it runs raw git, exactly what the host would do
/// without `-c core.fsmonitor=false`.
#[test]
fn the_fsmonitor_threat_is_real_without_the_defense() {
    let dir = repo("fsmonitor-threat");
    let marker = dir.join("THREAT-MARKER");
    let script = dir.join("fsmon.sh");
    write_executable(&script, &format!("#!/bin/sh\ntouch {}\n", marker.display()));
    assert!(git(
        &dir,
        &["config", "core.fsmonitor", script.to_str().unwrap()]
    ));
    change(&dir);
    assert!(git(&dir, &["add", "-A"]), "the raw add");
    assert!(
        marker.exists(),
        "the threat is real: a raw `git add` ran core.fsmonitor"
    );
}

/// The defense: the host's every git call disables `core.fsmonitor`, so a repo config script
/// never runs.
#[test]
fn the_fsmonitor_defense_blocks_a_repo_config_script() {
    let dir = repo("fsmonitor-defense");
    let marker = dir.join("DEFENSE-MARKER");
    let script = dir.join("fsmon.sh");
    write_executable(&script, &format!("#!/bin/sh\ntouch {}\n", marker.display()));
    assert!(git(
        &dir,
        &["config", "core.fsmonitor", script.to_str().unwrap()]
    ));
    change(&dir);
    commit_in(&dir, "m", &[], None, &[]).expect("the commit");
    assert!(
        !marker.exists(),
        "core.fsmonitor must not run: a model-writable config would execute in the host"
    );
}

/// A workspace that is a subdirectory of a repository: the model creates a `.git/` in the
/// workspace root (with a marker fsmonitor); the pinned repository differs, so the commit is
/// refused and the script never runs.
#[test]
fn a_git_dir_created_in_a_subdirectory_workspace_is_refused() {
    let repo = temp("subdir-repo");
    assert!(git(&repo, &["init", "-q", "-b", "feature/x"]));
    assert!(git(
        &repo,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ));
    let ws = repo.join("sub");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hi").unwrap();

    // The pin is taken at "session open", before the model runs: it is the upper repository.
    let pin = RepoPin::resolve(&ws).expect("the upper repository");

    // The model builds its own repository in the workspace.
    assert!(git(&ws, &["init", "-q", "-b", "feature/x"]));
    let marker = ws.join("ESCAPE-MARKER");
    let script = ws.join("fsmon.sh");
    write_executable(&script, &format!("#!/bin/sh\ntouch {}\n", marker.display()));
    assert!(git(
        &ws,
        &["config", "core.fsmonitor", script.to_str().unwrap()]
    ));

    let error = commit(
        &ws,
        &pin,
        "m",
        &[],
        None,
        &[],
        &radian_host::git_commit::default_allowed_filters(),
    )
    .unwrap_err();
    assert_eq!(error.code, "repository_changed", "{error:?}");
    assert!(
        !marker.exists(),
        "the model's repository config must never run"
    );
}

/// A workspace that is not a git repository does not mount `git_commit` at all.
#[tokio::test]
async fn a_non_repo_workspace_does_not_mount_git_commit() {
    let dir = temp("non-repo");
    assert!(RepoPin::resolve(&dir).is_none(), "not a repository");
    let tools = radian_host::HostTools::new(
        None,
        None,
        agent_extensions::McpToolProvider::new(Vec::new()),
        None,
    );
    let session = tools.open_session().await.expect("a session");
    let names: Vec<&str> = session
        .tool_specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(!names.contains(&"git_commit"), "{names:?}");
}

/// The threat is real on `git add` too: a repository's `core.hooksPath` pointing at a directory
/// in the writable worktree (husky's `.husky/`) runs `post-index-change` on the host's `git
/// add`. A documented canary for the defense below (raw git, no override).
#[test]
fn the_post_index_change_threat_is_real_without_the_defense() {
    let dir = repo("post-index-threat");
    let marker = dir.join("THREAT-MARKER");
    let husky = dir.join(".husky");
    std::fs::create_dir_all(&husky).unwrap();
    write_executable(
        &husky.join("post-index-change"),
        &format!("#!/bin/sh\ntouch {}\n", marker.display()),
    );
    assert!(git(&dir, &["config", "core.hooksPath", ".husky"]));
    change(&dir);
    assert!(git(&dir, &["add", "-A"]), "the raw add");
    assert!(
        marker.exists(),
        "the threat is real: a raw `git add` ran the .husky post-index-change hook"
    );
}

/// The defense: every git call (add included) points `core.hooksPath` away, so the hook never
/// runs in the host.
#[test]
fn the_post_index_change_hook_does_not_run() {
    let dir = repo("post-index-defense");
    let marker = dir.join("DEFENSE-MARKER");
    let husky = dir.join(".husky");
    std::fs::create_dir_all(&husky).unwrap();
    write_executable(
        &husky.join("post-index-change"),
        &format!("#!/bin/sh\ntouch {}\n", marker.display()),
    );
    assert!(git(&dir, &["config", "core.hooksPath", ".husky"]));
    change(&dir);
    commit_in(&dir, "m", &[], None, &[]).expect("the commit");
    assert!(
        !marker.exists(),
        "core.hooksPath must not run on git add: a model-writable .husky/ would execute in the host"
    );
}

/// ADR 0030: a `filter=<driver>` attribute runs `filter.<driver>.clean` when the path
/// is staged, and the model can write `.gitattributes`. A driver outside `[git] allowed_filters`
/// refuses the commit before anything is staged, so the script never runs.
#[test]
fn a_filter_driver_not_in_allowed_filters_is_refused() {
    let dir = repo("filter-refused");
    let outside = temp("filter-outside");
    let marker = outside.join("FILTER-RAN");
    let script = dir.join("evil-clean.sh");
    write_executable(
        &script,
        &format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    );
    assert!(git(
        &dir,
        &["config", "filter.evil.clean", script.to_str().unwrap()]
    ));
    std::fs::write(dir.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    change(&dir);

    // With no explicit paths (`git add -A`) the status list is what gets checked.
    let error = commit_in(&dir, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(error.message.contains("a.txt"), "{error:?}");
    assert!(error.message.contains("evil"), "{error:?}");
    assert!(
        !marker.exists(),
        "the disallowed filter must not run in the host"
    );
    // Nothing was staged.
    assert!(
        git(&dir, &["diff", "--cached", "--quiet"]),
        "the refusal stages nothing"
    );

    // The explicit-path branch checks the resolved paths the same way.
    let error = commit_in(&dir, "m", &["a.txt".to_string()], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(!marker.exists());
}

/// The opt-in: a driver the user named in `allowed_filters` runs (its command is not inspected).
#[test]
fn a_driver_named_in_allowed_filters_is_allowed() {
    let dir = repo("filter-allowed");
    let marker = dir.join("FILTER-RAN");
    let script = dir.join("custom-clean.sh");
    write_executable(
        &script,
        &format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    );
    assert!(git(
        &dir,
        &["config", "filter.custom.clean", script.to_str().unwrap()]
    ));
    std::fs::write(dir.join(".gitattributes"), "*.txt filter=custom\n").unwrap();
    change(&dir);

    let allowed = vec!["custom".to_owned()];
    let committed = commit_with_filters(&dir, "m", &[], &allowed).expect("allowed by name");
    assert!(
        committed.files >= 2,
        "a.txt and .gitattributes: {committed:?}"
    );
    assert!(
        marker.exists(),
        "an allowed driver runs: the user opted in by name"
    );
}

/// `lfs` is the default allowed driver, but only when its commands are a `git-lfs` command.
#[test]
fn lfs_is_allowed_only_with_a_git_lfs_command() {
    let dir = repo("filter-lfs");
    assert!(git(
        &dir,
        &["config", "filter.lfs.clean", "git-lfs clean -- %f"]
    ));
    assert!(git(
        &dir,
        &["config", "filter.lfs.process", "git-lfs filter-process"]
    ));
    std::fs::write(dir.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
    std::fs::write(dir.join("b.bin"), "binary\n").unwrap();

    // The check passes and `git add` is attempted. Whether `git-lfs` is installed decides the
    // outcome; the filter check itself must not refuse.
    match commit_in(&dir, "m", &[], None, &[]) {
        Ok(committed) => assert!(committed.files >= 1),
        Err(error) => {
            assert_ne!(
                error.code, "filter_not_allowed",
                "a git-lfs command must be allowed: {error:?}"
            );
            assert_eq!(
                error.code, "git_failed",
                "staging must have been attempted: {error:?}"
            );
        }
    }

    // A non-git-lfs `clean` for the `lfs` driver is refused.
    let other = repo("filter-lfs-other");
    let marker = other.join("LFS-FILTER-RAN");
    let script = other.join("lfs-clean.sh");
    write_executable(
        &script,
        &format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    );
    assert!(git(
        &other,
        &["config", "filter.lfs.clean", script.to_str().unwrap()]
    ));
    std::fs::write(other.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
    std::fs::write(other.join("b.bin"), "binary\n").unwrap();
    let error = commit_in(&other, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(error.message.contains("filter.lfs.clean"), "{error:?}");
    assert!(!marker.exists(), "the non-git-lfs clean must not run");
}

/// A repo whose `.gitattributes` routes `*.txt` through `filter.evil`, whose `clean` touches
/// `marker` outside the repo, with one `.txt` file at `path`. Returns `(dir, marker)`.
fn evil_filter_repo(label: &str, path: &str) -> (PathBuf, PathBuf) {
    let dir = repo(label);
    // `repo` wrote an untracked `a.txt`; remove it so only the intended path triggers.
    let _ = std::fs::remove_file(dir.join("a.txt"));
    let outside = temp(&format!("{label}-outside"));
    let marker = outside.join("FILTER-RAN");
    let script = dir.join("evil-clean.sh");
    write_executable(
        &script,
        &format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    );
    assert!(git(
        &dir,
        &["config", "filter.evil.clean", script.to_str().unwrap()]
    ));
    std::fs::write(dir.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir.join(parent)).unwrap();
    }
    std::fs::write(dir.join(path), "payload\n").unwrap();
    (dir, marker)
}

/// ADR 0030 (review fix): `git status` reports a new directory as `newdir/`, whose
/// `filter` attribute is `unspecified`, so the check must expand it to the files underneath.
#[test]
fn a_new_file_in_a_new_directory_is_checked() {
    let (dir, marker) = evil_filter_repo("filter-newdir", "newdir/a.txt");
    let error = commit_in(&dir, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(error.message.contains("newdir/a.txt"), "{error:?}");
    assert!(
        !marker.exists(),
        "a new directory must not hide its files from the check"
    );
}

/// An explicit directory argument is expanded to its files for the check.
#[test]
fn a_directory_argument_is_expanded_for_the_filter_check() {
    let (dir, marker) = evil_filter_repo("filter-dirarg", "newdir/a.txt");
    let error = commit_in(&dir, "m", &["newdir".to_string()], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(error.message.contains("newdir/a.txt"), "{error:?}");
    assert!(
        !marker.exists(),
        "a directory argument must not bypass the check"
    );
}

/// `status.showUntrackedFiles=no` hides untracked files from `git status`; the check must not
/// depend on it.
#[test]
fn status_show_untracked_files_no_does_not_hide_the_check() {
    let (dir, marker) = evil_filter_repo("filter-showuntracked", "newdir/a.txt");
    assert!(git(&dir, &["config", "status.showUntrackedFiles", "no"]));
    let error = commit_in(&dir, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(
        !marker.exists(),
        "status.showUntrackedFiles=no must not hide an untracked file"
    );
}

/// ADR 0030 (review fix): the path list the filter check reads is every tracked
/// file, so it must go through stdin. With ~30k long names the argv list is well over the
/// 1 MiB `ARG_MAX`, and `git check-attr` fails with `E2BIG`, which would make `git_commit`
/// fail in any large repository. It must succeed here.
#[test]
fn a_large_repository_does_not_hit_the_argument_limit() {
    let dir = repo("filter-large");
    // `repo` wrote an untracked `a.txt`; drop it so the tree is just the many files.
    let _ = std::fs::remove_file(dir.join("a.txt"));
    for i in 0..30_000 {
        let name = format!("{i:06}-{}.dat", "x".repeat(40));
        std::fs::write(dir.join(name), "").unwrap();
    }
    assert!(git(&dir, &["add", "-A"]), "add the many files");
    assert!(
        git(
            &dir,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "many",
            ],
        ),
        "commit the many files"
    );
    // One more change, then a normal commit: the check must not fail with E2BIG.
    std::fs::write(dir.join("last.txt"), "last\n").unwrap();
    let committed = commit_in(&dir, "m", &[], None, &[]).expect("a large repo commits");
    assert!(committed.files >= 1, "{committed:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

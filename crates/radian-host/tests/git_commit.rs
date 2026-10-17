//! ADR 0030 R10: the host-side `git_commit` tool. The blocking `commit()` is the exact code the
//! tool runs, so these tests drive it directly; one async test drives the full `run()` for the
//! audit event.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agent_core::{ToolCall, ToolProvider, ToolSession};
use radian_host::git_commit::{Committed, GitCommitTools, RepoPin, commit, dirty, run};
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

/// `git`'s trimmed stdout, for a query the test asserts on.
fn git_text(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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
/// `refuse_main = true`: the task-agent rule (the original behavior these tests pinned).
fn commit_in(
    dir: &Path,
    message: &str,
    paths: &[String],
    allowed: Option<&str>,
    protected: &[String],
) -> Result<Committed, agent_core::ToolFailure> {
    commit_in_as(dir, message, paths, allowed, protected, true)
}

/// [`commit_in`] with the ADR 0038 decision 4 switch explicit: `refuse_main = false` is the
/// interactive `general`/`lead` session, which may commit on `main`.
fn commit_in_as(
    dir: &Path,
    message: &str,
    paths: &[String],
    allowed: Option<&str>,
    protected: &[String],
    refuse_main: bool,
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
        refuse_main,
        false,
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
    commit(
        dir,
        &pin,
        message,
        paths,
        None,
        &[],
        allowed_filters,
        true,
        false,
    )
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

/// ADR 0030 O3 (review fix 2): an agent that may commit only to its own branch refuses every
/// commit when it has none (the `shared` case), even if it reached the tool. With its own branch
/// it commits there.
#[test]
fn a_session_with_no_branch_of_its_own_cannot_commit() {
    let dir = repo("require-branch");
    change(&dir);
    let pin = RepoPin::resolve(&dir).expect("a git repository");

    let error = commit(
        &dir,
        &pin,
        "m",
        &[],
        None,
        &[],
        &radian_host::git_commit::default_allowed_filters(),
        false,
        true,
    )
    .unwrap_err();
    assert_eq!(error.code, "no_branch", "{error:?}");

    let committed = commit(
        &dir,
        &pin,
        "m",
        &[],
        Some("feature/x"),
        &[],
        &radian_host::git_commit::default_allowed_filters(),
        false,
        true,
    )
    .expect("its own branch commits");
    assert_eq!(committed.branch, "feature/x");
}

/// ADR 0038 decision 4: a `general`/`lead` session may commit on `main`/`master`; a configured
/// protected branch and a detached HEAD are still refused.
#[test]
fn a_general_session_may_commit_on_main() {
    let dir = temp("general-main");
    assert!(git(&dir, &["init", "-q", "-b", "main"]));
    assert!(git(
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
        ]
    ));
    change(&dir);
    let committed = commit_in_as(&dir, "on main", &[], None, &[], false).expect("main is allowed");
    assert_eq!(committed.branch, "main");
    assert_eq!(committed.files, 1);

    // `master` too.
    let master = temp("general-master");
    assert!(git(&master, &["init", "-q", "-b", "master"]));
    assert!(git(
        &master,
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
        ]
    ));
    change(&master);
    assert_eq!(
        commit_in_as(&master, "on master", &[], None, &[], false)
            .expect("master is allowed")
            .branch,
        "master"
    );

    // A configured protected branch is refused for a general session too.
    assert_eq!(
        commit_in_as(&dir, "m", &[], None, &["main".to_string()], false)
            .unwrap_err()
            .code,
        "protected_branch"
    );
    // A detached HEAD is refused for a general session too.
    assert!(git(&dir, &["checkout", "-q", "--detach"]));
    assert_eq!(
        commit_in_as(&dir, "m", &[], None, &[], false)
            .unwrap_err()
            .code,
        "detached_head"
    );
}

/// ADR 0038 decision 4 / ADR 0030 O3: a task session keeps the `main`/`master` refusal and is
/// limited to its own `radian/<id>` branch.
#[test]
fn a_task_agent_is_refused_on_main_and_on_any_other_branch() {
    let dir = temp("task-branches");
    assert!(git(&dir, &["init", "-q", "-b", "main"]));
    assert!(git(
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
        ]
    ));
    change(&dir);
    // A task agent on `main` is refused even with `allowed_branch = radian/t1`.
    assert_eq!(
        commit_in_as(&dir, "m", &[], Some("radian/t1"), &[], true)
            .unwrap_err()
            .code,
        "protected_branch"
    );
    // On its own branch it succeeds.
    assert!(git(&dir, &["checkout", "-q", "-b", "radian/t1"]));
    change(&dir);
    assert_eq!(
        commit_in_as(&dir, "m", &[], Some("radian/t1"), &[], true)
            .expect("the task's own branch")
            .branch,
        "radian/t1"
    );
    // On any other branch it is refused with `wrong_branch`.
    assert!(git(&dir, &["checkout", "-q", "-b", "other"]));
    change(&dir);
    assert_eq!(
        commit_in_as(&dir, "m", &[], Some("radian/t1"), &[], true)
            .unwrap_err()
            .code,
        "wrong_branch"
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
        true,
        false,
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

/// ADR 0039 decision 3: Radian runs only `lfs` itself, with its own fixed `git-lfs clean`; it
/// never reads a filter command from the repository config. A driver the user named in
/// `allowed_filters` but which is not `lfs` is therefore refused — and its configured script
/// never runs.
#[test]
fn a_non_lfs_driver_is_refused_even_when_named_in_allowed_filters() {
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
    let error = commit_with_filters(&dir, "m", &[], &allowed).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(error.message.contains("custom"), "{error:?}");
    assert!(
        !marker.exists(),
        "a configured filter command must never run in the host"
    );
    assert!(
        git(&dir, &["diff", "--cached", "--quiet"]),
        "the refusal stages nothing"
    );
}

/// ADR 0039 decision 3: an allowed `lfs` path is filtered by Radian's **own** fixed command,
/// `git-lfs clean -- <path>`, never by the `filter.lfs.clean` string from git config. With a fake
/// `git-lfs` (the real one is often absent) the committed blob is the fake's stdout, the fake
/// sees the fixed argv, and a configured `filter.lfs.clean` script cannot run. Without `git-lfs`
/// the path is refused with a clear error.
#[test]
fn lfs_runs_radians_own_command_and_never_the_configured_one() {
    let dir = repo("filter-lfs");
    let configured_marker = dir.join("CONFIGURED-LFS-RAN");
    let configured = dir.join("lfs-clean.sh");
    write_executable(
        &configured,
        &format!("#!/bin/sh\ntouch {}\ncat\n", configured_marker.display()),
    );
    assert!(git(
        &dir,
        &["config", "filter.lfs.clean", configured.to_str().unwrap()]
    ));
    assert!(git(
        &dir,
        &["config", "filter.lfs.process", configured.to_str().unwrap()]
    ));
    std::fs::write(dir.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
    std::fs::write(dir.join("b.bin"), "binary\n").unwrap();

    // No `git-lfs`: refuse the path, and never fall back to the configured command.
    unsafe {
        std::env::set_var("RADIAN_GIT_LFS", dir.join("no-such-git-lfs"));
    }
    let error = commit_in(&dir, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_unavailable", "{error:?}");
    assert!(
        !configured_marker.exists(),
        "the configured lfs command must not run"
    );
    assert!(
        git(&dir, &["diff", "--cached", "--quiet"]),
        "a missing git-lfs stages nothing"
    );

    // A fake `git-lfs` on Radian's own command line: it logs its argv and cats a pointer file.
    let pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 7\n";
    let pointer_file = temp("filter-lfs-pointer").join("pointer");
    std::fs::write(&pointer_file, pointer).unwrap();
    let args_file = dir.join("LFS-ARGS");
    let fake = dir.join("fake-git-lfs");
    write_executable(
        &fake,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\ncat {}\n",
            args_file.display(),
            pointer_file.display()
        ),
    );
    unsafe {
        std::env::set_var("RADIAN_GIT_LFS", &fake);
    }
    let committed = commit_in(&dir, "m", &[], None, &[]).expect("radian's own git-lfs runs");
    assert!(committed.files >= 1, "{committed:?}");
    assert!(
        !configured_marker.exists(),
        "the configured lfs command must not run"
    );
    let args = std::fs::read_to_string(&args_file).expect("the fake logged its argv");
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["clean", "--", "b.bin"],
        "Radian's fixed command line"
    );
    // The committed blob is the pointer Radian hashed, not the working-tree content.
    let committed_blob = git_text(&dir, &["rev-parse", "HEAD:b.bin"]);
    std::fs::write(dir.join("pointer.txt"), pointer).unwrap();
    let pointer_blob = git_text(&dir, &["hash-object", "--no-filters", "--", "pointer.txt"]);
    assert_eq!(committed_blob, pointer_blob);
    let _ = std::fs::remove_file(dir.join("pointer.txt"));
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

/// Configures `filter.<driver>.<kind>` in `dir` to a command that touches `marker`, then passes
/// the input through (a marker-writing filter, ADR 0039 decision 3).
fn marker_filter(dir: &Path, driver: &str, kind: &str, marker: &Path) {
    let script = dir.join(format!("{driver}-{kind}.sh"));
    write_executable(
        &script,
        &format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    );
    assert!(
        git(
            dir,
            &[
                "config",
                &format!("filter.{driver}.{kind}"),
                script.to_str().unwrap(),
            ],
        ),
        "configure filter.{driver}.{kind}"
    );
}

/// ADR 0039 decision 3, the vector `GIT_ATTR_SOURCE=<empty tree>` does not close: a driver named
/// in `$GIT_DIR/info/attributes`. The commit reads it (and refuses); `dirty` reads attributes
/// without a check, so the no-filter overrides are what keep its command from running.
#[test]
fn a_driver_in_info_attributes_never_runs() {
    let dir = repo("filter-info-attrs");
    // Track a.txt first (no attributes yet), so the dirty check has a tracked stat-dirty file:
    // `git status` runs the clean filter for one, not for an untracked path.
    commit_in(&dir, "init", &[], None, &[]).expect("the initial commit");
    let marker = temp("filter-info-marker").join("FILTER-RAN");
    marker_filter(&dir, "evil", "clean", &marker);
    std::fs::create_dir_all(dir.join(".git/info")).unwrap();
    std::fs::write(dir.join(".git/info/attributes"), "*.txt filter=evil\n").unwrap();
    change(&dir);

    let pin = RepoPin::resolve(&dir).expect("a repository");
    assert!(dirty(&dir, &pin).expect("the dirty check"), "a.txt changed");
    assert!(!marker.exists(), "the dirty check must not run a filter");

    let error = commit_in(&dir, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(
        !marker.exists(),
        "info/attributes must not reach a filter command"
    );
}

/// The same for `core.attributesFile`, which also survives `GIT_ATTR_SOURCE=empty`.
#[test]
fn a_driver_in_core_attributes_file_never_runs() {
    let dir = repo("filter-caf");
    commit_in(&dir, "init", &[], None, &[]).expect("the initial commit");
    let marker = temp("filter-caf-marker").join("FILTER-RAN");
    marker_filter(&dir, "evil", "clean", &marker);
    let attrs = temp("filter-caf-attrs").join("attributes");
    std::fs::write(&attrs, "*.txt filter=evil\n").unwrap();
    assert!(git(
        &dir,
        &["config", "core.attributesFile", attrs.to_str().unwrap()]
    ));
    change(&dir);

    let pin = RepoPin::resolve(&dir).expect("a repository");
    assert!(dirty(&dir, &pin).expect("the dirty check"), "a.txt changed");
    assert!(
        !marker.exists(),
        "core.attributesFile must not run a filter"
    );

    let error = commit_in(&dir, "m", &[], None, &[]).unwrap_err();
    assert_eq!(error.code, "filter_not_allowed", "{error:?}");
    assert!(
        !marker.exists(),
        "core.attributesFile must not run a filter"
    );
}

/// ADR 0037 background job vs ADR 0039 decision 3: a thread rewrites `.gitattributes` to name a
/// marker filter, over and over, while commits run. Whatever the interleaving, the filter never
/// runs — staging is plumbing, and the commit's own index refresh carries the no-filter
/// overrides, so there is no check-then-use window. No sleeps.
///
/// The flipped file is excluded from the commit's path set (`.git/info/exclude`): the flip still
/// races the attribute read for `a.txt`, but it can no longer race `git hash-object`, which would
/// otherwise read `.gitattributes` mid-rewrite and fail with "short read while indexing" — a
/// `git_failed`, not the filter refusal under test.
#[test]
fn a_gitattributes_flip_during_commit_never_runs_a_filter() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = repo("filter-flip");
    let marker = temp("filter-flip-marker").join("FILTER-RAN");
    marker_filter(&dir, "flip", "clean", &marker);
    marker_filter(&dir, "flip", "smudge", &marker);
    marker_filter(&dir, "flip", "process", &marker);
    change(&dir);

    // The flipper rewrites `.gitattributes` in place, and `check-attr` reads attributes from the
    // worktree whether or not the file is tracked. Excluding it from the commit's path set keeps
    // it out of `git ls-files --others --exclude-standard`, so the plumbing never hashes a file
    // that is being rewritten: without this, `git hash-object` reads `.gitattributes` between the
    // flipper's truncate and its write and fails with "short read while indexing" (`git_failed`),
    // which is not the refusal this test is about. The flip still races the attribute read for
    // `a.txt`, which is the property pinned below.
    {
        use std::io::Write;
        let mut exclude = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(".git/info/exclude"))
            .expect("the repository's exclude file");
        writeln!(exclude, ".gitattributes").expect("exclude .gitattributes from the commit");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let attrs = dir.join(".gitattributes");
    let flipper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let _ = std::fs::write(&attrs, "*.txt filter=flip\n");
                let _ = std::fs::write(&attrs, "# no filter\n");
            }
        })
    };

    // Several commits, so the flip and a commit overlap in many schedules.
    for n in 0..8 {
        change(&dir);
        match commit_in(&dir, &format!("flip {n}"), &[], None, &[]) {
            Ok(_) => {}
            Err(error) => assert_eq!(error.code, "filter_not_allowed", "{error:?}"),
        }
    }
    stop.store(true, Ordering::SeqCst);
    flipper.join().unwrap();
    assert!(
        !marker.exists(),
        "no filter may run, whatever .gitattributes said"
    );
}

/// ADR 0039 decision 3: for a path with no filter, the plumbing stages exactly the blob `git add`
/// would.
#[test]
fn a_plain_file_stages_the_blob_git_add_would() {
    let dir = repo("filter-blob-eq");
    std::fs::write(dir.join("new.txt"), "new content\n").unwrap();
    let committed = commit_in(&dir, "m", &[], None, &[]).expect("the commit");
    assert!(committed.files >= 2, "{committed:?}");
    let staged = git_text(&dir, &["rev-parse", "HEAD:new.txt"]);
    let raw = git_text(&dir, &["hash-object", "--no-filters", "--", "new.txt"]);
    assert_eq!(staged, raw);
}

/// ADR 0039 decision 3: the executable bit and a symlink survive the plumbing (a symlink's blob
/// is its target path, which `git add` stores too).
#[cfg(unix)]
#[test]
fn the_executable_bit_and_symlinks_survive_staging() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = repo("filter-modes");
    let _ = std::fs::remove_file(dir.join("a.txt"));
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink("run.sh", dir.join("link")).unwrap();

    let committed = commit_in(&dir, "m", &[], None, &[]).expect("the commit");
    assert!(committed.files >= 2, "{committed:?}");
    let tree = git_text(&dir, &["ls-tree", "-r", "HEAD"]);
    assert!(tree.contains("100755") && tree.contains("run.sh"), "{tree}");
    assert!(tree.contains("120000") && tree.contains("link"), "{tree}");

    let link_blob = git_text(&dir, &["rev-parse", "HEAD:link"]);
    std::fs::write(dir.join("target.txt"), "run.sh").unwrap();
    let target_blob = git_text(&dir, &["hash-object", "--no-filters", "--", "target.txt"]);
    assert_eq!(link_blob, target_blob);
}

/// ADR 0039 decision 3: the plumbing covers the `git add -A` shapes — a deletion and the files of
/// a new directory.
#[test]
fn staging_covers_deletions_and_a_new_directory() {
    let dir = repo("filter-shapes");
    commit_in(&dir, "init", &[], None, &[]).expect("the initial commit");
    std::fs::create_dir_all(dir.join("new/deep")).unwrap();
    std::fs::write(dir.join("new/deep/x.txt"), "x\n").unwrap();
    std::fs::remove_file(dir.join("a.txt")).unwrap();

    let committed = commit_in(&dir, "shapes", &[], None, &[]).expect("the commit");
    assert!(committed.files >= 2, "{committed:?}");
    let tree = git_text(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("new/deep/x.txt"), "{tree}");
    assert!(
        !tree.contains("a.txt"),
        "the deletion must be staged: {tree}"
    );
}

/// ADR 0039 decision 3 (review fix 1): a moved gitlink is staged as mode 160000 with the
/// submodule's HEAD, and nothing inside the submodule runs a filter during the commit (only
/// `git rev-parse HEAD` runs there, with the shared no-filter overrides).
#[test]
fn a_moved_submodule_is_staged_as_a_gitlink_without_running_a_filter() {
    // The submodule's own repository, with two commits.
    let sub = temp("submodule-src");
    assert!(git(&sub, &["init", "-q", "-b", "main"]));
    assert!(git(&sub, &["config", "user.email", "t@example.com"]));
    assert!(git(&sub, &["config", "user.name", "t"]));
    std::fs::write(sub.join("s.txt"), "s1\n").unwrap();
    assert!(git(&sub, &["add", "-A"]));
    assert!(git(&sub, &["commit", "-q", "-m", "s1"]));
    let first = git_text(&sub, &["rev-parse", "HEAD"]);
    std::fs::write(sub.join("s.txt"), "s2\n").unwrap();
    assert!(git(&sub, &["commit", "-qam", "s2"]));

    // The superproject, with the submodule at its second commit.
    let dir = temp("submodule-super");
    assert!(git(&dir, &["init", "-q", "-b", "feature/x"]));
    assert!(git(&dir, &["config", "user.email", "t@example.com"]));
    assert!(git(&dir, &["config", "user.name", "t"]));
    std::fs::write(dir.join("top.txt"), "top\n").unwrap();
    assert!(git(&dir, &["add", "-A"]));
    assert!(git(&dir, &["commit", "-q", "-m", "init"]));
    assert!(git(
        &dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub.to_str().unwrap(),
            "sub",
        ]
    ));
    assert!(git(&dir, &["commit", "-q", "-m", "add sub"]));

    // Move the gitlink back to the first commit, and arm a marker filter in the submodule.
    let checkout = dir.join("sub");
    assert!(git(&checkout, &["checkout", "-q", first.as_str()]));
    let marker = temp("submodule-marker").join("FILTER-RAN");
    let script = checkout.join("evil-clean.sh");
    write_executable(
        &script,
        &format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    );
    assert!(git(
        &checkout,
        &["config", "filter.evil.clean", script.to_str().unwrap()]
    ));
    assert!(git(&checkout, &["config", "filter.evil.required", "true"]));
    std::fs::write(checkout.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    // Stat-dirty inside the submodule.
    std::fs::write(checkout.join("s.txt"), "s1-changed\n").unwrap();

    let committed = commit_in(&dir, "move the submodule", &[], None, &[]).expect("the commit");
    assert!(committed.files >= 1, "{committed:?}");
    let staged = git_text(&dir, &["ls-tree", "HEAD", "sub"]);
    assert!(
        staged.starts_with("160000") && staged.contains(first.as_str()),
        "the moved submodule must be staged as a 160000 gitlink at {first}: {staged}"
    );
    assert!(
        !marker.exists(),
        "nothing inside the submodule may run a filter in the host"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

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
    commit(dir, &pin, message, paths, allowed, protected)
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

    let error = commit(&ws, &pin, "m", &[], None, &[]).unwrap_err();
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

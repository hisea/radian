//! ADR 0039 decision 5: in a project the user has **not** trusted, every `shell` child
//! (foreground and background job alike, sandboxed or approved `outside_sandbox`) carries
//! the host git hardening as `GIT_CONFIG_*` environment, so repository configuration
//! cannot run commands through a shell git. In a trusted project nothing is injected, so
//! the user's own hooks still run.
//!
//! Each test is self-validating: it first runs the same command **without** hardening and
//! asserts the marker appears (the trap works), then with hardening and asserts it does
//! not (while git itself still succeeds).

mod common;

use std::path::Path;
use std::sync::Arc;

use radian_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::{CodingSession, CodingTools, Jobs};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::{Value, json};

fn full_access(ws: &Path) -> Sandbox {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    Sandbox::from_policy(policy).expect("full-access needs no backend")
}

/// The tools with the session's own jobs directory and git hardening on or off.
fn tools(ws: &Path, jobs_dir: &Path, harden_git: bool) -> CodingTools {
    if let Some(parent) = jobs_dir.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    CodingTools::with_sandbox(ws, full_access(ws))
        .expect("matching workspaces")
        .with_jobs(Arc::new(Jobs::new(jobs_dir).with_git_hardening(harden_git)))
}

async fn exec(
    session: &CodingSession,
    name: &str,
    arguments: Value,
) -> Result<ToolOutput, ToolFailure> {
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments,
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    session.execute(&call, cx).await
}

fn output_text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Runs `git` directly (not through a tool) for setup and assertions.
fn git(ws: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(ws)
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// A scratch repository with an initial commit and a local identity.
fn repo(ws: &Path) {
    git(ws, &["init", "-q", "."]);
    git(ws, &["config", "user.email", "test@example.com"]);
    git(ws, &["config", "user.name", "test"]);
    std::fs::write(ws.join("a.txt"), "hello\n").unwrap();
    git(ws, &["add", "a.txt"]);
    git(ws, &["commit", "-qm", "init"]);
}

/// Writes an executable marker script: running it creates `marker`.
fn marker_script(path: &Path, marker: &Path, extra_stdin: bool) {
    let mut script = format!("#!/bin/sh\ntouch {}\n", marker.display());
    if extra_stdin {
        script.push_str("cat\n");
    }
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

/// A `git status` through `shell` must not run the repository's `core.fsmonitor`.
#[tokio::test]
async fn untrusted_shell_does_not_run_repo_fsmonitor() {
    let dir = TempDir::new("shell-git-fsmonitor");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-fsmonitor");
    let hook = ws.join("fsmonitor-hook.sh");
    marker_script(&hook, &marker, false);
    git(&ws, &["config", "core.fsmonitor", &hook.to_string_lossy()]);

    // Control: without hardening the trap fires.
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    let out = exec(
        &session,
        "shell",
        json!({"command": "git status --porcelain --branch"}),
    )
    .await
    .unwrap();
    assert!(output_text(&out).contains("## "), "{out:?}");
    assert!(marker.exists(), "the control must fire the hook");

    // Hardened: the same command succeeds without running the hook.
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    let out = exec(
        &session,
        "shell",
        json!({"command": "git status --porcelain --branch"}),
    )
    .await
    .unwrap();
    assert!(output_text(&out).contains("## "), "{out:?}");
    assert!(!marker.exists(), "hardened git must not run core.fsmonitor");
}

/// A background job runs under the same hardening as a foreground command.
#[tokio::test]
async fn untrusted_background_job_does_not_run_repo_fsmonitor() {
    let dir = TempDir::new("shell-git-fsmonitor-bg");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-fsmonitor-bg");
    let hook = ws.join("fsmonitor-hook.sh");
    marker_script(&hook, &marker, false);
    git(&ws, &["config", "core.fsmonitor", &hook.to_string_lossy()]);

    async fn status_in_background(session: &CodingSession) -> String {
        let started = exec(
            session,
            "shell",
            json!({"command": "git status --porcelain --branch", "background": true}),
        )
        .await
        .unwrap();
        let id = started
            .structured
            .clone()
            .expect("structured")
            .get("job_id")
            .and_then(|id| id.as_str())
            .expect("a job id")
            .to_owned();
        let done = exec(
            session,
            "job",
            json!({"id": id, "op": "wait", "timeout_ms": 30_000}),
        )
        .await
        .unwrap();
        output_text(&done)
    }

    // Control: without hardening the trap fires.
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    let done = status_in_background(&session).await;
    assert!(done.contains("## "), "{done}");
    assert!(marker.exists(), "the control must fire the hook");

    // Hardened: the job succeeds without running the hook.
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    let done = status_in_background(&session).await;
    assert!(done.contains("## "), "{done}");
    assert!(
        !marker.exists(),
        "a hardened job must not run core.fsmonitor"
    );
    session.shutdown_jobs();
}

/// A hardened `git add` must not run the repository's `filter.<driver>.clean`: the driver
/// is still named by `.gitattributes`, but its command is emptied (this also proves empty
/// values survive the `GIT_CONFIG_VALUE_*` spelling).
#[tokio::test]
async fn untrusted_shell_empties_repo_filter_commands() {
    let dir = TempDir::new("shell-git-filter");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-filter");
    let clean = ws.join("clean-hook.sh");
    marker_script(&clean, &marker, true);
    git(
        &ws,
        &["config", "filter.evil.clean", &clean.to_string_lossy()],
    );
    git(&ws, &["config", "filter.evil.smudge", "cat"]);
    git(&ws, &["config", "filter.evil.required", "true"]);
    std::fs::write(ws.join(".gitattributes"), "*.txt filter=evil\n").unwrap();

    // Control: without hardening the clean filter fires on `git add`.
    std::fs::write(ws.join("b.txt"), "data\n").unwrap();
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    exec(&session, "shell", json!({"command": "git add b.txt"}))
        .await
        .unwrap();
    assert!(marker.exists(), "the control must fire the clean filter");

    // Hardened: the same add succeeds without running the filter, and stages the file.
    git(&ws, &["rm", "-q", "--cached", "b.txt"]);
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    exec(&session, "shell", json!({"command": "git add b.txt"}))
        .await
        .unwrap();
    assert!(
        !marker.exists(),
        "hardened git must not run the clean filter"
    );
    let staged = exec(
        &session,
        "shell",
        json!({"command": "git diff --cached --name-only"}),
    )
    .await
    .unwrap();
    assert!(
        output_text(&staged).contains("b.txt"),
        "the file is staged: {staged:?}"
    );
}

/// A hardened `git diff` must not run the repository's `diff.<driver>.textconv`: the driver
/// is still named by `.gitattributes`, but its command becomes `cat`, so the text diff is
/// preserved (verified on git 2.53.0: an empty override would error out instead).
#[tokio::test]
async fn untrusted_shell_neutralises_repo_diff_textconv() {
    let dir = TempDir::new("shell-git-textconv");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-textconv");
    let conv = ws.join("textconv-hook.sh");
    marker_script(&conv, &marker, true);
    git(
        &ws,
        &["config", "diff.tc.textconv", &conv.to_string_lossy()],
    );
    std::fs::write(ws.join(".gitattributes"), "*.bin diff=tc\n").unwrap();
    std::fs::write(ws.join("f.bin"), "AAA\n").unwrap();
    git(&ws, &["add", ".gitattributes", "f.bin"]);
    git(&ws, &["commit", "-qm", "baseline"]);
    std::fs::write(ws.join("f.bin"), "BBB\n").unwrap();

    // Control: without hardening the textconv fires on `git diff`.
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    let out = exec(&session, "shell", json!({"command": "git diff -- f.bin"}))
        .await
        .unwrap();
    assert!(output_text(&out).contains("exit code: 0"), "{out:?}");
    assert!(marker.exists(), "the control must fire the textconv");

    // Hardened: the same diff succeeds without running the converter, and still shows
    // the text diff.
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    let out = exec(&session, "shell", json!({"command": "git diff -- f.bin"}))
        .await
        .unwrap();
    let text = output_text(&out);
    assert!(text.contains("exit code: 0"), "{out:?}");
    assert!(!marker.exists(), "hardened git must not run the textconv");
    assert!(
        text.contains("-AAA") && text.contains("+BBB"),
        "the text diff is preserved: {out:?}"
    );
}

/// A hardened `git diff` must not run the repository's `diff.external`: it becomes `true`,
/// so the patch is suppressed but the command still succeeds (verified on git 2.53.0: an
/// empty override would error out instead).
#[tokio::test]
async fn untrusted_shell_disables_repo_diff_external() {
    let dir = TempDir::new("shell-git-external");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-external");
    let external = ws.join("external-hook.sh");
    marker_script(&external, &marker, false);
    git(
        &ws,
        &["config", "diff.external", &external.to_string_lossy()],
    );
    std::fs::write(ws.join("a.txt"), "hello\nchange\n").unwrap();

    // Control: without hardening the external diff fires.
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    exec(&session, "shell", json!({"command": "git diff -- a.txt"}))
        .await
        .unwrap();
    assert!(marker.exists(), "the control must fire the external diff");

    // Hardened: the same diff succeeds without running it.
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    let out = exec(&session, "shell", json!({"command": "git diff -- a.txt"}))
        .await
        .unwrap();
    let text = output_text(&out);
    assert!(text.contains("exit code: 0"), "{out:?}");
    assert!(!marker.exists(), "hardened git must not run diff.external");
    assert!(!text.contains("fatal"), "the diff still succeeds: {out:?}");
}

/// A hardened `git diff` must not run a `diff.<driver>.command` external driver: it becomes
/// `true`, so the command still succeeds while the patch for those paths is suppressed
/// (verified on git 2.53.0: an empty override would error out instead).
#[tokio::test]
async fn untrusted_shell_disables_repo_diff_command() {
    let dir = TempDir::new("shell-git-command");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-command");
    let driver = ws.join("driver-hook.sh");
    marker_script(&driver, &marker, false);
    git(
        &ws,
        &["config", "diff.dc.command", &driver.to_string_lossy()],
    );
    std::fs::write(ws.join(".gitattributes"), "*.dcf diff=dc\n").unwrap();
    std::fs::write(ws.join("g.dcf"), "data\n").unwrap();
    git(&ws, &["add", ".gitattributes", "g.dcf"]);

    // Control: without hardening the driver fires on `git diff`.
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    exec(
        &session,
        "shell",
        json!({"command": "git diff --cached -- g.dcf"}),
    )
    .await
    .unwrap();
    assert!(marker.exists(), "the control must fire the diff driver");

    // Hardened: the same diff succeeds without running the driver.
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    let out = exec(
        &session,
        "shell",
        json!({"command": "git diff --cached -- g.dcf"}),
    )
    .await
    .unwrap();
    let text = output_text(&out);
    assert!(text.contains("exit code: 0"), "{out:?}");
    assert!(
        !marker.exists(),
        "hardened git must not run diff.<name>.command"
    );
}

/// An approved `outside_sandbox` call in an untrusted project carries the same hardening:
/// the approval names the command, not the repository's config.
#[tokio::test]
async fn untrusted_outside_sandbox_shell_does_not_run_repo_fsmonitor() {
    let dir = TempDir::new("shell-git-outside");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-fsmonitor-outside");
    let hook = ws.join("fsmonitor-hook.sh");
    marker_script(&hook, &marker, false);
    git(&ws, &["config", "core.fsmonitor", &hook.to_string_lossy()]);

    async fn status_outside(session: &CodingSession) -> String {
        let out = exec(
            session,
            "shell",
            json!({
                "command": "git status --porcelain --branch",
                "outside_sandbox": true,
                "outside_sandbox_reason": "test the hardening outside the sandbox",
            }),
        )
        .await
        .unwrap();
        output_text(&out)
    }

    // Control: without hardening the trap fires, sandbox or not.
    let plain = tools(&ws, &dir.file("sessions/plain.jobs"), false);
    let session = plain.open_session().await.expect("session");
    let text = status_outside(&session).await;
    assert!(text.contains("## "), "{text}");
    assert!(marker.exists(), "the control must fire the hook");

    // Hardened: the same approved call succeeds without running the hook.
    std::fs::remove_file(&marker).unwrap();
    let hardened = tools(&ws, &dir.file("sessions/hardened.jobs"), true);
    let session = hardened.open_session().await.expect("session");
    let text = status_outside(&session).await;
    assert!(text.contains("## "), "{text}");
    assert!(
        !marker.exists(),
        "a hardened outside_sandbox call must not run core.fsmonitor"
    );
}
/// A trusted project injects nothing: the user's own `pre-commit` hook still runs on a
/// `git commit` through `shell`.
#[tokio::test]
async fn trusted_shell_runs_the_user_pre_commit_hook() {
    let dir = TempDir::new("shell-git-trusted");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    repo(&ws);

    let marker = dir.file("marker-pre-commit");
    let hooks = ws.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    marker_script(&hooks.join("pre-commit"), &marker, false);

    // Hardening off (trusted): the hook runs and the commit lands.
    let trusted = tools(&ws, &dir.file("sessions/trusted.jobs"), false);
    let session = trusted.open_session().await.expect("session");
    std::fs::write(ws.join("c.txt"), "new\n").unwrap();
    let out = exec(
        &session,
        "shell",
        json!({"command": "git add c.txt && git commit -qm trusted"}),
    )
    .await
    .unwrap();
    assert!(marker.exists(), "a trusted shell runs the pre-commit hook");
    assert!(output_text(&out).contains("trusted") || output_text(&out).is_empty());
    let log = std::process::Command::new("git")
        .current_dir(&ws)
        .args(["log", "--format=%s", "-1"])
        .output()
        .expect("git log");
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("trusted"),
        "the commit landed"
    );
}

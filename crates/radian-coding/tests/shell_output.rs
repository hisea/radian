//! A truncated **foreground** `shell` result registers as a settled entry in the same job
//! registry under a normal job id (ADR 0037 decision 3a): `job { op: read }` reads it, the id
//! is never a path, a path-like or cross-session id is `unknown_job`, and retention is
//! explicitly bounded.
//!
//! Retention needs a **caller-supplied private directory** (the host passes the session's jobs
//! directory). A provider built with `with_sandbox` but no `with_jobs` has no safe place for it:
//! a workspace directory would be readable by the model's `read`, so no entry is registered and
//! no file is written. The tests below pin both the enabled and the disabled path.

mod common;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use radian_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::{CodingSession, CodingTools, Jobs, Limits};
use radian_sandbox::{
    ProbeReport, Sandbox, SandboxBackend, SandboxCommand, SandboxError, SandboxMode, SandboxPolicy,
};
use serde_json::{Value, json};

fn full_access(ws: &Path) -> Sandbox {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    Sandbox::from_policy(policy).expect("full-access needs no backend")
}

/// The host path: the provider is given the session's own (0700, outside the workspace) jobs
/// directory, so foreground retention is enabled.
fn host_tools(ws: &Path, jobs_dir: &Path, limits: Limits) -> CodingTools {
    // The host creates the session directory tree before the session opens; the private root is
    // created one level, so its parent must exist (and is never model-writable).
    if let Some(parent) = jobs_dir.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    CodingTools::with_sandbox(ws, full_access(ws))
        .expect("matching workspaces")
        .with_jobs(Arc::new(Jobs::new(jobs_dir)))
        .with_limits(limits)
}

/// The no-host path: `with_sandbox` but no session-private directory.
fn no_host_tools(ws: &Path, limits: Limits) -> CodingTools {
    CodingTools::with_sandbox(ws, full_access(ws))
        .expect("matching workspaces")
        .with_limits(limits)
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

fn structured(output: &ToolOutput) -> Value {
    output.structured.clone().expect("structured output")
}

/// Every `.out` file under `root`, recursively.
fn out_files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "out") {
                out.push(path);
            }
        }
    }
    out
}

/// The default caps used by the retrieval tests: a small model-facing view, so the middle is
/// genuinely discarded, with the default retained bounds.
fn small_view() -> Limits {
    Limits {
        default_shell_output_bytes: 1024,
        max_shell_output_bytes: 1024,
        ..Limits::default()
    }
}

/// A capped foreground command exposes a job id, and `job` reaches output
/// the head+tail view dropped.
#[tokio::test]
async fn a_truncated_command_exposes_a_retrievable_job_id() {
    let dir = TempDir::new("shell-output-handle");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &jobs, small_view());
    let session = tools.open_session().await.expect("session");

    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let value = structured(&output);
    let id = value["job_id"]
        .as_str()
        .expect("a job id for a truncated result")
        .to_owned();
    assert!(
        !id.contains('/') && !id.contains('\\'),
        "the id is opaque, not a path: {id}"
    );
    assert_eq!(value["output_retained_cut"], json!(false), "{value}");
    assert!(
        value["output_retained_bytes"].as_u64().unwrap() > 1024,
        "the whole output is retained: {value}"
    );
    let text = output_text(&output);
    assert!(text.contains("`job read`"), "{text}");
    assert!(text.contains(&id), "{text}");

    // A line the head+tail view discarded (the tail starts around line 4850).
    let found = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "pattern": "^2500$"}),
    )
    .await
    .unwrap();
    assert!(
        output_text(&found).contains("2500: 2500"),
        "{}",
        output_text(&found)
    );

    // Tail and line-range views.
    let tail = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 3}),
    )
    .await
    .unwrap();
    assert!(
        output_text(&tail).contains("5000: 5000"),
        "{}",
        output_text(&tail)
    );
    let range = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "from_line": 1, "max_lines": 2}),
    )
    .await
    .unwrap();
    assert!(
        output_text(&range).starts_with("1: 1\n2: 2\n"),
        "{}",
        output_text(&range)
    );
}

/// An id is scoped to the session that produced it; another session's call is `unknown_job`.
#[tokio::test]
async fn an_id_from_another_session_is_unknown() {
    let dir = TempDir::new("shell-output-cross");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &dir.file("sessions/sid.jobs"), small_view());
    let first = tools.open_session().await.expect("first session");
    let output = exec(&first, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let id = structured(&output)["job_id"]
        .as_str()
        .expect("a job id")
        .to_owned();

    // Another session (its own registry, like the host builds per session) does not know it.
    let other = host_tools(&ws, &dir.file("sessions/other.jobs"), small_view());
    let second = other.open_session().await.expect("second session");
    let failure = exec(
        &second,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 1}),
    )
    .await
    .expect_err("another session's id must be refused");
    assert_eq!(failure.code, "unknown_job", "{failure:?}");
}

/// A path-like input is a filesystem path attempt, never a file open: it is `unknown_job`, and
/// the file's contents never come back.
#[tokio::test]
async fn a_path_like_id_cannot_open_a_file() {
    let dir = TempDir::new("shell-output-path");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &jobs, small_view());
    let session = tools.open_session().await.expect("session");

    let outside = dir.file("outside.txt");
    std::fs::write(&outside, "SECRET-CONTENT\n").unwrap();
    let absolute = outside.to_string_lossy().into_owned();
    for bad in [absolute.as_str(), "../outside.txt", "out-1.out", "job-1-1"] {
        let failure = exec(
            &session,
            "job",
            json!({"id": bad, "op": "read", "tail_lines": 1}),
        )
        .await
        .expect_err("a path is not an id");
        assert_eq!(failure.code, "unknown_job", "{bad}: {failure:?}");
        assert!(
            !failure.message.contains("SECRET-CONTENT"),
            "{bad}: {failure:?}"
        );
    }
}

/// The private directory/path never appears in the shell result or in the retrieval result, and
/// no retained file ever lands in the workspace.
#[tokio::test]
async fn the_retained_path_is_never_revealed() {
    let dir = TempDir::new("shell-output-secret");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &jobs, small_view());
    let session = tools.open_session().await.expect("session");

    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let value = structured(&output);
    let id = value["job_id"].as_str().expect("a job id").to_owned();

    let private = jobs.to_string_lossy().into_owned();
    assert!(!value.to_string().contains(&private), "{value}");
    assert!(!output_text(&output).contains(&private));

    let read = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 2}),
    )
    .await
    .unwrap();
    assert!(!structured(&read).to_string().contains(&private));
    assert!(!output_text(&read).contains(&private));

    // The retained file is outside the workspace, so the model's `read` cannot reach it by any
    // workspace-relative path derived from the id.
    assert!(
        out_files_under(&ws).is_empty(),
        "no retained output file may live under the workspace"
    );
    let derived = format!(".radian/jobs/{id}.out");
    let failure = exec(&session, "read", json!({"path": derived}))
        .await
        .expect_err("a workspace path must not reach the retained file");
    assert!(
        !failure.message.contains("2500"),
        "the retained content must not come back: {failure:?}"
    );
}

/// The retained file is bounded per item, and when the cap cuts it the shell result says so and
/// does not promise the lost bytes back.
#[tokio::test]
async fn the_retained_file_is_capped_and_says_so() {
    let dir = TempDir::new("shell-output-cap");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(
        &ws,
        &jobs,
        Limits {
            default_shell_output_bytes: 64,
            max_shell_output_bytes: 64,
            max_foreground_retained_bytes: 200,
            ..Limits::default()
        },
    );
    let session = tools.open_session().await.expect("session");

    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let value = structured(&output);
    assert_eq!(value["output_retained_cut"], json!(true), "{value}");
    let retained = value["output_retained_bytes"].as_u64().unwrap();
    let total = value["output_total_bytes"].as_u64().unwrap();
    assert!(retained <= 200, "the per-item cap bounds the file: {value}");
    assert!(total > retained, "{value}");
    assert!(
        output_text(&output).contains("not recoverable"),
        "a cut must be stated: {}",
        output_text(&output)
    );

    let id = value["job_id"].as_str().expect("a job id");
    let read = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "from_line": 1}),
    )
    .await
    .unwrap();
    assert!(
        !output_text(&read).is_empty(),
        "the retained bytes are still readable"
    );
}

/// The session byte cap evicts the oldest entry, whose id then reads as unknown.
#[tokio::test]
async fn the_session_cap_evicts_the_oldest_entry() {
    let dir = TempDir::new("shell-output-evict");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(
        &ws,
        &jobs,
        Limits {
            default_shell_output_bytes: 64,
            max_shell_output_bytes: 64,
            max_foreground_retained_bytes: 512,
            max_foreground_session_bytes: 1024,
            max_foreground_items: 32,
            ..Limits::default()
        },
    );
    let session = tools.open_session().await.expect("session");

    let mut ids = Vec::new();
    for _ in 0..3 {
        let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
            .await
            .unwrap();
        ids.push(
            structured(&output)["job_id"]
                .as_str()
                .expect("a job id")
                .to_owned(),
        );
    }

    let failure = exec(
        &session,
        "job",
        json!({"id": ids[0], "op": "read", "tail_lines": 1}),
    )
    .await
    .expect_err("the oldest entry was evicted");
    assert_eq!(failure.code, "unknown_job", "{failure:?}");

    let ok = exec(
        &session,
        "job",
        json!({"id": ids[2], "op": "read", "tail_lines": 1}),
    )
    .await
    .expect("the newest item is still there");
    assert!(!output_text(&ok).is_empty());
}

/// A command whose output fits the view registers no entry and leaves no file behind.
#[tokio::test]
async fn a_non_truncated_command_leaves_no_entry_or_file() {
    let dir = TempDir::new("shell-output-none");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &jobs, Limits::default());
    let session = tools.open_session().await.expect("session");

    let output = exec(&session, "shell", json!({"command": "echo hello"}))
        .await
        .unwrap();
    let value = structured(&output);
    assert_eq!(value["job_id"], json!(null), "{value}");
    assert!(!output_text(&output).contains("`job read`"));
    assert_eq!(
        out_files_under(&jobs).len(),
        0,
        "no retained file for an untruncated command"
    );
}

/// `job` read without a range reads the increment (plus `[status]`); no read mode is
/// required.
#[tokio::test]
async fn a_plain_read_returns_the_increment() {
    let dir = TempDir::new("shell-output-no-read");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &jobs, small_view());
    let session = tools.open_session().await.expect("session");
    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let id = structured(&output)["job_id"]
        .as_str()
        .expect("a job id")
        .to_owned();

    let found = exec(&session, "job", json!({"id": id, "op": "read"}))
        .await
        .expect("a plain read returns the increment");
    assert!(
        output_text(&found).contains("[status:"),
        "{}",
        output_text(&found)
    );
}

/// Session shutdown drops the registry and removes the retained files.
#[tokio::test]
async fn shutdown_removes_the_retained_files() {
    let dir = TempDir::new("shell-output-shutdown");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = host_tools(&ws, &jobs, small_view());
    let session = tools.open_session().await.expect("session");
    exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();

    assert!(
        !out_files_under(&jobs).is_empty(),
        "a truncated command keeps its file"
    );
    session.shutdown_outputs();
    assert_eq!(
        out_files_under(&jobs).len(),
        0,
        "shutdown removes the retained files"
    );
}

// ---------- the no-host provider: there is no safe private root ----------

/// `CodingTools::with_sandbox` without a caller-supplied private directory must not retain
/// anything under the workspace (the model's `read` could reach it). The ordinary
/// truncation notice still tells the model bytes were discarded.
#[tokio::test]
async fn the_no_host_provider_retains_nothing_in_the_workspace() {
    let dir = TempDir::new("shell-output-no-host");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = no_host_tools(&ws, small_view());
    let session = tools.open_session().await.expect("session");

    // A hand-built call to the removed name is an unknown tool, never a path open.
    let failure = exec(&session, "shell_output", json!({"handle": "out-0-1"}))
        .await
        .expect_err("the removed name is unknown");
    assert_eq!(failure.code, "unknown_tool", "{failure:?}");

    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let value = structured(&output);
    assert_eq!(value["job_id"], json!(null), "{value}");
    assert!(
        value["stdout_truncated_bytes"].as_u64().unwrap() > 0,
        "the view is still truncated and says so: {value}"
    );
    assert!(
        out_files_under(&ws).is_empty(),
        "a no-host provider must not leave a readable output file under the workspace"
    );
}

// ---------- the private root is symlink-proof ----------

/// A pre-planted symlink at the private root cannot redirect the retained output: the registry
/// refuses the root, so no entry exists and nothing is written through the link.
#[cfg(unix)]
#[tokio::test]
async fn a_planted_root_symlink_cannot_redirect_output() {
    let dir = TempDir::new("shell-output-symlink");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.file("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let outside = dir.file("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), "UNCHANGED\n").unwrap();

    let jobs = sessions.join("sid.jobs");
    std::os::unix::fs::symlink(&outside, &jobs).unwrap();

    let tools = host_tools(&ws, &jobs, small_view());
    let session = tools.open_session().await.expect("session");
    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();

    assert_eq!(structured(&output)["job_id"], json!(null));
    let entries: Vec<String> = std::fs::read_dir(&outside)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["sentinel".to_owned()],
        "nothing may be written through a symlinked root: {entries:?}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("sentinel")).unwrap(),
        "UNCHANGED\n"
    );
}

// ---------- an error path cleans the retention file ----------

/// A backend whose probe passes but whose command cannot be built: the shell call fails after
/// the retention file was created, and the cleanup guard must remove it.
struct SpawnFailingBackend;

static SPAWN_FAILING: SpawnFailingBackend = SpawnFailingBackend;

impl SandboxBackend for SpawnFailingBackend {
    fn name(&self) -> &'static str {
        "spawn-failing"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        Ok(ProbeReport {
            backend: self.name(),
            detail: "test".into(),
        })
    }

    fn command(
        &self,
        _policy: &SandboxPolicy,
        _program: &OsStr,
        _args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError> {
        Err(SandboxError::BackendUnavailable {
            backend: self.name(),
            detail: "the test backend refuses to build the command".into(),
        })
    }
}

#[tokio::test]
async fn a_spawn_failure_leaves_no_retained_file() {
    let dir = TempDir::new("shell-output-error");
    let ws = dir.file("ws");
    let jobs = dir.file("sessions/sid.jobs");
    std::fs::create_dir_all(&ws).unwrap();
    // The session directory tree exists (the host creates it), so the retention file really is
    // created before the spawn fails; the test then proves the cleanup guard removed it.
    std::fs::create_dir_all(jobs.parent().unwrap()).unwrap();

    let sandbox = Sandbox::from_backend(&SPAWN_FAILING, SandboxMode::WorkspaceWrite, &ws)
        .expect("the test probe passes");
    let tools = CodingTools::with_sandbox(&ws, sandbox)
        .expect("matching workspaces")
        .with_jobs(Arc::new(Jobs::new(&jobs)))
        .with_limits(small_view());
    let session = tools.open_session().await.expect("session");

    let failure = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .expect_err("the command cannot be built");
    assert_eq!(failure.code, "sandbox_unavailable", "{failure:?}");
    assert_eq!(
        out_files_under(&jobs).len(),
        0,
        "a failed call must not leave its retention file behind"
    );
}

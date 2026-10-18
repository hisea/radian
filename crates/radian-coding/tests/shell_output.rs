//! `shell_output`: a truncated **foreground** `shell` result carries an opaque, session-scoped
//! handle through which the retained output can be read. The handle is never a path, a path-like
//! or cross-session handle is `unknown_output`, and retention is explicitly bounded.

mod common;

use std::path::Path;

use agent_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::{CodingSession, CodingTools, Limits};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::{Value, json};

fn transparent_tools(ws: &Path, limits: Limits) -> CodingTools {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full-access needs no backend");
    CodingTools::with_sandbox(ws, sandbox)
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

/// The default caps used by the retrieval tests: a small model-facing view, so the middle is
/// genuinely discarded, with the default retained bounds.
fn small_view() -> Limits {
    Limits {
        default_shell_output_bytes: 1024,
        max_shell_output_bytes: 1024,
        ..Limits::default()
    }
}

/// A capped foreground command exposes a non-path handle, and the retrieval tool reaches output
/// the head+tail view dropped.
#[tokio::test]
async fn a_truncated_command_exposes_a_retrievable_handle() {
    let dir = TempDir::new("shell-output-handle");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, small_view());
    let session = tools.open_session().await.expect("session");

    // The tool is advertised only with `shell`.
    assert!(
        session
            .tool_specs()
            .iter()
            .any(|spec| spec.name == "shell_output"),
        "shell_output is advertised with shell"
    );

    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let value = structured(&output);
    let handle = value["output_handle"]
        .as_str()
        .expect("a handle for a truncated result")
        .to_owned();
    assert!(
        !handle.contains('/') && !handle.contains('\\'),
        "the handle is opaque, not a path: {handle}"
    );
    assert_eq!(value["output_retained_cut"], json!(false), "{value}");
    assert!(
        value["output_retained_bytes"].as_u64().unwrap() > 1024,
        "the whole output is retained: {value}"
    );
    let text = output_text(&output);
    assert!(text.contains("shell_output"), "{text}");
    assert!(text.contains(&handle), "{text}");

    // A line the head+tail view discarded (the tail starts around line 4850).
    let found = exec(
        &session,
        "shell_output",
        json!({"handle": handle, "pattern": "^2500$"}),
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
        "shell_output",
        json!({"handle": handle, "tail_lines": 3}),
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
        "shell_output",
        json!({"handle": handle, "from_line": 1, "max_lines": 2}),
    )
    .await
    .unwrap();
    assert!(
        output_text(&range).starts_with("1: 1\n2: 2\n"),
        "{}",
        output_text(&range)
    );
}

/// A handle is scoped to the session that produced it; another session's call is `unknown_output`.
#[tokio::test]
async fn a_handle_from_another_session_is_unknown() {
    let dir = TempDir::new("shell-output-cross");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, small_view());
    let first = tools.open_session().await.expect("first session");
    let output = exec(&first, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let handle = structured(&output)["output_handle"]
        .as_str()
        .expect("a handle")
        .to_owned();

    let second = tools.open_session().await.expect("second session");
    let failure = exec(
        &second,
        "shell_output",
        json!({"handle": handle, "tail_lines": 1}),
    )
    .await
    .expect_err("another session's handle must be refused");
    assert_eq!(failure.code, "unknown_output", "{failure:?}");
}

/// A path-like input is a filesystem path attempt, never a file open: it is `unknown_output`, and
/// the file's contents never come back.
#[tokio::test]
async fn a_path_like_handle_cannot_open_a_file() {
    let dir = TempDir::new("shell-output-path");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, small_view());
    let session = tools.open_session().await.expect("session");

    let outside = dir.file("outside.txt");
    std::fs::write(&outside, "SECRET-CONTENT\n").unwrap();
    let absolute = outside.to_string_lossy().into_owned();
    for bad in [absolute.as_str(), "../outside.txt", "foreground/out-1.out"] {
        let failure = exec(
            &session,
            "shell_output",
            json!({"handle": bad, "tail_lines": 1}),
        )
        .await
        .expect_err("a path is not a handle");
        assert_eq!(failure.code, "unknown_output", "{bad}: {failure:?}");
        assert!(
            !failure.message.contains("SECRET-CONTENT"),
            "{bad}: {failure:?}"
        );
    }
}

/// The private directory/path never appears in the shell result or in the retrieval result.
#[tokio::test]
async fn the_retained_path_is_never_revealed() {
    let dir = TempDir::new("shell-output-secret");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, small_view());
    let session = tools.open_session().await.expect("session");

    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let value = structured(&output);
    let handle = value["output_handle"]
        .as_str()
        .expect("a handle")
        .to_owned();

    let foreground_dir = ws.join(".radian").join("jobs").join("foreground");
    let needle = foreground_dir.to_string_lossy().into_owned();
    assert!(!value.to_string().contains(&needle), "{value}");
    assert!(!output_text(&output).contains(&needle));

    let read = exec(
        &session,
        "shell_output",
        json!({"handle": handle, "tail_lines": 2}),
    )
    .await
    .unwrap();
    assert!(!structured(&read).to_string().contains(&needle));
    assert!(!output_text(&read).contains(&needle));
}

/// The retained file is bounded per item, and when the cap cuts it the shell result says so and
/// does not promise the lost bytes back.
#[tokio::test]
async fn the_retained_file_is_capped_and_says_so() {
    let dir = TempDir::new("shell-output-cap");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(
        &ws,
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

    let handle = value["output_handle"].as_str().expect("a handle");
    let read = exec(
        &session,
        "shell_output",
        json!({"handle": handle, "from_line": 1}),
    )
    .await
    .unwrap();
    assert!(
        !output_text(&read).is_empty(),
        "the retained bytes are still readable"
    );
}

/// The session byte cap evicts the oldest item, whose handle then reads as unknown.
#[tokio::test]
async fn the_session_cap_evicts_the_oldest_handle() {
    let dir = TempDir::new("shell-output-evict");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(
        &ws,
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

    let mut handles = Vec::new();
    for _ in 0..3 {
        let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
            .await
            .unwrap();
        handles.push(
            structured(&output)["output_handle"]
                .as_str()
                .expect("a handle")
                .to_owned(),
        );
    }

    let failure = exec(
        &session,
        "shell_output",
        json!({"handle": handles[0], "tail_lines": 1}),
    )
    .await
    .expect_err("the oldest item was evicted");
    assert_eq!(failure.code, "unknown_output", "{failure:?}");

    let ok = exec(
        &session,
        "shell_output",
        json!({"handle": handles[2], "tail_lines": 1}),
    )
    .await
    .expect("the newest item is still there");
    assert!(!output_text(&ok).is_empty());
}

/// A command whose output fits the view creates no handle and leaves no file behind.
#[tokio::test]
async fn a_non_truncated_command_leaves_no_handle_or_file() {
    let dir = TempDir::new("shell-output-none");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, Limits::default());
    let session = tools.open_session().await.expect("session");

    let output = exec(&session, "shell", json!({"command": "echo hello"}))
        .await
        .unwrap();
    let value = structured(&output);
    assert_eq!(value["output_handle"], json!(null), "{value}");
    assert!(!output_text(&output).contains("shell_output"));

    let foreground_dir = ws.join(".radian").join("jobs").join("foreground");
    let leftovers = std::fs::read_dir(&foreground_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "out"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "no retained file for an untruncated command");
}

/// `shell_output` needs a read; without one it is an argument error, not an empty read.
#[tokio::test]
async fn a_read_is_required() {
    let dir = TempDir::new("shell-output-no-read");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, small_view());
    let session = tools.open_session().await.expect("session");
    let output = exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();
    let handle = structured(&output)["output_handle"]
        .as_str()
        .expect("a handle")
        .to_owned();

    let failure = exec(&session, "shell_output", json!({"handle": handle}))
        .await
        .expect_err("no read");
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
}

/// Session shutdown drops the registry and removes the retained files.
#[tokio::test]
async fn shutdown_removes_the_retained_files() {
    let dir = TempDir::new("shell-output-shutdown");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws, small_view());
    let session = tools.open_session().await.expect("session");
    exec(&session, "shell", json!({"command": "seq 1 5000"}))
        .await
        .unwrap();

    let foreground_dir = ws.join(".radian").join("jobs").join("foreground");
    assert!(
        std::fs::read_dir(&foreground_dir)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0)
            > 0,
        "a truncated command keeps its file"
    );
    session.shutdown_outputs();
    let leftovers = std::fs::read_dir(&foreground_dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "shutdown removes the retained files");
}

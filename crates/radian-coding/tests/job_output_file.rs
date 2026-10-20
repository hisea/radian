//! `job`'s file arguments (ADR 0039 decision 1 review fix): the incremental ring is
//! bounded, so the model pages the **full** output file by line with `tail_lines`,
//! `from_line`/`max_lines` or `pattern`.
//!
//! A `FullAccess` sandbox (no backend needed) drives a real job that prints more lines than the
//! 64 KiB ring can hold, so every assertion below fails if the tool only reads the ring.

mod common;

use std::path::Path;

use agent_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput,
    ToolProvider, ToolSession,
};
use common::TempDir;
use radian_coding::CodingSession;
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::{Value, json};

/// The test job's line count: enough that the early lines are long gone from the 64 KiB ring.
const LINES: u64 = 200_000;

fn transparent_tools(ws: &Path) -> radian_coding::CodingTools {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full-access needs no backend");
    radian_coding::CodingTools::with_sandbox(ws, sandbox).expect("matching workspaces")
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

/// Runs a background job and returns once it has settled, so the output file is complete.
async fn run_to_end(session: &CodingSession, command: &str) -> String {
    let started = exec(
        session,
        "shell",
        json!({"command": command, "background": true}),
    )
    .await
    .expect("start the job");
    let job_id = structured(&started)["job_id"]
        .as_str()
        .expect("a job id")
        .to_owned();
    let settled = exec(
        session,
        "job",
        json!({"id": job_id, "op": "wait", "timeout_ms": 60_000}),
    )
    .await
    .expect("wait for the job");
    let text = output_text(&settled);
    assert!(text.contains("[status: exited 0]"), "{text}");
    job_id
}

/// A 200k-line job: `tail_lines 5`, `from_line 1 max_lines 3` and `pattern` all read the file,
/// not the ring.
#[tokio::test]
async fn the_file_arguments_read_the_full_output_not_the_ring() {
    let dir = TempDir::new("job-file-read");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws);
    let session = tools.open_session().await.expect("session");

    let id = run_to_end(&session, &format!("seq 1 {LINES}")).await;

    // The last five lines, numbered.
    let tail = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 5}),
    )
    .await
    .expect("tail");
    let tail_text = output_text(&tail);
    let value = structured(&tail);
    assert_eq!(value["total_lines"], json!(LINES), "{value}");
    assert_eq!(value["mode"], json!("tail"), "{value}");
    assert_eq!(value["start_line"], json!(LINES - 4), "{value}");
    assert_eq!(value["end_line"], json!(LINES), "{value}");
    for line in (LINES - 4)..=LINES {
        assert!(
            tail_text.contains(&format!("{line}: {line}\n")),
            "line {line} missing:\n{tail_text}"
        );
    }
    assert!(tail_text.contains(&format!("showing lines {}-{LINES}", LINES - 4)));

    // The first three lines, which the ring dropped.
    let head = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "from_line": 1, "max_lines": 3}),
    )
    .await
    .expect("head");
    let head_text = output_text(&head);
    assert!(head_text.starts_with("1: 1\n2: 2\n3: 3\n"), "{head_text}");
    assert!(head_text.contains("showing lines 1-3"), "{head_text}");
    assert!(head_text.contains("from_line=4"), "next line: {head_text}");

    // A line that only exists in the dropped part.
    let found = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "pattern": "^1234$"}),
    )
    .await
    .expect("pattern");
    let found_text = output_text(&found);
    assert!(found_text.contains("1234: 1234\n"), "{found_text}");
    assert_eq!(structured(&found)["matches"], json!(1), "{found_text}");

    // A tail larger than the ring's window still starts at the file's own line number.
    let wide = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 100_000}),
    )
    .await
    .expect("wide tail");
    let wide_text = output_text(&wide);
    assert!(
        wide_text.starts_with("100001: 100001\n"),
        "a wide tail must start at the file's line 100001, not the ring's window:\n{wide_text}"
    );

    session.shutdown_jobs();
}

/// An id that is not a registry key is an `unknown_job`, never a path the tool opens.
#[tokio::test]
async fn a_job_id_with_path_characters_is_refused() {
    let dir = TempDir::new("job-file-id");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws);
    let session = tools.open_session().await.expect("session");

    for bad in ["../job-1.out", "job-1/../../x", ".."] {
        let failure = exec(
            &session,
            "job",
            json!({"id": bad, "op": "read", "from_line": 1}),
        )
        .await
        .expect_err("a path-like id must be refused");
        assert_eq!(failure.code, "unknown_job", "{bad}: {failure:?}");
    }
}

/// The file arguments are mutually exclusive, and an out-of-range value is a named argument
/// error rather than a silent default.
#[tokio::test]
async fn the_file_arguments_are_validated() {
    let dir = TempDir::new("job-file-args");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws);
    let session = tools.open_session().await.expect("session");
    let id = run_to_end(&session, "echo hi").await;

    let failure = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 2, "pattern": "hi"}),
    )
    .await
    .expect_err("two modes");
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");

    let failure = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "from_line": 0}),
    )
    .await
    .expect_err("from_line 0");
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");

    let failure = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "pattern": "("}),
    )
    .await
    .expect_err("bad regex");
    assert_eq!(failure.code, "invalid_regex", "{failure:?}");

    session.shutdown_jobs();
}

/// GPT fills every optional field (seen 2026-10-17: `pattern: ""`, `tail_lines: 1`,
/// `from_line: 1`, `max_lines: 300`). `op: "read"` with the range fields reads the file;
/// a blank `pattern` is no pattern; two real range forms at once is `invalid_arguments`.
#[tokio::test]
async fn filled_optional_fields_still_read() {
    let dir = TempDir::new("job-mode");
    let ws = dir.file("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let tools = transparent_tools(&ws);
    let session = tools.open_session().await.expect("session");
    let id = run_to_end(&session, "printf 'one\\ntwo\\nthree\\n'").await;

    // Plain read: the incremental read (already drained by run_to_end), not a file read.
    let new = exec(&session, "job", json!({"id": id, "op": "read"}))
        .await
        .expect("plain read");
    let text = output_text(&new);
    assert!(text.contains("[no new output]"), "{text}");
    assert!(text.contains("[status: exited 0]"), "{text}");

    // `tail_lines: 1` with a blank pattern and a range: the blank pattern is dropped
    // (`args::parse` drops an optional `""`), so this is a tail read.
    let tail = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "tail_lines": 1, "pattern": ""}),
    )
    .await
    .expect("tail");
    let text = output_text(&tail);
    assert!(text.contains("three"), "{text}");
    assert!(!text.contains("one"), "only the last line: {text}");

    // `from_line`/`max_lines` reads the range.
    let range = exec(
        &session,
        "job",
        json!({"id": id, "op": "read", "from_line": 1, "max_lines": 300}),
    )
    .await
    .expect("range");
    let text = output_text(&range);
    assert!(text.contains("one") && text.contains("three"), "{text}");

    session.shutdown_jobs();
}

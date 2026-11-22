//! End-to-end tests of The session commands: sessions on disk, `--list`, `--search`, `--export`, `--fork` and `--resume`.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::{Value, json};
use std::path::Path;
use support::*;

// ---------- sessions on disk (Phase 2 / B) ----------

/// 1: a new session writes a valid, contiguous JSONL with the runtime's events.
#[tokio::test]
async fn a_new_session_writes_a_valid_jsonl() {
    let sessions = fresh_session_dir();
    let server = MockServer::start(vec![text_reply("hi")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-one",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hello",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let path = sessions.join("s-one.jsonl");
    let session = radian_session::Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (0..seqs.len() as u64).collect::<Vec<_>>(),
        "seq is contiguous"
    );
    for event in [
        "SessionCreated",
        "InboxEnqueued",
        "InputClaimed",
        "InputAdmitted",
        "TurnFinished",
    ] {
        assert!(
            session.events().iter().any(|e| e.event == event),
            "missing {event} in {:?}",
            session
                .events()
                .iter()
                .map(|e| &e.event)
                .collect::<Vec<_>>()
        );
    }
    // The one-shot turn is on disk, not only in memory.
    assert_eq!(session.history().unwrap().len(), 2);
    // The terminal input is recorded as human input (ADR 0011 L2): the permission layer
    // relies on this value, so it must not be `None` or some other source.
    let enqueued = session
        .events()
        .iter()
        .find(|e| e.event == "InboxEnqueued")
        .expect("the input was enqueued");
    match &enqueued.data {
        radian_session::EventData::InboxEnqueued { source, .. } => {
            assert_eq!(*source, radian_core::InputSource::User)
        }
        other => panic!("expected InboxEnqueued, got {other:?}"),
    }
}

/// 2: `--resume` puts the previous history in front of the new request.
#[tokio::test]
async fn resume_carries_the_previous_history() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-resume",
                "--session-dir",
                dir,
                "-p",
                "first",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--resume",
                "s-resume",
                "--session-dir",
                dir,
                "-p",
                "second",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let second = messages_of(&server, 1);
    assert_eq!(last_user_text(&second), "second");
    let texts: Vec<&str> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect();
    assert!(
        texts.contains(&"first"),
        "the resumed request lacks the old input: {second}"
    );
    assert!(
        texts.contains(&"one"),
        "the resumed request lacks the old answer: {second}"
    );
}

/// 4: `--list` shows only the sessions of the current workspace.
#[tokio::test]
async fn list_shows_only_the_current_workspace() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    // The workspaces must be siblings of the session directory: a workspace inside it is
    // refused (ADR 0020), and this test is about `--list`, not the sandbox.
    let name = sessions.file_name().unwrap().to_string_lossy().into_owned();
    let ws_a = sessions.with_file_name(format!("{name}-ws-a"));
    let ws_b = sessions.with_file_name(format!("{name}-ws-b"));
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let server = MockServer::start(vec![text_reply("a"), text_reply("b")]).await;

    for (id, workspace) in [("a-1", &ws_a), ("b-1", &ws_b)] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    workspace.to_str().unwrap(),
                    "--session-id",
                    id,
                    "--session-dir",
                    dir,
                    "-p",
                    "hi",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
    }

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--workspace",
                ws_a.to_str().unwrap(),
                "--session-dir",
                dir,
                "--list",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("a-1"), "{stdout}");
    assert!(
        !stdout.contains("b-1"),
        "another workspace leaked in: {stdout}"
    );
    // The row carries id, created, last activity and a message count.
    let row = stdout.lines().find(|l| l.contains("a-1")).unwrap();
    assert_eq!(row.split('\t').count(), 4, "{row}");
    assert_eq!(row.split('\t').nth(3).unwrap(), "2", "{row}");
}

/// 5: reasoning is printed apart from the visible answer.
#[tokio::test]
async fn reasoning_is_printed_apart_from_the_answer() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(
            json!({"role": "assistant", "reasoning_content": "thinking hard"}),
            None,
        ),
        chunk(json!({"content": "the answer"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "the answer\n", "stdout carries only the answer");
    assert!(stderr.contains("[reasoning]"), "{stderr}");
    assert!(stderr.contains("thinking hard"), "{stderr}");
    assert!(
        !stdout.contains("thinking hard"),
        "reasoning leaked to stdout"
    );
}

/// 6: a second process cannot open a session another process holds.
#[tokio::test]
async fn a_second_process_cannot_open_the_same_session() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;

    let mut first = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "s-lock",
            "--session-dir",
            dir,
        ],
        &[],
    );
    first.write("go\n").await;
    wait_until("the model request", || server.requests().len() == 1).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--resume",
                "s-lock",
                "--session-dir",
                dir,
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("locked") || stderr.contains("another writer"),
        "{stderr}"
    );

    // Release the first process and let it leave.
    sigint(&first.child);
    first.wait_for_stderr("Cancelled").await;
    first.write("/exit\n").await;
    let (code, _, _) = first.finish().await;
    assert_eq!(code, 0);
}

/// 7: the session file carries neither the API key nor a header value.
#[tokio::test]
async fn the_session_file_contains_no_key_or_header_values() {
    let sessions = fresh_session_dir();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-secret",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--header",
                "x-team-token=file-secret-value",
                "-p",
                "hi",
            ],
            &[("RADIAN_EXEC_FILE_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let body = std::fs::read_to_string(sessions.join("s-secret.jsonl")).unwrap();
    assert!(!body.contains(KEY), "the key reached the session file");
    assert!(
        !body.contains("file-secret-value"),
        "a header value reached the session file"
    );
    assert!(!body.contains("Authorization"), "{body}");
}

/// 3: the process is killed mid-turn (`kill -9`), and `--resume` recovers: the unpaired tool
/// is classified and **not** replayed, and the input that was claimed but never admitted
/// comes back instead of being lost.
#[tokio::test]
async fn a_killed_session_resumes_without_replaying_the_tool() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    let scratch = scratch("kill-resume");
    let log = scratch.join("fixture.log");
    let config = mcp_config(&scratch, &log);
    let path = sessions.join("s-kill.jsonl");
    // `fx__sleep` ignores cancellation, so the turn is genuinely mid-tool when the process
    // dies; the fixture has no `FIXTURE_IGNORE_EOF`, so killing the CLI releases it.
    // The resumed session has a pending steer, which (ADR 0011) opens a turn by
    // itself as soon as the driver runs — before the `-p next` followup. The three replies
    // cover the killed turn's tool call and whichever of the resumed turns runs first.
    let server = MockServer::start(vec![
        tool_reply("call_1", "fx__sleep", "{\"ms\": 30000}"),
        text_reply("done"),
        text_reply("done"),
    ])
    .await;

    let mut first = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--yes",
            "--mcp-config",
            config.to_str().unwrap(),
            "--session-id",
            "s-kill",
            "--session-dir",
            dir,
        ],
        &[],
    );
    first.write("go\n").await;
    wait_until("the tool call to start", || {
        std::fs::read_to_string(&log)
            .map(|text| text.contains("tools/call"))
            .unwrap_or(false)
    })
    .await;
    // With `--yes` the loop reads during the turn, so this line becomes a steer. Wait until
    // it is durably enqueued (a second InboxEnqueued) before killing. The claim now happens
    // at the step boundary — the driver's wake branch only handles cancellation — so a steer
    // that arrives mid-step is requeued from its InboxEnqueued record on recovery.
    first.write("steer-line\n").await;
    wait_until("the steer to be enqueued", || {
        std::fs::read_to_string(&path)
            .map(|text| text.matches("\"InboxEnqueued\"").count() >= 2)
            .unwrap_or(false)
    })
    .await;

    // The genuine crash: SIGKILL, no chance to flush anything.
    first.child.kill().await.unwrap();
    let _ = first.child.wait().await;
    let pid = fixture_pid(&log);
    assert!(
        gone_within(pid, STATE_WATCHDOG).await,
        "the fixture survived the killed CLI"
    );

    // What the crash left: the tool started, never settled.
    let crashed = std::fs::read_to_string(&path).unwrap();
    assert!(crashed.contains("\"ToolExecutionStarted\""), "{crashed}");
    assert!(!crashed.contains("\"ToolExecutionSettled\""), "{crashed}");
    assert_eq!(
        crashed.matches("tools/call").count(),
        0,
        "the session file must not contain fixture logs"
    );
    let calls_before = log_lines(&log)
        .iter()
        .filter(|line| line.contains("tools/call"))
        .count();
    assert_eq!(calls_before, 1);

    // Resume: the repaired result is written back, the steer is requeued and applied, and
    // the tool is not run again.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--yes",
                "--mcp-config",
                config.to_str().unwrap(),
                "--resume",
                "s-kill",
                "--session-dir",
                dir,
                "-p",
                "next",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let session = radian_session::Session::open(&path).unwrap();
    let history = session.history().unwrap();
    let texts: Vec<String> = history.iter().map(radian_core::Message::text).collect();
    assert!(
        texts.contains(&"steer-line".to_owned()),
        "the claimed steer was lost: {texts:?}"
    );
    let repaired = history.iter().find_map(|m| match m {
        radian_core::Message::Tool { result, .. } if result.call_id == "call_1" => Some(result),
        _ => None,
    });
    assert_eq!(
        repaired.and_then(|r| r.error_code.as_deref()),
        Some("outcome_unknown_after_crash"),
        "the unpaired call was not classified: {history:?}"
    );
    // The tool was not replayed: still exactly one fixture call, in the first process.
    let calls_after = log_lines(&log)
        .iter()
        .filter(|line| line.contains("tools/call"))
        .count();
    assert_eq!(calls_after, 1, "the sleep was replayed on resume");
    // The steer was admitted in the resumed turn, not merely left in the queue.
    let events = std::fs::read_to_string(&path).unwrap();
    assert!(
        events.matches("\"InputAdmitted\"").count() >= 2,
        "the requeued steer was never admitted: {events}"
    );
}

// ---------- session export and search (--export / --search) ----------

/// Acceptance 1: `--export` renders the conversation as Markdown by default and as JSON with
/// `--format json`.
#[tokio::test]
async fn export_renders_markdown_and_json() {
    let dir = scratch("export-basic");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("REPLY-TEXT-ONE")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "e1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "USER-TEXT-ONE",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
        "--export",
        "e1",
    ];
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("# Session e1"), "{stdout}");
    assert!(stdout.contains("USER-TEXT-ONE"), "{stdout}");
    assert!(stdout.contains("REPLY-TEXT-ONE"), "{stdout}");

    let mut json_args = base.to_vec();
    json_args.extend_from_slice(&["--format", "json"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&json_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let parsed: Value = serde_json::from_str(&stdout).expect("the JSON export is valid JSON");
    assert_eq!(parsed["session_id"], "e1");
    assert!(stdout.contains("USER-TEXT-ONE"), "{stdout}");
    assert!(stdout.contains("REPLY-TEXT-ONE"), "{stdout}");
}

/// Acceptance 2: reasoning and provider payload are omitted by default and included only
/// with their switches.
#[tokio::test]
async fn export_omits_reasoning_and_provider_payload_by_default() {
    let dir = scratch("export-optin");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "optin",
        &canonical(&ws),
        "VISIBLE-ANSWER",
        Some("SECRET-REASONING"),
        Some(r#"{"marker":"PAYLOAD-MARKER"}"#),
    );
    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
        "--export",
        "optin",
    ];

    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("VISIBLE-ANSWER"), "{stdout}");
    assert!(!stdout.contains("SECRET-REASONING"), "{stdout}");
    assert!(!stdout.contains("PAYLOAD-MARKER"), "{stdout}");

    let mut with_reasoning = base.to_vec();
    with_reasoning.push("--include-reasoning");
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&with_reasoning, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SECRET-REASONING"), "{stdout}");
    assert!(!stdout.contains("PAYLOAD-MARKER"), "{stdout}");

    let mut with_payload = base.to_vec();
    with_payload.push("--include-provider-payload");
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&with_payload, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PAYLOAD-MARKER"), "{stdout}");
    assert!(!stdout.contains("SECRET-REASONING"), "{stdout}");
}

/// Acceptance 3: the process's key and every `--header` value are redacted from the export.
#[tokio::test]
async fn export_redacts_the_key_and_header_values() {
    let dir = scratch("export-secrets");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "sec",
        &canonical(&ws),
        "the key sk-cli-test-secret and the header hdr-value-xyz",
        None,
        None,
    );
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--export",
                "sec",
                "--header",
                "X-Test=hdr-value-xyz",
            ],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains(KEY), "the key leaked: {stdout}");
    assert!(
        !stdout.contains("hdr-value-xyz"),
        "the header leaked: {stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
}

/// Acceptance 4: a session that belongs to another workspace is refused by name.
#[tokio::test]
async fn export_refuses_another_workspaces_session() {
    let dir = scratch("export-other-ws");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws_a = dir.join("ws-a");
    let ws_b = dir.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    write_session(&sessions, "in-b", &canonical(&ws_b), "text", None, None);
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws_a.to_str().unwrap(),
                "--export",
                "in-b",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("not in this workspace"), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
}

/// ADR 0006 §4: `--resume` applies the same workspace rule as `--export` and ACP's
/// `session/load`. A session created in another workspace is refused by name, and the journal
/// is not written.
#[tokio::test]
async fn resume_refuses_another_workspaces_session() {
    // `common::process_dir`, not `scratch`: the sandbox cannot create `/tmp` directories, and
    // this test opens no Unix socket (the reason `scratch` uses `/tmp`).
    let dir = common::process_dir("radian-exec-resume-other-ws");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws_a = dir.join("ws-a");
    let ws_b = dir.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let session = write_session(&sessions, "from-a", &canonical(&ws_a), "reply", None, None);
    let before = std::fs::read(&session).unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws_b.to_str().unwrap(),
                "--resume",
                "from-a",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("from-a") && stderr.contains("not in this workspace"),
        "the error names the session and the rule: {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "a refused resume writes nothing to the journal"
    );
}

/// Acceptance 5: `--out` writes a file, refuses an existing one, and `--force` overwrites it.
#[tokio::test]
async fn export_out_refuses_to_overwrite_without_force() {
    let dir = scratch("export-out");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(&sessions, "out", &canonical(&ws), "OUT-TEXT", None, None);
    let out = dir.join("export.md");
    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
        "--export",
        "out",
        "--out",
        out.to_str().unwrap(),
    ];

    let (code, _, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let first = std::fs::read_to_string(&out).unwrap();
    assert!(first.contains("OUT-TEXT"), "{first}");

    let (code, _, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), first);

    std::fs::write(&out, "OLD").unwrap();
    let mut forced = base.to_vec();
    forced.push("--force");
    let (code, _, stderr) = feed_and_wait(spawn_radian(&forced, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert_ne!(std::fs::read_to_string(&out).unwrap(), "OLD");
}

/// Acceptance 6: `--search` prints one line per hit and reports "no match" on stderr with a
/// zero exit when nothing matches.
#[tokio::test]
async fn search_finds_text_and_reports_no_match() {
    let dir = scratch("search");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("SEARCHABLE-MARKER here")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "s1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "find me",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    let mut hit_args = base.to_vec();
    hit_args.extend_from_slice(&["--search", "SEARCHABLE-MARKER"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&hit_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let row = stdout.lines().next().unwrap();
    assert!(row.starts_with("s1\t"), "{row}");
    assert_eq!(row.split('\t').count(), 3, "{row}");
    assert!(row.contains("SEARCHABLE-MARKER"), "{row}");

    let mut miss_args = base.to_vec();
    miss_args.extend_from_slice(&["--search", "NO-SUCH-TEXT-XYZ"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&miss_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("no match"), "{stderr}");
}

/// Acceptance 7: the session commands write nothing into the session directory (not even an
/// index cache; `--search` builds one in the system temp directory).
#[tokio::test]
async fn session_commands_do_not_touch_the_session_directory() {
    let dir = scratch("session-readonly");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("KEEP-MARKER")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "keep",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "keep me",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let before = jsonl_snapshot(&sessions);

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    let mut export_args = base.to_vec();
    export_args.extend_from_slice(&["--export", "keep"]);
    let (code, _, stderr) = feed_and_wait(spawn_radian(&export_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let mut search_args = base.to_vec();
    search_args.extend_from_slice(&["--search", "KEEP-MARKER"]);
    let (code, _, stderr) = feed_and_wait(spawn_radian(&search_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");

    assert_eq!(
        jsonl_snapshot(&sessions),
        before,
        "a session command created or modified a .jsonl file"
    );
    // The search cache (`index.db`) may be created or updated: it is disposable, not a
    // session (ADR 0006 §10).
    assert!(
        dir_names(&sessions).iter().any(|name| name == "index.db"),
        "this test no longer exercises the shared index cache"
    );
}

/// The session commands are their own mode: they cannot be combined with a prompt or a
/// conversation, which is a clap usage error (exit 2).
#[tokio::test]
async fn session_commands_conflict_with_a_prompt() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&["--model", "m", "--export", "x", "-p", "hi"], &[]),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

/// `--model` is not required for the session-management commands: none of them starts a
/// conversation or reads the key.
#[tokio::test]
async fn session_commands_do_not_need_a_model() {
    let dir = scratch("no-model");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "nm",
        &canonical(&ws),
        "NO-MODEL-TEXT",
        None,
        None,
    );
    let base = [
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];

    let mut list_args = base.to_vec();
    list_args.push("--list");
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&list_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("nm"), "{stdout}");

    let mut export_args = base.to_vec();
    export_args.extend_from_slice(&["--export", "nm"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&export_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("NO-MODEL-TEXT"), "{stdout}");

    let mut search_args = base.to_vec();
    search_args.extend_from_slice(&["--search", "NO-MODEL-TEXT"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&search_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("nm"), "{stdout}");
}

/// A conversation still requires a model: with no `--model` and no `model.model` in the
/// config file, `-p` and interactive mode are usage errors (exit 2, before any session is
/// created). The decision moved from clap to `setup` so the config file can supply it.
/// ADR 0008 §2: the message names the config file path and the `--provider <p> --model <m>`
/// form.
#[tokio::test]
async fn a_conversation_still_requires_a_model() {
    // The setup check runs **before** the API-key lookup, and its wording is unique: asserting
    // only `--model` would also be satisfied by the later "no API key" error, so a mutation
    // that dropped the model requirement would slip past. Assert the sentence itself.
    let (code, _, stderr) = feed_and_wait(spawn_radian(&["-p", "hi"], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("no model: pass --provider <p> --model <m>"),
        "{stderr}"
    );

    let (code, _, stderr) = feed_and_wait(spawn_radian(&[], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("no model: pass --provider <p> --model <m>"),
        "{stderr}"
    );
}

/// ADR 0008 §2: a fresh `RADIAN_HOME` gets a commented default config on startup; the
/// no-model failure names its path. A second run leaves it untouched (bytes and mode), and
/// an explicit `--config` path that does not exist is never created.
#[tokio::test]
async fn first_launch_creates_a_default_config_and_names_it() {
    let dir = scratch("first-launch");
    let home = dir.0.join("home");
    let expected = home.join("config/config.toml");
    // First run: the file is created, the message names it and the `--provider/--model` form.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&["-p", "hi"], &[("RADIAN_HOME", home.to_str().unwrap())]),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "created a default config at {}",
            expected.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "no model: pass --provider <p> --model <m>, or set model.provider/model in {}",
            expected.display()
        )),
        "{stderr}"
    );
    assert!(expected.is_file(), "the default config was created");
    let before = std::fs::read(&expected).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&expected).unwrap().permissions().mode() & 0o777,
            0o600,
            "the fresh config is owner-only"
        );
        assert_eq!(
            std::fs::metadata(expected.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "the config directory is owner-only"
        );
    }
    // The template parses to the default config (every key commented out).
    let parsed = radian_host::parse_config(&expected, &std::fs::read_to_string(&expected).unwrap())
        .expect("the template must parse");
    assert_eq!(parsed.model, radian_host::ModelConfig::default());
    // Second run: the file is not touched.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&["-p", "hi"], &[("RADIAN_HOME", home.to_str().unwrap())]),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        !stderr.contains("created a default config"),
        "an existing file is never rewritten: {stderr}"
    );
    assert_eq!(
        std::fs::read(&expected).unwrap(),
        before,
        "not one byte changes"
    );
    // An explicit missing path is never created (a typo must not leave a file).
    let missing = dir.0.join("custom.toml");
    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &["-p", "hi", "--config", missing.to_str().unwrap()],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(
        !missing.exists(),
        "an explicit missing path is never created"
    );
}

/// `--export` validates the id as a session id before using it as a file name: a path-like
/// value must not be able to read a `.jsonl` outside the session directory. The forged file
/// one level up claims the current workspace, so the workspace check alone would let it
/// through; the id validation is what refuses it.
#[tokio::test]
async fn export_refuses_a_path_like_session_id() {
    let dir = scratch("export-path-id");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // `dir/x.jsonl` is `sessions/../x.jsonl`; its workspace is the current one on purpose.
    write_session(
        &dir,
        "x",
        &canonical(&ws),
        "OUTSIDE-THE-SESSION-DIR",
        None,
        None,
    );
    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    for bad in ["../x", "a/b", ".."] {
        let mut args = base.to_vec();
        args.extend_from_slice(&["--export", bad]);
        let (code, stdout, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 2, "{bad:?}: {stderr}");
        assert!(stderr.contains("session id"), "{bad:?}: {stderr}");
        assert!(stdout.is_empty(), "{bad:?}: {stdout}");
        assert!(
            !stderr.contains("OUTSIDE-THE-SESSION-DIR"),
            "{bad:?}: the error leaked file content: {stderr}"
        );
    }
}

/// An export does not know which provider wrote the session, so without `--api-key-env` both
/// providers' default key variables are named secrets. The key here has no recognizable
/// pattern (unlike `KEY`'s `sk-` prefix), so only a named secret can remove it.
#[tokio::test]
async fn export_redacts_the_anthropic_key_without_api_key_env() {
    let opaque = "opaque-anthropic-credential-7f3a";
    let dir = scratch("export-anthropic-secret");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "anth",
        &canonical(&ws),
        &format!("the anthropic key {opaque} appears here"),
        None,
        None,
    );
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--export",
                "anth",
            ],
            &[("ANTHROPIC_API_KEY", opaque)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stdout.contains(opaque),
        "the anthropic key leaked: {stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
}

// ---------- session fork (ADR 0022) ----------

/// Writes a parent with one finished turn: `PARENT-TEXT` as the first user message. Returns
/// the `TurnFinished` seq (the fork boundary the CLI should default to).
async fn write_turn_session(sessions: &Path, id: &str, workspace: &str) -> u64 {
    use radian_core::{JournalRecord, Message, TranscriptJournal};
    let path = sessions.join(format!("{id}.jsonl"));
    let journal = radian_session::SessionJournal::create(
        &path,
        id,
        radian_session::SessionCreatedData::new(workspace, json!({})),
    )
    .unwrap();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnStarted {
            turn_id: "t-1".into(),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("PARENT-TEXT"),
        })
        .await
        .unwrap();
    let seq = journal.next_seq();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnFinished {
            turn_id: "t-1".into(),
            outcome: radian_session::TurnOutcome::Completed,
            reason: None,
            stop_reason: None,
        })
        .await
        .unwrap();
    drop(journal);
    seq
}

/// `radian exec --fork` creates a child with the parent's history, records the lineage in both
/// files, and refuses a parent another process holds.
#[tokio::test]
async fn fork_creates_a_child_and_refuses_a_locked_parent() {
    let dir = scratch("fork-cli");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = canonical(&ws);
    let boundary = write_turn_session(&sessions, "parent", &workspace).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                // No --model: --fork is a session command.
                "--fork",
                "parent",
                "--session-id",
                "child",
                "--session-dir",
                sessions.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains(&format!("forked parent at seq {boundary} into child")),
        "{stdout}"
    );
    assert!(stdout.contains("--resume child"), "{stdout}");

    // The child's initial history is the parent's one turn.
    let child = std::fs::read_to_string(sessions.join("child.jsonl")).unwrap();
    assert!(
        child.contains(&format!(
            "\"parent\":{{\"session_id\":\"parent\",\"seq\":{boundary},\"relation\":\"fork\"}}"
        )),
        "{child}"
    );
    assert!(child.contains("PARENT-TEXT"), "{child}");

    // The parent gained exactly one SessionForked with the same seq.
    let parent = std::fs::read_to_string(sessions.join("parent.jsonl")).unwrap();
    assert_eq!(parent.matches("\"event\":\"SessionForked\"").count(), 1);
    assert!(
        parent.contains(&format!(
            "\"child_session_id\":\"child\",\"parent_seq\":{boundary}"
        )),
        "{parent}"
    );

    // A parent another process is writing must be refused with exit 1, not waited for.
    let held =
        radian_session::SessionJournal::resume(sessions.join("parent.jsonl"), "parent").unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--fork",
                "parent",
                "--session-id",
                "child-2",
                "--session-dir",
                sessions.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("being written by another process"),
        "{stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert!(!sessions.join("child-2.jsonl").exists());
    drop(held);
}

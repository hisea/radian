//! End-to-end tests of `radian exec` itself: streaming, exit codes, usage errors, approval, interactive mode, signals, built-in tools, the sandbox and permission layer.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_anthropic::testing as anth;
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::*;

// ---------- streaming and exit codes ----------

#[tokio::test]
async fn one_shot_streams_text_to_stdout_only() {
    let server = MockServer::start(vec![text_reply("Hello there")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "Hello there\n");
    assert!(!stderr.contains("Hello there"), "{stderr}");
    // The first message is the system prompt (the skill list is in it); the user's words are
    // the user message.
    assert_eq!(last_user_text(&messages_of(&server, 0)), "hi");
}

#[tokio::test]
async fn a_model_error_exits_one() {
    let server = MockServer::start(vec![Reply::status(
        401,
        "{\"error\":{\"message\":\"bad key\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("HTTP 401"), "{stderr}");
}

/// ADR 0032: the diagnostics say `radian: …`, not `agent: …` (the binary is
/// gone; `radian exec` is the headless command). Every failure line carries the prefix.
#[tokio::test]
async fn diagnostics_carry_the_radian_prefix() {
    let server = MockServer::start(vec![Reply::status(
        401,
        "{\"error\":{\"message\":\"bad key\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.lines().any(|line| line.starts_with("radian: ")),
        "a diagnostic line starts with `radian: `: {stderr}"
    );
    assert!(
        !stderr.contains("agent: "),
        "no `agent: ` line remains: {stderr}"
    );
}

// ---------- usage and configuration errors ----------

#[tokio::test]
async fn usage_errors_exit_two() {
    let (code, _, _) = feed_and_wait(spawn_radian(&["-p", "hi"], &[]), "").await; // no --model
    assert_eq!(code, 2);
}

/// ADR 0018 removed `--context-threshold`; passing it is a usage error (exit 2), never a
/// silently ignored flag.
#[tokio::test]
async fn the_removed_context_threshold_flag_is_an_error() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--context-threshold",
                "80",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--context-threshold"), "{stderr}");
}

#[tokio::test]
async fn bad_mcp_config_exits_two_with_the_path() {
    let dir = scratch("config");

    // Unknown field.
    let unknown = write_file(&dir, "unknown.json", "{\"servers\": [], \"bogus\": 1}");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "-p",
                "hi",
                "--mcp-config",
                unknown.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains(unknown.to_str().unwrap()), "{stderr}");

    // Missing file.
    let missing = dir.join("no-such-file.json");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "-p",
                "hi",
                "--mcp-config",
                missing.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains(missing.to_str().unwrap()), "{stderr}");

    // Duplicate ids, caught before anything starts.
    let dup = write_file(
        &dir,
        "dup.json",
        &json!({"servers": [
            {"id": "fx", "command": fixture_bin()},
            {"id": "fx", "command": fixture_bin()}
        ]})
        .to_string(),
    );
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "-p",
                "hi",
                "--mcp-config",
                dup.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains(dup.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("duplicate"), "{stderr}");
}

// ---------- tool approval ----------

#[tokio::test]
async fn answering_y_approves_and_the_tool_runs() {
    let dir = scratch("approve-y");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                config.to_str().unwrap(),
                // MCP asks under `workspace`; `full` allows it (ADR 0012/0043).
                "--profile",
                "workspace",
            ],
            &[],
        ),
        "go\ny\n/exit\n",
    )
    .await;

    assert_eq!(code, 0);
    assert!(stderr.contains("[y/N]"), "{stderr}");
    assert_eq!(server.requests().len(), 2);
    // The echo fixture returns its arguments; the approval let it run.
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

#[tokio::test]
async fn answering_n_denies_the_call() {
    let dir = scratch("approve-n");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("fine")).await;

    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                config.to_str().unwrap(),
                "--profile",
                "workspace",
            ],
            &[],
        ),
        "go\nn\n/exit\n",
    )
    .await;

    assert_eq!(code, 0);
    let denial = tool_result_content(&server, 1);
    assert!(denial.contains("denied"), "{denial}");
    assert!(!denial.contains(r#"{"hello":"mcp"}"#), "{denial}");
}

#[tokio::test]
async fn yes_flag_allows_without_asking() {
    let dir = scratch("yes");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--mcp-config",
                config.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert!(!stderr.contains("[y/N]"), "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

#[tokio::test]
async fn one_shot_without_a_terminal_denies_without_waiting() {
    let dir = scratch("deny");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("fine")).await;

    // stdin is a pipe that stays open and that nobody writes to: the CLI must deny at once
    // because it has no terminal, not because it ran into end of input. Closing stdin here
    // would let an approval that does try to read look the same.
    let running = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "go",
            "--mcp-config",
            config.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );
    let (code, _, _) = running
        .finish_open(
            EXIT_WATCHDOG,
            "no input was written; the denial must not wait for EOF",
        )
        .await;

    assert_eq!(code, 0);
    let denial = tool_result_content(&server, 1);
    assert!(denial.contains("no terminal"), "{denial}");
    assert!(denial.contains("--yes"), "{denial}");
}

// ---------- interactive mode ----------

#[tokio::test]
async fn interactive_sessions_keep_history_and_eof_exits_cleanly() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, stdout, _) = feed_and_wait(
        spawn_radian(&["--model", "m", "--base-url", &server.base_url], &[]),
        "first\nsecond\n", // EOF, no /exit
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "one\ntwo\n");
    let second = messages_of(&server, 1);
    assert_eq!(last_user_text(&second), "second");
    // The first round-trip is still in the conversation.
    let texts: Vec<&str> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect();
    assert!(texts.contains(&"first"), "{second}");
    assert!(texts.contains(&"one"), "{second}");
}

#[tokio::test]
async fn system_prompt_is_sent() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--system",
                "Be brief.",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    assert_eq!(system["role"], "system");
    let content = system["content"].as_str().unwrap_or_default();
    // ADR 0036: `--system` replaces the role layer only. Layer 0a `<runtime>` is
    // first and layer 0b `<session>` is last; the skills section (ADR 0025) sits between them.
    assert!(content.starts_with("<runtime>"), "{content}");
    assert!(
        content.contains("<role>\nBe brief.\n</role>"),
        "the role layer is the --system text: {content}"
    );
    assert!(content.contains("## Skills (ADR 0025)"), "{content}");
    assert!(
        content.trim_end().ends_with("</session>"),
        "layer 0b is last: {content}"
    );
    assert!(content.contains("model: m"), "{content}");
}

/// ADR 0036: without `--system` the built-in `default` role is layer 1, between the
/// `<runtime>` layer and the context files, with the `<session>` layer last.
#[tokio::test]
async fn the_default_role_layers_are_sent() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    // A private home: the mounted tools must not depend on the machine's real home.
    let home = scratch("default-role");
    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0);
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    let content = system["content"].as_str().unwrap_or_default();
    assert!(content.starts_with("<runtime>"), "{content}");
    let runtime_at = content.find("<runtime>").unwrap();
    let role_at = content.find("<role>").unwrap();
    let session_at = content.find("<session>").unwrap();
    assert!(runtime_at < role_at && role_at < session_at, "{content}");
    assert!(
        content.contains("You are radian, a coding agent"),
        "the default role: {content}"
    );
    // ADR 0021 (removed): no memory section, no memory rules.
    assert!(!content.contains("<memory>"), "{content}");
    assert!(!content.contains("Write a memory file"), "{content}");
    assert!(content.trim_end().ends_with("</session>"), "{content}");
    assert!(content.contains("model: m"), "{content}");
}

/// The `<runtime>` Unix-socket sentence follows the backend: Linux bwrap denies every
/// Unix socket in `workspace` (slice 0's seccomp filter), so the sentence is present;
/// macOS seatbelt does too, so the sentence stays exactly as today. Setting the flag
/// from the tier again fails the Linux half.
#[tokio::test]
async fn the_workspace_system_prompt_unix_socket_sentence_follows_the_backend() {
    let dir = scratch("unix-sockets-backend");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    let content = system["content"].as_str().unwrap_or_default();
    if cfg!(target_os = "linux") {
        assert!(
            content.contains("Outbound Unix socket connections are denied"),
            "bwrap denies every Unix socket in workspace, so the sentence is present: {content}"
        );
    } else {
        assert!(
            content.contains("Outbound Unix socket connections are denied"),
            "seatbelt restricts Unix sockets, so the sentence stays: {content}"
        );
    }
}

/// The `full` tier has no sandbox and a reachable network (ADR 0016 decision 1, ADR
/// 0043 decision 5), so its `<runtime>` facts say so in plain sentences.
#[tokio::test]
async fn the_full_system_prompt_states_network_on() {
    let dir = scratch("full-network-on");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "full",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    let content = system["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("There is no sandbox."),
        "full has no sandbox: {content}"
    );
    assert!(
        content.contains("The network is on."),
        "full has a reachable network: {content}"
    );
    assert!(
        !content.contains("Network: off."),
        "no stale off flag beside the no-sandbox facts: {content}"
    );
}

/// The removed tier name is unknown: `--profile safe` fails like any typo (fail closed),
/// and never runs the model.
#[tokio::test]
async fn profile_safe_is_rejected_as_an_unknown_tier() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--profile",
                "safe",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_ne!(code, 0, "an unknown tier must fail: {stderr}");
    assert!(
        stderr.contains("'safe'"),
        "the refusal must name the problem: {stderr}"
    );
    assert!(
        server.requests().is_empty(),
        "the model must never run for an unknown tier"
    );
}

#[tokio::test]
async fn turn_timeout_ends_the_turn() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let start = Instant::now();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--turn-timeout-secs",
                "1",
            ],
            &[],
        ),
        "",
    )
    .await;

    // A turn that hits the deadline ends Cancelled(DeadlineExceeded): exit 3 (ADR 0044).
    assert_eq!(code, 3, "a deadline-exceeded turn exits 3: {stderr}");
    assert!(stderr.contains("DeadlineExceeded"), "{stderr}");
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "{:?}",
        start.elapsed()
    );
}

// ---------- Ctrl-C ----------

#[tokio::test]
async fn first_sigint_cancels_the_turn_and_the_session_survives() {
    let server = MockServer::start(vec![
        Reply::SseThenHang { pieces: Vec::new() },
        text_reply("recovered"),
    ])
    .await;
    let mut child = Running::spawn(&["--model", "m", "--base-url", &server.base_url], &[]);

    child.write("go\n").await;
    // The recorded request proves the turn is hanging on the model, and the interrupt
    // task has been waiting since before that — a signal now cannot land at the prompt.
    wait_until("the model request", || server.requests().len() == 1).await;
    sigint(&child.child);
    child.write("again\n/exit\n").await;

    let (code, stdout, stderr) = child.finish().await;

    assert_eq!(code, 0);
    assert!(stderr.contains("cancelled"), "{stderr}");
    // The second turn really ran, on the same session.
    assert_eq!(server.requests().len(), 2);
    assert_eq!(stdout, "recovered\n");
    assert_eq!(last_user_text(&messages_of(&server, 1)), "again");
}

#[tokio::test]
async fn second_sigint_exits_130() {
    // Load note: the two signals are separated only by a pipe round-trip (the marker
    // wait below), so this test is the load detector for the signal path. A listener
    // that re-subscribes after every signal loses the second Ctrl-C when the
    // forwarding task is preempted in the gap — the turn still ends Cancelled and the
    // loop idles forever — about one hung iteration in seven with six busy loops and
    // this test pinned to the same four cores. Validate signal-handling changes with
    // that pinning and `--stress-count 300`: the fixed listener trips zero.
    let dir = scratch("130");
    let log = dir.join("fixture.log");
    let config = mcp_config(&dir, &log);
    // `freeze` blocks the fixture's reading thread, so after the first Ctrl-C the client
    // waits out its full 2s MCP cancel grace for a ping that can never be answered: a real,
    // bounded wind-down window for the second Ctrl-C.
    let server = MockServer::start(vec![tool_reply("call_1", "fx__freeze", "{}")]).await;
    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--yes",
            "--mcp-config",
            config.to_str().unwrap(),
        ],
        &[],
    );
    let start = Instant::now();

    child.write("go\n").await;
    // The fixture log proves the tool call is active before the first signal.
    wait_until("the fixture to receive tools/call", || {
        std::fs::read_to_string(&log)
            .map(|text| text.contains("tools/call"))
            .unwrap_or(false)
    })
    .await;
    sigint(&child.child);
    // State gate, not a timer: wait until the CLI reports the wind-down, which proves the
    // first Ctrl-C was handled while the turn is still winding down (the MCP cancel grace
    // keeps it there), then send the second one.
    child.wait_for_stderr("Ctrl-C again to quit").await;
    let forced_at = Instant::now();
    sigint(&child.child);

    // Keep stdin open while waiting: closing it would race an EOF against the second signal,
    // and this test is about the signal, not about EOF.
    let (code, _, stderr) = child
        .finish_open(
            EXIT_WATCHDOG,
            "the second SIGINT was already sent after the wind-down marker",
        )
        .await;

    assert_eq!(code, 130, "{stderr}");
    // The second signal forces the exit; it does not wait out the MCP cancel grace (2s).
    assert!(
        forced_at.elapsed() < Duration::from_millis(1000),
        "the forced exit waited instead of forcing: {:?}",
        forced_at.elapsed()
    );
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "{:?}",
        start.elapsed()
    );
    // Even the forced exit reaped the fixture.
    assert!(gone_within(fixture_pid(&log), STATE_WATCHDOG).await);
}

// ---------- child process lifetime ----------

#[tokio::test]
async fn every_exit_reaps_the_mcp_children() {
    let dir = scratch("reap");
    for (name, input, sigint_at_prompt) in [
        ("exit", "hi\n/exit\n", false),
        ("eof", "hi\n", false),
        ("sigint", "", true),
    ] {
        let log = dir.join(format!("{name}.log"));
        // This fixture does not leave when its stdin closes. A server that did would vanish
        // when the CLI process dies, whatever the CLI did, and the test could not tell.
        let config = mcp_config_with(&dir, &log, &[("FIXTURE_IGNORE_EOF", "1")], Some(1));
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let args = [
            "--model".to_owned(),
            "m".to_owned(),
            "--base-url".to_owned(),
            server.base_url.clone(),
            "--mcp-config".to_owned(),
            config.display().to_string(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _) = if sigint_at_prompt {
            // Wait until the prompt is really up, then leave with Ctrl-C.
            let mut running = Running::spawn(&arg_refs, &[]);
            running.wait_for_stderr("> ").await;
            sigint(&running.child);
            let (code, _, _) = running.finish().await;
            (code, ())
        } else {
            let (code, _, _) = feed_and_wait(spawn_radian(&arg_refs, &[]), input).await;
            (code, ())
        };
        assert_eq!(code, 0, "{name}");

        let pid = fixture_pid(&log);
        assert!(
            gone_within(pid, STATE_WATCHDOG).await,
            "{name}: fixture {pid} still running"
        );
    }
}

#[tokio::test]
async fn a_normal_exit_lets_each_server_shut_down_before_the_cli_leaves() {
    let dir = scratch("graceful");
    for (name, input) in [("exit", "hi\n/exit\n"), ("eof", "hi\n")] {
        let log = dir.join(format!("{name}.log"));
        // The server takes 300 ms to finish after its stdin closes. If the CLI killed it, or
        // just left, the marker would be missing (or arrive after the CLI is gone).
        let config = mcp_config_with(&dir, &log, &[("FIXTURE_SLOW_SHUTDOWN_MS", "300")], None);
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let args = [
            "--model".to_owned(),
            "m".to_owned(),
            "--base-url".to_owned(),
            server.base_url.clone(),
            "--mcp-config".to_owned(),
            config.display().to_string(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let (code, _, _) = feed_and_wait(spawn_radian(&arg_refs, &[]), input).await;

        assert_eq!(code, 0, "{name}");
        let lines = log_lines(&log);
        assert!(
            lines.contains(&"shutdown-complete".to_owned()),
            "{name}: {lines:?}"
        );
    }
}

#[tokio::test]
async fn typing_exit_leaves_even_though_stdin_is_still_open() {
    // A terminal never reaches end of input. The CLI must not wait for one more line after
    // `/exit`: the stdin reader's blocking read cannot be interrupted, so exiting has to
    // abandon it instead of waiting for it.
    let server = MockServer::start(vec![text_reply("hello")]).await;
    let mut running = Running::spawn(&["--model", "m", "--base-url", &server.base_url], &[]);

    running.write("hi\n/exit\n").await;
    let (code, stdout, _) = running
        .finish_open(EXIT_WATCHDOG, "/exit was written; stdin stayed open")
        .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "hello\n");
}

// ---------- approval must not eat input ----------

#[tokio::test]
async fn cancelling_at_the_approval_prompt_does_not_swallow_the_next_line() {
    let dir = scratch("swallow");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("ok")).await;
    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--mcp-config",
            config.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );

    child.write("go\n").await;
    // The request alone is not enough: it was sent before `before_tool_call`. The approval
    // prompt on stderr proves the turn is really waiting for an answer.
    wait_until("the tool request", || server.requests().len() == 1).await;
    child.wait_for_stderr("[y/N]").await;
    sigint(&child.child);
    // Wait until the CLI has actually cancelled before typing: the next line must belong
    // to the next prompt, not race the dying approval for it.
    child.wait_for_stderr("Cancelled").await;
    child.write("next\n/exit\n").await;

    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    // The line after the cancelled turn reached the model, not a stranded read.
    assert_eq!(server.requests().len(), 2, "{stderr}");
    assert_eq!(last_user_text(&messages_of(&server, 1)), "next", "{stderr}");
}

// ---------- built-in coding tools ----------

/// One-shot: the model asks for the built-in `write`, then answers.
#[tokio::test]
async fn built_in_write_tool_creates_a_file_in_the_workspace() {
    let dir = scratch("builtin-write");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"hello\n"}"#,
        ),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "write a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "hello\n"
    );
    // The tool result went back to the model.
    assert!(
        tool_result_content(&server, 1).contains("wrote 6 bytes"),
        "{}",
        tool_result_content(&server, 1)
    );
}

#[tokio::test]
async fn built_in_read_outside_the_workspace_is_refused_and_the_turn_continues() {
    let dir = scratch("builtin-escape");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "read", r#"{"path":"/etc/passwd"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "read a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "the turn continues: {stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("absolute path") && result.contains("/etc/passwd"),
        "the model sees the refusal: {result}"
    );
}

/// ADR 0039 decision 4: the `full` tier reads an outside path through the file tools. The
/// model reads a scratch file outside the workspace by absolute path, and the turn
/// continues with its content.
#[tokio::test]
async fn built_in_read_outside_the_workspace_reads_in_the_full_tier() {
    let dir = scratch("builtin-full-read");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let outside = dir.join("outside.txt");
    std::fs::write(&outside, "FULL-TIER-SENTINEL\n").unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "read",
            &format!(
                r#"{{"path":{}}}"#,
                serde_json::to_string(outside.to_str().unwrap()).unwrap()
            ),
        ),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "read a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "the turn continues: {stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("FULL-TIER-SENTINEL"),
        "the model sees the outside file: {result}"
    );
}

#[tokio::test]
async fn without_workspace_the_built_in_tools_do_not_exist() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"x","content":"y"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--yes",
                "-p",
                "write a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("no tool named"),
        "without --workspace there is no `write`: {result}"
    );
}

// ---------- profiles, the sandbox, and the permission layer (Phase 3 / cli-tools) ----------

/// 1 (ADR 0043): `full` **is** the default, and without `--workspace` no write/execute tool
/// exists at all. The startup line says `no sandbox`; putting the default back to `workspace`
/// is the mutation this catches.
#[tokio::test]
async fn full_is_the_default_and_no_write_tools_without_a_workspace() {
    let server = MockServer::start(vec![
        tool_reply("c1", "write", r#"{"path":"x.txt","content":"y"}"#),
        tool_reply("c2", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--yes",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    // The default is named on startup, and it is `full` with no sandbox and no prompts.
    assert!(stderr.contains("[profile] full"), "{stderr}");
    assert!(
        stderr.contains("no prompts"),
        "the default must say nothing prompts: {stderr}"
    );
    assert!(
        stderr.contains("[sandbox] no sandbox:"),
        "the default must say there is no sandbox: {stderr}"
    );
    // Neither tool is advertised, so the model cannot even ask for one.
    let names = advertised_tools(&server, 0);
    for absent in ["write", "patch", "shell"] {
        assert!(
            !names.contains(&absent.to_owned()),
            "`{absent}` must not exist without --workspace: {names:?}"
        );
    }
    assert!(tool_result_content(&server, 1).contains("no tool named"));
    assert!(tool_result_content(&server, 2).contains("no tool named"));
}

/// ADR 0043 decision 2: `--profile workspace` names the enforced tier, and the startup line
/// says what it means ("sandbox on: commands stay in the workspace").
#[tokio::test]
async fn the_workspace_flag_turns_the_sandbox_on() {
    let dir = scratch("flag-workspace");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("sandbox on: commands stay in the workspace"),
        "{stderr}"
    );
}

/// ADR 0043 §4 (TK0216): a request carries only mounted schemas, and a description names
/// another tool only when that tool is mounted. The Terminal-Bench arm mounts
/// `read,write,patch,shell`; `grep`, `glob` and `job` are absent, so the shell text must not
/// name them.
#[tokio::test]
async fn a_four_tool_set_keeps_the_shell_description_inside_the_set() {
    // `common::process_dir` stays inside the workspace sandbox's writable tree, so this test
    // also runs under the `sandbox` nextest profile (no `/tmp`). `full` is the default and
    // needs no backend, so no nested Seatbelt is asked for.
    let dir = common::process_dir("radian-exec-tools-shell");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--tools",
                "read,write,patch,shell",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        advertised_tools(&server, 0),
        ["read", "write", "patch", "shell"],
        "the reduced arm mounts exactly its four tools"
    );
    let tools = server.requests()[0].json()["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let shell = tools
        .iter()
        .find(|tool| tool["function"]["name"] == "shell")
        .expect("shell is mounted");
    let description = shell["function"]["description"]
        .as_str()
        .expect("the shell description");
    for absent in ["`grep`", "`glob`", "`find`", "search files"] {
        assert!(
            !description.contains(absent),
            "{absent:?} must not be named without its tool: {description}"
        );
    }
    assert!(
        description.contains("Use `read` to read and list files"),
        "{description}"
    );
    let background = shell["function"]["parameters"]["properties"]["background"]["description"]
        .as_str()
        .expect("the background field");
    assert!(
        !background.contains("`job`"),
        "`job` is not mounted, so the background note must not name it: {background}"
    );
}

/// Writes `<ws>/.radian/config.toml` with `text`.
fn write_project_config(ws: &Path, text: &str) {
    let dir = ws.join(".radian");
    std::fs::create_dir_all(&dir).unwrap();
    write_file(&dir, "config.toml", text);
}

/// ADR 0043 decision 2: a project may only narrow, so `full` there is refused with a warning.
/// The mutation this catches is accepting it: with a config `workspace` and a project `full`,
/// the wider tier would take effect.
#[tokio::test]
async fn a_project_config_cannot_widen_the_tier() {
    let dir = scratch("project-full");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_project_config(&ws, "[permissions]\nprofile = \"full\"\n");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[permissions]\nprofile = \"workspace\"\n",
    );
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[project]"), "{stderr}");
    assert!(stderr.contains("cannot widen"), "{stderr}");
    // Still the config's `workspace`, not the project's `full`.
    assert!(
        stderr.contains("sandbox on: commands stay in the workspace"),
        "{stderr}"
    );
}

/// ADR 0043 decision 2: the remembered user record and the project declaration are resolved to
/// the **narrower** of the two. Taking the wider one is the mutation this catches.
#[tokio::test]
async fn a_remembered_tier_is_narrowed_by_the_project() {
    let dir = scratch("project-narrows");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_project_config(&ws, "[permissions]\nprofile = \"workspace\"\n");
    let rh = dir.join("rh");
    let access = rh.join("access");
    std::fs::create_dir_all(&access).unwrap();
    let key = std::fs::canonicalize(&ws).unwrap();
    write_file(
        &access,
        "access.json",
        &format!("{{\"{}\": \"full\"}}\n", key.display()),
    );
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[("RADIAN_HOME", rh.to_str().unwrap())],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("[access] workspace (from project config)"),
        "{stderr}"
    );
    assert!(stderr.contains("[profile] workspace"), "{stderr}");
}

/// ADR 0043 decision 2: an explicit `--profile` wins, and the startup line still names what
/// the project asks for.
#[tokio::test]
async fn an_explicit_flag_wins_and_the_line_names_the_project_ask() {
    let dir = scratch("flag-wins");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_project_config(&ws, "[permissions]\nprofile = \"workspace\"\n");
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "full",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[profile] full"), "{stderr}");
    assert!(stderr.contains("[sandbox] no sandbox:"), "{stderr}");
    assert!(
        stderr.contains("the project asks for workspace"),
        "{stderr}"
    );
}

/// ADR 0034: under `full` the shell keeps the six `HERDR_*` names and
/// any `[sandbox] allow_env` name, and still drops everything else. `HERDR_PANE_ID` being set
/// in the parent is what the shell must see.
#[tokio::test]
async fn the_shell_environment_carries_herdr_and_allow_env() {
    let dir = scratch("shell-env");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let rh = dir.join("rh");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[sandbox]\nallow_env = [\"FOO\"]\n",
    );
    let command =
        "printf '%s|%s|%s' \"$HERDR_PANE_ID\" \"$FOO\" \"$SECRET_API_KEY\" > env.txt".to_string();
    let server = MockServer::start(vec![shell_reply("c1", &command), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[
                ("RADIAN_HOME", rh.to_str().unwrap()),
                ("HERDR_PANE_ID", "pane-9"),
                ("FOO", "foo-value"),
                ("SECRET_API_KEY", "sk-secret-value"),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let seen = std::fs::read_to_string(ws.join("env.txt"))
        .unwrap_or_else(|e| panic!("the shell did not run: {e}\n{stderr}"));
    assert_eq!(
        seen, "pane-9|foo-value|",
        "the shell must see the HERDR name and the configured name, and nothing else"
    );
}

/// 2: a failed sandbox probe refuses to run; it never falls back to an unsandboxed run.
///
/// `RADIAN_SANDBOX_PROBE_FAIL=1` is a test seam that makes the CLI behave exactly as if the
/// backend probe had failed. It can only cause a refusal, so it cannot weaken the guarantee;
/// it exists because a real probe failure cannot be produced on a machine that has the
/// backend.
#[tokio::test]
async fn a_failed_sandbox_probe_refuses_instead_of_degrading() {
    let dir = scratch("probe-fail");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                // The enforced tier is what an unavailable backend refuses (ADR 0043:
                // `full` is the default and needs no backend).
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "hi",
            ],
            &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("sandbox backend is unavailable") || stderr.contains("cannot be enforced"),
        "{stderr}"
    );
    assert_eq!(server.requests().len(), 0, "the model must never run");
    assert!(stdout.is_empty(), "{stdout}");
    // Nothing was written into the workspace either.
    assert_eq!(std::fs::read_dir(&ws).unwrap().count(), 0);
}

/// 10: the startup text states the real limits and never claims credential isolation
/// (ADR 0012 P4). `--profile workspace` names the tier whose facts sentence carries
/// the limits.
#[tokio::test]
async fn the_startup_text_does_not_claim_credential_isolation() {
    let dir = scratch("wording");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[profile] workspace"), "{stderr}");
    assert!(stderr.contains("[sandbox]"), "{stderr}");
    for phrase in [
        "credentials are isolated",
        "credential isolation",
        "credentials are safe",
        "fully isolated",
        "凭据已隔离",
        "IPC is contained",
    ] {
        assert!(
            !stderr.contains(phrase),
            "the startup text must not claim {phrase:?}: {stderr}"
        );
    }
    // The honest limit is stated instead.
    assert!(stderr.contains("not tightened"), "{stderr}");
    // ADR 0012, decision (option B): MCP servers keep running unsandboxed
    // under workspace-write, and the notice is what tells the user so. With that choice this
    // sentence is the only safeguard, so a test has to hold it in place.
    assert!(
        stderr.contains("MCP servers are NOT sandboxed"),
        "the notice must say MCP runs outside the sandbox: {stderr}"
    );
    // ...and must not claim, unqualified, that every write is confined.
    assert!(
        !stderr.contains("[sandbox] writes are confined to the workspace"),
        "{stderr}"
    );
}

/// ADR 0036: `agent` reads the workspace's `AGENTS.md` and names it on the startup line.
#[tokio::test]
async fn the_context_file_is_reported_on_the_startup_line() {
    let dir = scratch("context-line");
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "CONTEXT-LINE-MARKER\n").unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[context] AGENTS.md"), "{stderr}");
}

/// Every tool result text in request `n` (a batch has more than one).
fn tool_result_contents(server: &MockServer, n: usize) -> Vec<String> {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The tool result text for one call id in request `n`.
fn tool_result_for(server: &MockServer, n: usize, call_id: &str) -> String {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == call_id)
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_else(|| {
            panic!(
                "no tool result for {call_id} in request {n}: {:?}",
                messages_of(server, n)
            )
        })
}

fn shell_reply(id: &str, command: &str) -> Reply {
    tool_reply(id, "shell", &json!({"command": command}).to_string())
}

/// A shell call that asks to leave the sandbox: the ask-mechanics tests below use these,
/// because slice 1 of `sandbox-auto-allow-shell` lets a sandboxed `workspace` shell run
/// without asking. An approved outside call really runs unsandboxed, so these stay on
/// harmless `echo` commands.
fn outside_reply(id: &str, command: &str) -> Reply {
    tool_reply(
        id,
        "shell",
        &json!({
            "command": command,
            "outside_sandbox": true,
            "outside_sandbox_reason": "the test leaves the sandbox on purpose",
        })
        .to_string(),
    )
}

/// 3: a shell call that leaves the sandbox asks; "allow once" runs it; the *next*
/// identical call asks again. Runs under `--profile workspace` with `outside_sandbox`
/// calls — a sandboxed shell no longer asks (slice 1 of `sandbox-auto-allow-shell`).
#[tokio::test]
async fn a_shell_command_asks_every_time_when_allowed_once() {
    let dir = scratch("shell-ask");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        outside_reply("c1", "echo hi"),
        outside_reply("c2", "echo hi"),
        text_reply("done"),
    ])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("y\n").await;
    child.wait_for_stderr_count("[y/N]", 2).await;
    child.write("y\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr.matches("[y/N]").count(),
        2,
        "an allow-once must not cover the next call: {stderr}"
    );
    assert!(
        stderr.contains("echo hi"),
        "the prompt shows the command: {stderr}"
    );
    assert!(tool_result_content(&server, 1).contains("hi"));
    assert!(tool_result_content(&server, 2).contains("hi"));
}

/// 4: "allow for this session" covers the next call in the same turn; a resumed session has
/// no grant and asks again. Uses `outside_sandbox` calls: a sandboxed `workspace` shell
/// no longer asks (slice 1 of `sandbox-auto-allow-shell`), and the grant never crosses
/// the sandbox boundary.
#[tokio::test]
async fn allow_for_session_covers_the_turn_and_not_a_resume() {
    let sessions = fresh_session_dir();
    let dir = scratch("shell-session");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let outside_args = json!({
        "command": "echo hi",
        "outside_sandbox": true,
        "outside_sandbox_reason": "the test leaves the sandbox on purpose",
    })
    .to_string();
    let two_calls = Reply::sse(&[
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "c1", "type": "function",
                 "function": {"name": "shell", "arguments": outside_args}},
                {"index": 1, "id": "c2", "type": "function",
                 "function": {"name": "shell", "arguments": outside_args.clone()}}
            ]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let server = MockServer::start(vec![
        two_calls,
        text_reply("done"),
        outside_reply("c3", "echo hi"),
        text_reply("done again"),
    ])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--session-id",
            "s-session",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("a\n").await; // always for this session
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr.matches("[y/N]").count(),
        1,
        "the second call in the same turn must use the grant: {stderr}"
    );
    let results = tool_result_contents(&server, 1);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results.iter().all(|text| text.contains("hi")),
        "{results:?}"
    );

    // Resume: a new hooks instance, so the grant is gone and it asks again.
    let mut resumed = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--resume",
            "s-session",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    resumed.write("go\n").await;
    resumed.wait_for_stderr("[y/N]").await;
    resumed.write("n\n").await;
    resumed.write("/exit\n").await;
    let (code, _, stderr) = resumed.finish().await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[y/N]"), "resume must ask again: {stderr}");
}

/// Slice 1 of `sandbox-auto-allow-shell`: a compound runs without a prompt inside the
/// sandbox — the tier's blanket `allow` covers every segment (`approver = policy`), so
/// no allow rule is involved at all. The unsandboxed ask stays covered end to end by the
/// `outside_sandbox` tests (`a_shell_command_asks_every_time_when_allowed_once`,
/// `allow_for_session_covers_the_turn_and_not_a_resume`,
/// `never_turns_ask_into_a_denial`, `every_decision_is_audited_once_with_the_right_approver`);
/// the unit tests in `workspace_sandbox.rs` pin the unsandboxed compound ask.
#[tokio::test]
async fn a_sandboxed_compound_shell_command_runs_without_a_prompt() {
    let sessions = fresh_session_dir();
    let dir = scratch("compound-sandboxed");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // Inside the workspace: the shell really runs, and a confined sandbox still lets
    // workspace-local writes through, so the marker proves execution either way.
    let marker = ws.join("compound-ran.txt");
    let composite = format!("git status; touch {}", marker.display());
    let server = MockServer::start(vec![
        shell_reply("c1", "git status"),
        shell_reply("c2", &composite),
        text_reply("done"),
    ])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--session-id",
            "s-compound",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    // No prompt to wait for: gate on the model's third request (two tool calls, then the
    // final text) instead.
    wait_until("both shell calls to reach the model", || {
        server.requests().len() >= 3
    })
    .await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("[y/N]"),
        "nothing may ask for a sandboxed shell: {stderr}"
    );
    assert!(marker.exists(), "the compound ran: {stderr}");
    let simple = tool_result_for(&server, 1, "c1");
    assert!(
        simple.contains("$ git status") && !simple.contains("denied"),
        "the simple command ran: {simple}"
    );
    let compound = tool_result_for(&server, 2, "c2");
    assert!(
        !compound.contains("denied"),
        "the compound was not denied: {compound}"
    );

    let raw = std::fs::read_to_string(sessions.join("s-compound.jsonl")).unwrap();
    let decisions: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["event"] == "PermissionResolved")
        .map(|value| value["data"].clone())
        .collect();
    for call in ["c1", "c2"] {
        let decision = decisions
            .iter()
            .find(|d| d["call_id"] == call)
            .unwrap_or_else(|| panic!("no decision for {call}: {decisions:#?}"));
        assert_eq!(decision["decision"], "allow", "{decision:#?}");
        assert_eq!(
            decision["approver"], "policy",
            "the tier blanket decided it: {decision:#?}"
        );
    }
}

/// 6: `--approval never` turns an `ask` into a denial: the command never runs. Uses an
/// `outside_sandbox` call — a sandboxed `workspace` shell no longer asks (slice 1 of
/// `sandbox-auto-allow-shell`).
#[tokio::test]
async fn never_turns_ask_into_a_denial() {
    let dir = scratch("never-deny");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![outside_reply("c1", "echo hi"), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--approval",
                "never",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("outside the sandbox"),
        "the denial must name what was refused: {result}"
    );
    assert!(
        !result.contains("exit code: 0"),
        "the command never runs: {result}"
    );
}

/// `full` runs shell with no terminal and no `--yes`: nothing asks, so the command runs
/// and only real output comes back (`42` cannot come from a denial: denials echo the
/// command, which never contains it). Reverting the tier's blanket shell `allow` denies
/// the call, so this test is that mutation's end-to-end catcher.
#[tokio::test]
async fn full_runs_shell_without_a_terminal_and_without_yes() {
    let dir = scratch("full-shell-no-yes");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        shell_reply("c1", "echo $((40 + 2))"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "full",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("[y/N]"),
        "nothing may ask in full: {stderr}"
    );
    let result = tool_result_for(&server, 1, "c1");
    assert!(result.contains("42"), "the command ran: {result}");
    assert!(!result.contains("denied"), "{result}");
    assert!(!result.contains("not approved"), "{result}");
}

/// Slice 1 of `sandbox-auto-allow-shell`: headless `radian exec --profile workspace`
/// without `--yes` runs a sandboxed shell with no prompt — the tier's blanket `allow`
/// answers it (`approver = policy`), the way `full` already did. Reverting the blanket
/// `allow` denies the call (no terminal to ask in), so this test is that mutation's
/// end-to-end catcher.
#[tokio::test]
async fn workspace_runs_a_sandboxed_shell_without_a_terminal_and_without_yes() {
    let dir = scratch("workspace-shell-no-yes");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        shell_reply("c1", "echo $((40 + 2))"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("[y/N]"),
        "nothing may ask for a sandboxed shell: {stderr}"
    );
    let result = tool_result_for(&server, 1, "c1");
    assert!(result.contains("42"), "the command ran: {result}");
    assert!(!result.contains("denied"), "{result}");
    assert!(!result.contains("not approved"), "{result}");
}

/// Slice 1 of `sandbox-auto-allow-shell`: the same headless run with an
/// `outside_sandbox` request is denied with a clear result — `ask` with no approver is
/// denied as before, and nothing re-runs the command outside automatically. Letting it
/// skip `ask` runs the call, so this test is that mutation's end-to-end catcher.
#[tokio::test]
async fn workspace_outside_sandbox_is_denied_without_a_terminal() {
    let dir = scratch("workspace-outside-no-yes");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        outside_reply("c1", "echo SHOULD-NOT-RUN"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_for(&server, 1, "c1");
    assert!(
        result.contains("not approved") || result.contains("denied"),
        "the outside call must be denied with a clear result: {result}"
    );
    assert!(
        !result.contains("exit code: 0"),
        "the command must not have run: {result}"
    );
}

/// 7: Ctrl-C while waiting for a shell approval cancels the turn; the process does not hang.
/// Uses an `outside_sandbox` call so there is an approval to cancel (a sandboxed
/// `workspace` shell no longer asks since slice 1).
#[tokio::test]
async fn cancelling_at_the_shell_approval_does_not_hang() {
    let dir = scratch("shell-cancel");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![outside_reply("c1", "echo hi"), text_reply("done")]).await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr("[y/N]").await;
    sigint(&child.child);
    // The approval wait is interrupted and the turn converges; nothing hangs.
    child.wait_for_stderr("cancelled").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    // The turn never reached a second model request: the approval was interrupted, not
    // answered, and the command did not run.
    assert_eq!(server.requests().len(), 1, "{stderr}");
}

/// 8: every decision writes exactly one `PermissionResolved`, with the right approver.
#[tokio::test]
async fn every_decision_is_audited_once_with_the_right_approver() {
    let sessions = fresh_session_dir();
    let dir = scratch("audit");
    let ws = dir.join("ws");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(ws.join("f.txt"), "content").unwrap();
    let outside_file = outside.join("x.txt");

    let three_calls = Reply::sse(&[
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "r1", "type": "function",
                 "function": {"name": "read", "arguments": "{\"path\":\"f.txt\"}"}},
                {"index": 1, "id": "s1", "type": "function",
                 "function": {"name": "shell", "arguments": json!({
                     "command": "echo hi",
                     "outside_sandbox": true,
                     "outside_sandbox_reason": "the test leaves the sandbox on purpose"}).to_string()}},
                {"index": 2, "id": "w1", "type": "function",
                 "function": {"name": "write",
                              "arguments": format!("{{\"path\":{},\"content\":\"x\"}}", json!(outside_file.to_string_lossy()))}}
            ]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let server = MockServer::start(vec![three_calls, text_reply("done")]).await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--session-id",
            "s-audit",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    // `read` is allowed by a rule (no prompt); a sandboxed `shell` no longer asks either
    // (slice 1 of `sandbox-auto-allow-shell`), so the audited `ask` is the
    // `outside_sandbox` call; `write` outside the workspace has no rule and falls to ask.
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("y\n").await;
    child.wait_for_stderr_count("[y/N]", 2).await;
    child.write("n\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;
    assert_eq!(code, 0, "{stderr}");

    let raw = std::fs::read_to_string(sessions.join("s-audit.jsonl")).unwrap();
    let decisions: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["event"] == "PermissionResolved")
        .map(|value| value["data"].clone())
        .collect();
    assert_eq!(decisions.len(), 3, "{decisions:#?}");
    let by_call = |id: &str| -> Value {
        decisions
            .iter()
            .find(|d| d["call_id"] == id)
            .unwrap_or_else(|| panic!("no decision for {id}: {decisions:#?}"))
            .clone()
    };
    assert_eq!(by_call("r1")["decision"], "allow");
    assert_eq!(by_call("r1")["approver"], "policy", "a rule decided it");
    assert_eq!(by_call("s1")["decision"], "allow");
    assert_eq!(by_call("s1")["approver"], "user", "the human answered");
    assert_eq!(by_call("w1")["decision"], "deny");
    assert_eq!(by_call("w1")["approver"], "user", "the human answered");
    assert_eq!(
        decisions.len(),
        decisions
            .iter()
            .map(|d| d["call_id"].clone())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "one decision per call"
    );
}

/// 9: under `workspace`, a shell command cannot write outside the workspace.
#[tokio::test]
async fn shell_cannot_write_outside_the_workspace() {
    let dir = scratch("shell-escape");
    let ws = dir.join("ws");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("escaped.txt");
    let command = format!("echo x > {}", target.display());
    let server = MockServer::start(vec![shell_reply("c1", &command), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_content(&server, 1);
    // The sandbox denies the write, but the shell's exit status for a failed redirection is
    // its own: bash uses 1, dash (Linux's `/bin/sh`) uses 2. Assert what the test means — the
    // command failed — not one shell's number.
    let exit = result
        .lines()
        .find_map(|line| line.strip_prefix("exit code: "))
        .expect("the shell result has an `exit code:` line");
    assert_ne!(
        exit, "0",
        "the sandboxed command must fail (a redirection denied): {result}"
    );
    assert!(
        result.contains("sandbox denied: filesystem-write"),
        "the failure must carry the sandbox marker: {result}"
    );
    assert!(
        !target.exists(),
        "the sandbox let a write escape the workspace"
    );
}

// ---------- small fixes: existing session id, provider flags, scratch cleanup ----------

/// D15: naming an existing session with `--session-id` points at the command that continues
/// it, and does not touch the file.
#[tokio::test]
async fn session_id_already_exists_points_at_resume() {
    let dir = scratch("dup-id");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("one")]).await;
    let args = [
        "--model",
        "m",
        "--base-url",
        &server.base_url,
        "--workspace",
        ws.to_str().unwrap(),
        "--session-id",
        "dup",
        "--session-dir",
        sessions.to_str().unwrap(),
        "-p",
        "hi",
    ];
    let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let before = std::fs::read(sessions.join("dup.jsonl")).unwrap();

    let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stderr.contains("--resume dup"), "{stderr}");
    assert_eq!(
        std::fs::read(sessions.join("dup.jsonl")).unwrap(),
        before,
        "the existing session must not be touched"
    );
}

/// `--thinking` and `--max-tokens` are Anthropic-only: under the default openai-compatible
/// provider they are a usage error before any session file exists, not a silent no-op.
#[tokio::test]
async fn anthropic_only_flags_are_refused_under_openai_compatible() {
    // `--thinking` left this check in ADR 0026 D11: the unified levels ride both wire
    // protocols (`reasoning_effort` for OpenAI-compatible), so only `--max-tokens` remains
    // Anthropic-only.
    {
        let (label, extra) = ("max-tokens", vec!["--max-tokens", "100"]);
        let dir = scratch(&format!("flag-{label}"));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut args = vec![
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "flagged",
            "-p",
            "hi",
        ];
        args.extend(extra);
        let (code, stdout, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 2, "{label}: {stderr}");
        assert!(stderr.contains("anthropic"), "{label}: {stderr}");
        assert!(stdout.is_empty(), "{label}: {stdout}");
        assert!(
            std::fs::read_dir(&sessions).unwrap().next().is_none(),
            "{label}: a session file was created"
        );
    }
}

/// The same flags work under `--provider anthropic`: `--max-tokens` reaches the body and
/// `--thinking adaptive` becomes the `thinking` object.
#[tokio::test]
async fn anthropic_provider_accepts_thinking_and_max_tokens() {
    let server = anth::MockServer::start(vec![anth_text_reply("THINK-OK")]).await;
    let dir = scratch("anthropic-flags");
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude-x",
                "--provider",
                "anthropic",
                "--thinking",
                "adaptive",
                "--max-tokens",
                "4096",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("THINK-OK"), "{stdout}");
    let body = server.requests()[0].json();
    assert_eq!(body["max_tokens"], 4096, "{body}");
    assert_eq!(body["thinking"], json!({"type": "adaptive"}), "{body}");
}

/// D14: a scratch directory is removed when its guard drops, so a passing test leaves
/// nothing behind under `TMPDIR`.
#[test]
fn scratch_directories_are_removed_when_the_guard_drops() {
    let path = {
        let dir = scratch("scratch-drop");
        assert!(dir.exists(), "{}", dir.display());
        dir.to_path_buf()
    };
    assert!(
        !path.exists(),
        "the scratch guard must remove {}",
        path.display()
    );
}

/// ADR 0016: the tier names are `workspace`/`full`; the old names stay accepted as
/// aliases but are not advertised in `--help`. The removed `safe` tier is rejected.
#[tokio::test]
async fn tier_names_and_aliases_are_accepted_but_help_lists_only_new_names() {
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&["--help"], &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("workspace"), "{stdout}");
    assert!(stdout.contains("full"), "{stdout}");
    assert!(
        !stdout.contains("safe"),
        "the removed tier must not be advertised: {stdout}"
    );
    assert!(
        !stdout.contains("workspace-write"),
        "the old name must not be advertised: {stdout}"
    );
    assert!(
        !stdout.contains("full-access"),
        "the old name must not be advertised: {stdout}"
    );

    for profile in ["workspace", "full", "workspace-write", "full-access"] {
        let dir = scratch(&format!("alias-{profile}"));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--profile",
                    profile,
                    "--session-dir",
                    sessions.to_str().unwrap(),
                    "--list",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{profile}: {stderr}");
    }
}

/// A new session directory and file are owner-only (ADR 0013): a session is potentially
/// sensitive data (ADR 0006 §11).
#[cfg(unix)]
#[tokio::test]
async fn a_new_session_directory_and_file_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = scratch("private-session");
    let sessions = dir.join("sessions"); // does not exist yet
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &sessions,
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "priv",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(mode(&sessions), 0o700, "the session directory must be 0700");
    assert_eq!(
        mode(&sessions.join("priv.jsonl")),
        0o600,
        "the session file must be 0600"
    );
}

/// A session directory that already exists with wider permissions is reported on stderr and
/// left exactly as it was: the fix never silently changes a user's directory.
#[cfg(unix)]
#[tokio::test]
async fn a_wide_session_directory_is_warned_about_and_not_changed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch("wide-session-dir");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &sessions,
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("[privacy]"),
        "no warning on stderr: {stderr}"
    );
    assert!(stderr.contains("chmod 700"), "{stderr}");
    assert_eq!(
        std::fs::metadata(&sessions).unwrap().permissions().mode() & 0o777,
        0o755,
        "an existing directory's permissions must not be changed"
    );
}

/// The configured endpoint can carry credentials in its userinfo or a query token. Any error
/// that names it must redact them (ADR 0013 T4), and the host must not prepend the raw value.
#[tokio::test]
async fn a_base_url_with_credentials_is_redacted_in_errors() {
    // No key + a remote endpoint: the "no API key" error names the endpoint.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "https://user:url-secret@example.com/v1?token=query-secret",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(!stderr.contains("url-secret"), "userinfo leaked: {stderr}");
    assert!(
        !stderr.contains("query-secret"),
        "query token leaked: {stderr}"
    );
    assert!(stderr.contains("example.com"), "{stderr}");

    // An unparseable URL with a secret: the provider error names the redacted form, and the
    // host must not prepend the raw one.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", "url-secret", "-p", "hi"],
            &[("OPENAI_API_KEY", "sk-x")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        !stderr.contains("url-secret"),
        "the raw URL leaked: {stderr}"
    );
}

/// A search snippet is session content and can carry a model-echoed key; `--search` redacts
/// the known secrets exactly like `--export` (ADR 0013 T3).
#[tokio::test]
async fn search_redacts_the_key_from_a_snippet() {
    let dir = scratch("search-redact");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("echoed sk-search-secret in the answer")]).await;
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

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--search",
                "echoed",
            ],
            &[("OPENAI_API_KEY", "sk-search-secret")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stdout.contains("sk-search-secret"),
        "the key leaked into a search hit: {stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
}

// ---------- model errors end the turn, not the session (ADR 0017) ----------

/// Interactive mode: a 500 is shown on stderr and the prompt comes back; the next line runs a
/// new turn that succeeds. The driver is not ended by the model error.
#[tokio::test]
async fn an_interactive_model_error_is_shown_and_the_session_continues() {
    let server = MockServer::start(vec![
        // A non-transient status (ADR 0017 retries only connection failures, dropped
        // streams, idle timeouts and 408/429/5xx/529), so one failure ends the turn.
        Reply::status(400, "provider is down"),
        text_reply("SECOND-OK"),
    ])
    .await;
    let mut cli = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "recover",
        ],
        &[("OPENAI_API_KEY", KEY)],
    );
    cli.wait_for_stderr("> ").await;
    cli.write("first\n").await;
    // The error text reaches stderr (masked by the adapter and the host).
    cli.wait_for_stderr("HTTP 400").await;
    cli.wait_for_stderr("provider is down").await;
    // The prompt is back: the session did not exit.
    cli.wait_for_stderr_count("> ", 2).await;
    cli.write("second\n").await;
    cli.wait_for_stderr_count("> ", 3).await;
    cli.write("/exit\n").await;
    let (code, stdout, stderr) = cli
        .finish_open(
            Duration::from_secs(5),
            "/exit was written after the recovered turn; stdin stayed open",
        )
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SECOND-OK"), "{stdout}");
    assert_eq!(server.requests().len(), 2, "both turns reached the model");
}

/// ADR 0017: a transient error (HTTP 500) is retried inside the one model call, the
/// retry line goes to stderr, and the turn still succeeds on the second attempt.
#[tokio::test]
async fn a_transient_error_is_retried_and_the_turn_still_succeeds() {
    let server = MockServer::start(vec![
        Reply::status(500, "provider is down"),
        text_reply("RETRIED-OK"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("RETRIED-OK"), "{stdout}");
    assert!(stderr.contains("retrying 1/3"), "{stderr}");
    assert_eq!(
        server.requests().len(),
        2,
        "the first attempt and its retry reached the model"
    );
}

/// One-shot mode is unchanged: a model error still exits 1, and the message is on stderr.
#[tokio::test]
async fn one_shot_model_error_still_exits_one_and_shows_the_error() {
    let server = MockServer::start(vec![Reply::status(400, "provider is down")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("HTTP 400"), "{stderr}");
    assert!(stdout.is_empty(), "stdout must stay model-only: {stdout}");
}

/// ADR 0018's fallback: a recognizable context-overflow error is classified as such and gets
/// one extra hint line (the automatic compaction is a later slice); it still exits 1.
#[tokio::test]
async fn a_context_overflow_error_adds_a_hint() {
    let body = r#"{"error":{"message":"This model's maximum context length is 16385 tokens. However, your messages resulted in 20000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let server = MockServer::start(vec![Reply::status(400, body)]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("model context overflow"), "{stderr}");
    assert!(
        stderr.contains("compaction could not keep the request"),
        "the hint must be shown: {stderr}"
    );
    assert!(stdout.is_empty(), "stdout must stay model-only: {stdout}");
}

// ---------- dogfooding D21: a closed stdout is not a panic ----------

/// D21: every "print and exit" command must treat a closed stdout as a normal stop: exit 0,
/// no panic on stderr.
#[cfg(unix)]
#[test]
fn a_closed_stdout_is_not_a_panic_for_every_output_command() {
    let dir = scratch("sigpipe");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "s1",
        &canonical(&ws),
        "hello sigpipe",
        None,
        None,
    );

    let home = scratch("sigpipe-home");

    let sessions_arg = sessions.to_str().unwrap();
    let ws_arg = ws.to_str().unwrap();
    let home_arg = home.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec![
            "--model",
            "m",
            "--session-dir",
            sessions_arg,
            "--workspace",
            ws_arg,
            "--list",
        ],
        vec![
            "--model",
            "m",
            "--session-dir",
            sessions_arg,
            "--workspace",
            ws_arg,
            "--search",
            "hello",
        ],
        vec![
            "--model",
            "m",
            "--session-dir",
            sessions_arg,
            "--workspace",
            ws_arg,
            "--export",
            "s1",
        ],
        vec!["trust", "status"],
    ];
    for args in cases {
        let mut child = std::process::Command::new(common::radian_bin())
            .arg("exec")
            .args(&args)
            .env("RADIAN_HOME", home_arg)
            .env("HOME", home_arg)
            .env("RADIAN_SESSION_DIR", sessions_arg)
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .stdin(Stdio::null())
            .stdout(closed_stdout())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agent");
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        let status = child.wait().expect("wait");
        assert_eq!(status.code(), Some(0), "args {args:?}: stderr {stderr}");
        assert!(!stderr.contains("panicked"), "args {args:?}: {stderr}");
    }
}

/// D22 wording: `--list`/`--search` no longer silently ignore `--session-id`; clap refuses the
/// combination with a usage error (exit 2).
#[tokio::test]
async fn list_and_search_refuse_session_id() {
    for args in [
        vec!["--list", "--session-id", "x"],
        vec!["--search", "x", "--session-id", "x"],
    ] {
        let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 2, "args {args:?}: {stderr}");
        assert!(stderr.contains("--session-id"), "args {args:?}: {stderr}");
    }
}

// ---------- subtree context files (ADR 0036) ----------

/// A one-call `read` reply.
fn read_reply(id: &str, path: &str) -> Reply {
    tool_reply(id, "read", &json!({"path": path}).to_string())
}

/// A one-call `grep` reply.
fn grep_reply(id: &str, pattern: &str, path: &str) -> Reply {
    tool_reply(
        id,
        "grep",
        &json!({"pattern": pattern, "path": path}).to_string(),
    )
}

/// A one-call `glob` reply.
fn glob_reply(id: &str, pattern: &str) -> Reply {
    tool_reply(id, "glob", &json!({"pattern": pattern}).to_string())
}

/// Writes a nested file under `dir`, creating its directories.
fn write_nested(dir: &Path, relative: &str, text: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// How often `needle` appears in the n-th recorded request's **conversation** messages. The
/// system prompt is left out, so a startup file and an appended block cannot be confused.
fn conversation_count(server: &MockServer, n: usize, needle: &str) -> usize {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .map(|message| message.to_string().matches(needle).count())
        .sum()
}

/// The session files' text, concatenated.
fn sessions_text(sessions: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return String::new();
    };
    let mut text = String::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    text
}

/// 1 and 2: reading `crates/x/src/a.rs` appends `crates/x/AGENTS.md` once, before the next
/// model request, and the journal has exactly one `ContextFileAppended`; reading
/// `crates/x/src/b.rs` next appends nothing.
#[tokio::test]
async fn a_subtree_context_file_appends_once_before_the_next_request() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-once");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    write_nested(&ws, "crates/x/src/b.rs", "fn b() {}");
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        read_reply("c2", "crates/x/src/b.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 3, "{stderr}");
    assert_eq!(
        conversation_count(&server, 0, "SUBTREE-X-RULES"),
        0,
        "nothing is appended before the first read"
    );
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-X-RULES"),
        1,
        "the block is in the request after the first read"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-X-RULES"),
        1,
        "the second read appends nothing"
    );

    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        1,
        "{journal}"
    );
    assert!(
        journal.contains("\"path\":\"crates/x/AGENTS.md\""),
        "{journal}"
    );
    assert!(
        journal.contains(&format!("\"bytes\":{}", "SUBTREE-X-RULES".len())),
        "{journal}"
    );
}

/// 2: after `crates/x/AGENTS.md` is loaded, reading `crates/x/sub/c.rs` appends only
/// `crates/x/sub/AGENTS.md`.
#[tokio::test]
async fn a_deeper_subtree_appends_only_the_deeper_file() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-deeper");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    write_nested(&ws, "crates/x/sub/AGENTS.md", "SUBTREE-SUB-RULES");
    write_nested(&ws, "crates/x/sub/c.rs", "fn c() {}");
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        read_reply("c2", "crates/x/sub/c.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-SUB-RULES"),
        0,
        "the deeper file is not loaded yet"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-X-RULES"),
        1,
        "the parent file stays once"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-SUB-RULES"),
        1,
        "only the deeper file is appended"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        2,
        "{journal}"
    );
    assert!(
        journal.contains("\"path\":\"crates/x/sub/AGENTS.md\""),
        "{journal}"
    );
}

/// 3: the workspace root's `AGENTS.md` is loaded at startup and is never appended again.
#[tokio::test]
async fn a_startup_context_file_is_never_appended_again() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-startup");
    write_nested(&ws, "AGENTS.md", "ROOT-RULES");
    write_nested(&ws, "src/a.rs", "fn a() {}");
    let server = MockServer::start(vec![read_reply("c1", "src/a.rs"), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "ROOT-RULES"),
        0,
        "the startup file is not appended"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        0,
        "{journal}"
    );
}

/// 5: `[context] subtree_files = false` appends nothing and writes no event.
#[tokio::test]
async fn the_subtree_switch_turns_the_append_off() {
    let (dir, ws, sessions) = subtree_scratch("subtree-off");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[context]\nsubtree_files = false\n",
    );
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-X-RULES"),
        0,
        "the switch appends nothing"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        0,
        "{journal}"
    );
}

/// 7: a resumed session whose journal already has `ContextFileAppended` for
/// `crates/x/AGENTS.md` does not append it again.
#[tokio::test]
async fn a_resumed_session_does_not_append_a_recorded_file_again() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-resume");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    write_nested(&ws, "crates/x/src/b.rs", "fn b() {}");
    let first = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        text_reply("done"),
    ])
    .await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &first.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "subtree-resume",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        sessions_text(&sessions)
            .matches("\"event\":\"ContextFileAppended\"")
            .count(),
        1
    );

    let second = MockServer::start(vec![
        read_reply("c2", "crates/x/src/b.rs"),
        text_reply("done again"),
    ])
    .await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &second.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--resume",
                "subtree-resume",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go on",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&second, 0, "SUBTREE-X-RULES"),
        1,
        "the resumed transcript carries the one appended message"
    );
    assert_eq!(
        conversation_count(&second, 1, "SUBTREE-X-RULES"),
        1,
        "the resumed read appends nothing"
    );
    assert_eq!(
        sessions_text(&sessions)
            .matches("\"event\":\"ContextFileAppended\"")
            .count(),
        1
    );
}

/// 9: a `grep` with `path: crates/x` appends `crates/x/AGENTS.md`; a `glob` whose results
/// list files under two subtrees appends both subtrees' files, each once.
#[tokio::test]
async fn grep_paths_and_glob_results_trigger_their_subtrees() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-search");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/a.rs", "fn x() {}");
    write_nested(&ws, "crates/y/AGENTS.md", "SUBTREE-Y-RULES");
    write_nested(&ws, "crates/y/b.rs", "fn y() {}");
    let server = MockServer::start(vec![
        grep_reply("c1", "fn ", "crates/x"),
        glob_reply("c2", "**/*.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-X-RULES"),
        1,
        "the grep path triggered crates/x"
    );
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-Y-RULES"),
        0,
        "crates/y was not touched yet"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-X-RULES"),
        1,
        "crates/x stays once"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-Y-RULES"),
        1,
        "the glob result triggered crates/y"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        2,
        "{journal}"
    );
}

/// 10: `[context] other = 1` lands in `skipped`, the session starts, and the startup line
/// names the ignored key.
#[tokio::test]
async fn an_unknown_context_key_is_skipped_and_the_session_starts() {
    let (dir, ws, sessions) = subtree_scratch("subtree-unknown-key");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[context]\nother = 1\n",
    );
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 1, "{stderr}");
    assert!(
        stderr.contains("[config] ignored key `context.other`"),
        "{stderr}"
    );
}

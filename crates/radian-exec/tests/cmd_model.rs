//! End-to-end tests of `radian exec` and the model route: the API key, custom headers, session ids, usage, the context budget, providers, the catalog window line and `RADIAN_EXTRA_BODY`.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_anthropic::testing as anth;
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::*;
use tokio::process::{Child, Command};

// ---------- the API key ----------

#[tokio::test]
async fn the_key_comes_from_the_named_env_and_never_leaks() {
    // The error body echoes the key back; neither stream may carry it.
    let server = MockServer::start(vec![Reply::status(
        401,
        &format!("{{\"error\":{{\"message\":\"bad {KEY}\"}}}}"),
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_EXEC_TEST_KEY", KEY)],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(!stdout.contains(KEY), "stdout leaked the key");
    assert!(!stderr.contains(KEY), "stderr leaked the key");
}

/// ADR 0013 T4: a runtime error is redacted with **every** login in
/// `auth.json`, not only the route's key. The server echoes a key that belongs to another
/// provider; the adapter cannot mask that one (it only knows its own key), so the host must.
#[tokio::test]
async fn a_runtime_error_redacts_every_auth_key_not_only_the_route_key() {
    let route_key = "sk-ROUTE-KEY-111111";
    let other_key = "sk-OTHER-PROVIDER-KEY-222222";
    let server = MockServer::start(vec![Reply::status(
        401,
        &format!("{{\"error\":{{\"message\":\"bad {other_key}\"}}}}"),
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[
                ("OPENAI_API_KEY", route_key),
                ("ANTHROPIC_API_KEY", other_key),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {route_key}").as_str())
    );
    assert!(
        !stderr.contains(other_key),
        "the runtime error leaked another provider's key: {stderr}"
    );
    assert!(!stdout.contains(other_key), "stdout leaked it: {stdout}");
    assert!(stderr.contains("<redacted>"), "{stderr}");
}

/// ADR 0031 T3: a non-interactive run with project resources is untrusted by default, prints
/// the one-line way out, and writes nothing.
#[tokio::test]
async fn a_workspace_with_project_resources_is_untrusted_without_a_terminal() {
    let dir = scratch("trust-default");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\n---\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    let server = MockServer::start(vec![text_reply("TRUST-OK")]).await;
    let (code, _stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "--session-id",
                "trust-default",
            ],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0);
    assert!(
        stderr.contains("project resources not loaded (untrusted)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("project resources: not loaded (untrusted: default)"),
        "{stderr}"
    );
    assert!(stderr.contains("--trust"), "{stderr}");
    assert!(
        !home.join("trust/trust.json").exists(),
        "nothing is written"
    );
}

/// ADR 0031 T3: `--trust`/`--no-trust` override for this process only and never write.
#[tokio::test]
async fn trust_flags_override_without_writing() {
    let dir = scratch("trust-flags");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\n---\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    // One reply per iteration: `-p` runs a turn each time.
    let server = MockServer::start(vec![text_reply("OK"), text_reply("OK")]).await;
    for (flag, expected) in [
        (
            "--trust",
            "project resources: loaded (trusted: command line)",
        ),
        (
            "--no-trust",
            "project resources: not loaded (untrusted: command line)",
        ),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "-p",
                    "hi",
                    flag,
                    "--workspace",
                    ws.to_str().unwrap(),
                    "--session-dir",
                    sessions.to_str().unwrap(),
                    "--session-id",
                    if flag == "--trust" {
                        "trust-flag-a"
                    } else {
                        "trust-flag-b"
                    },
                ],
                &[("RADIAN_HOME", home.to_str().unwrap())],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{flag}: {stderr}");
        assert!(stderr.contains(expected), "{flag}: {stderr}");
        assert!(
            !home.join("trust/trust.json").exists(),
            "{flag} must not write trust.json"
        );
    }
}

/// Project skills (backlog project-skills): a trusted project's `.agents/skills` reach the
/// system prompt, `skill` tool and `/skill:` catalog; an untrusted project's do not. The
/// startup line tells the truth in both states.
#[tokio::test]
async fn project_skills_load_only_when_trusted() {
    let dir = scratch("project-skills-e2e");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: PROJECT-SKILL-MARKER greets.\n---\nbody\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    // One reply per iteration: `-p` runs a turn each time.
    let server = MockServer::start(vec![text_reply("OK"), text_reply("OK")]).await;
    for (n, (flag, loaded, status)) in [
        (
            "--trust",
            true,
            "project resources: loaded (trusted: command line)",
        ),
        (
            "--no-trust",
            false,
            "project resources: not loaded (untrusted: command line)",
        ),
    ]
    .iter()
    .enumerate()
    {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "-p",
                    "hi",
                    flag,
                    "--workspace",
                    ws.to_str().unwrap(),
                    "--session-dir",
                    sessions.to_str().unwrap(),
                    "--session-id",
                    &format!("project-skills-{n}"),
                ],
                &[("RADIAN_HOME", home.to_str().unwrap())],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{flag}: {stderr}");
        assert!(stderr.contains(status), "{flag}: {stderr}");
        let system = messages_of(&server, n)
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .and_then(|message| message["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            system.contains("PROJECT-SKILL-MARKER"),
            *loaded,
            "{flag}: the project skill must{} reach the system prompt: {system}",
            if *loaded { "" } else { " not" }
        );
    }
}

/// ADR 0031 T3: piped stdin is **not** an interactive user. The trust question must not read
/// its first line (which could be a message, or a stray `y`), so a piped run takes the
/// untrusted default and sends the piped line as a message.
#[tokio::test]
async fn piped_stdin_is_never_read_as_the_trust_answer() {
    let dir = scratch("trust-piped");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\n---\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    let server = MockServer::start(vec![text_reply("PIPED-OK")]).await;
    let (code, _stdout, stderr) = feed_and_wait(
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
                "--session-id",
                "trust-piped",
            ],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        // A stray `y` would be a wrong "allow" if the question read it.
        "hello from the pipe\n",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    // No question: piped stdin is not a terminal.
    assert!(!stderr.contains("project resources found in"), "{stderr}");
    assert!(
        stderr.contains("project resources: not loaded (untrusted: default)"),
        "{stderr}"
    );
    assert!(
        !home.join("trust/trust.json").exists(),
        "nothing is written"
    );
    // The piped line was sent as an ordinary message, not consumed by a trust answer.
    let messages = server.requests()[0].json()["messages"].clone();
    let user = messages
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(user, "hello from the pipe");
}

#[tokio::test]
async fn a_remote_base_url_without_a_key_fails_fast() {
    let start = Instant::now();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "https://api.example.com/v1",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    // ADR 0026 D2/D3: the message names the fix — `radian login <provider>` (which writes
    // `credentials/auth.json`) — and names the redacted endpoint.
    assert!(
        stderr.contains("radian login openai-compatible"),
        "{stderr}"
    );
    assert!(stderr.contains("/login"), "{stderr}");
    assert!(stderr.contains("https://api.example.com/v1"), "{stderr}");
    // The message must not suggest a placeholder key value or embed a JSON template with a
    // key slot any more: `/login` asks for the key without echoing it.
    assert!(!stderr.contains("MY_KEY"), "{stderr}");
    // No network round trip was attempted.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_local_base_url_needs_no_key_and_sends_no_authorization() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, stdout, _) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "ok\n");
    assert_eq!(server.requests()[0].header("authorization"), None);
}

/// ADR 0026 D2: `--api-key-env` is removed, so passing it is a usage error (the key comes only
/// from `auth.json`). This is the mutation guard for re-adding the flag.
#[tokio::test]
async fn the_removed_api_key_env_flag_is_a_usage_error() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--api-key-env", "SOME_KEY", "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--api-key-env"), "{stderr}");
    assert!(stderr.contains("unexpected argument"), "{stderr}");
}

/// ADR 0026 D0: `--base-url`/`--header` only apply to the generic descriptor; a non-generic
/// descriptor sets its own endpoint and headers, so passing them is a usage error.
#[tokio::test]
async fn a_non_generic_descriptor_refuses_base_url_and_header() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--provider",
                "anthropic",
                "--model",
                "claude-x",
                "--base-url",
                "https://api.example.com",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("openai-compatible"), "{stderr}");
    assert!(stderr.contains("--base-url"), "{stderr}");

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--provider",
                "anthropic",
                "--model",
                "claude-x",
                "--header",
                "x-a=b",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--header"), "{stderr}");
}

/// ADR 0026 D0 acceptance: `--provider opencode-go` sends `x-opencode-session` exactly as the
/// old `--header 'x-opencode-session={session_id}'` did, plus `x-opencode-client: radian`.
#[tokio::test]
async fn opencode_go_sends_the_session_header_byte_for_byte() {
    let server = MockServer::start(vec![text_reply("GO-OK")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--provider",
                "opencode-go",
                "--model",
                "deepseek-v4.1-flash",
                "--session-id",
                "go-session",
                "-p",
                "hi",
            ],
            &[
                ("OPENCODE_GO_API_KEY", "sk-go-test"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("GO-OK"), "{stdout}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(requests[0].header("x-opencode-session"), Some("go-session"));
    assert_eq!(requests[0].header("x-opencode-client"), Some("radian"));
}

/// ADR 0026 D2 acceptance: a config that still has `model.api_key` is refused, never silently
/// ignored, and the message points at `auth.json`.
#[tokio::test]
async fn a_config_with_model_api_key_is_refused() {
    let dir = scratch("api-key-removed");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "schema_version = 1\n[model]\napi_key = { from = \"env\", name = \"K\" }\n",
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "-p", "hi"],
            &[("RADIAN_CONFIG", path.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    // The error is the config refusal, not the later no-credential error: it names the key.
    assert!(stderr.contains("model.api_key"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert!(stderr.contains("/login"), "{stderr}");
}

// ---------- custom headers ----------

#[tokio::test]
async fn a_header_flag_is_sent_and_can_be_repeated() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "x-opencode-session=stable-123",
                "--header",
                "x-team=radian",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "ok\n");
    let request = &server.requests()[0];
    assert_eq!(request.header("x-opencode-session"), Some("stable-123"));
    assert_eq!(request.header("x-team"), Some("radian"));
}

#[tokio::test]
async fn a_header_without_an_equals_sign_is_a_usage_error_that_does_not_echo_it() {
    // The malformed argument may itself be a credential: the error must not contain it.
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9",
                "--header",
                "Bearer sk-looks-like-a-secret",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("NAME=VALUE"), "{stderr}");
    assert!(!stderr.contains("sk-looks-like-a-secret"), "{stderr}");
}

#[tokio::test]
async fn an_invalid_header_name_is_refused() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9",
                "--header",
                "bad name=value",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    assert!(stderr.contains("not a valid HTTP header name"), "{stderr}");
}

#[tokio::test]
async fn an_authorization_header_is_refused_so_the_key_has_one_source() {
    let server = MockServer::start(vec![text_reply("should not be reached")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "Authorization=Bearer sk-from-a-header",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert!(!stderr.contains("sk-from-a-header"), "{stderr}");
    assert!(
        server.requests().is_empty(),
        "no request may be sent when a header is refused"
    );
}

#[tokio::test]
async fn a_header_value_never_leaks_into_errors() {
    // The server echoes the header value in an error body; neither stream may carry it.
    let server = MockServer::start(vec![Reply::status(
        400,
        "{\"error\":{\"message\":\"bad team-secret-value\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "x-team-token=team-secret-value",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("HTTP 400"), "{stderr}");
    assert!(!stderr.contains("team-secret-value"), "{stderr}");
}

// ---------- session id and usage ----------

/// The `[session] <id>` note, when the CLI printed one.
fn generated_session_id(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        Some(
            line.strip_prefix("[session] ")?
                .split(' ')
                .next()?
                .to_owned(),
        )
    })
}

#[tokio::test]
async fn a_header_value_can_reference_the_generated_session_id() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "x-opencode-session={session_id}",
            ],
            &[],
        ),
        "first\nsecond\n",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let id = generated_session_id(&stderr).unwrap_or_else(|| panic!("no session note: {stderr}"));
    assert!(!id.is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("x-opencode-session"), Some(id.as_str()));
    // The same process uses the same id for every request.
    assert_eq!(requests[1].header("x-opencode-session"), Some(id.as_str()));
}

#[tokio::test]
async fn two_processes_get_different_generated_session_ids() {
    let mut ids = Vec::new();
    for _ in 0..2 {
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--header",
                    "x-session={session_id}",
                    "-p",
                    "hi",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
        let id = generated_session_id(&stderr).expect("the CLI printed the generated id");
        assert_eq!(server.requests()[0].header("x-session"), Some(id.as_str()));
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1], "different processes must differ");
}

#[tokio::test]
async fn an_explicit_session_id_is_used_verbatim_and_not_announced() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "fixed-42",
                "--header",
                "x-session={session_id}",
            ],
            &[],
        ),
        "first\nsecond\n",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        generated_session_id(&stderr).is_none(),
        "an explicit id needs no note: {stderr}"
    );
    let requests = server.requests();
    assert_eq!(requests[0].header("x-session"), Some("fixed-42"));
    assert_eq!(requests[1].header("x-session"), Some("fixed-42"));
}

#[tokio::test]
async fn include_usage_flag_controls_stream_options() {
    // Absent: the default policy. 127.0.0.1 is not api.openai.com, so usage is off.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;
    assert!(server.requests()[0].json().get("stream_options").is_none());

    // --include-usage: on.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--include-usage",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(
        server.requests()[0].json()["stream_options"],
        json!({"include_usage": true})
    );

    // --include-usage=false: explicitly off.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--include-usage=false",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert!(server.requests()[0].json().get("stream_options").is_none());
}

// ---------- context budget (ADR 0018): a computed input budget, never a stop ----------

/// A tool call reply that also reports provider usage for the request it answers.
fn used_tool_reply(id: &str, name: &str, arguments: &str, input: u64) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 1, "total_tokens": input + 1}}),
    ])
}

/// 1: without `--context-window` no budget exists; the startup says so, and the turn runs.
#[tokio::test]
async fn without_a_window_the_budget_is_off_and_says_so() {
    let (_dir, ws) = budget_workspace("budget-off");
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 3300),
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
    assert!(stderr.contains("[budget] window unknown"), "{stderr}");
    assert!(
        !stderr.contains("ADR"),
        "the budget line names no ADR: {stderr}"
    );
    // No budget is invented: `B` is unknown, not a guess (ADR 0018).
    assert!(stderr.contains("B ?"), "{stderr}");
    assert!(stderr.contains("no budget"), "{stderr}");
    // The turn ran to completion: both requests were sent.
    assert_eq!(server.requests().len(), 2, "{stderr}");
}

/// ADR 0015 (a zero window is unset, never a real zero): `[model] context_window = 0` leaves
/// the window unknown in `radian exec`, exactly like the key being absent, and the turn runs
/// with the guard off. A reader that refuses `0` fails startup here; a host that read `0` as a
/// real window would carry a zero budget instead of no budget.
#[tokio::test]
async fn a_zero_context_window_in_the_config_is_unset_and_the_guard_is_off() {
    let (dir, ws) = budget_workspace("budget-zero-config");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[model]\ncontext_window = 0\n",
    );
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
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
    assert!(stdout.contains("done"), "{stdout}");
    assert!(stderr.contains("[budget] window unknown"), "{stderr}");
    assert!(stderr.contains("no budget"), "{stderr}");
    assert!(
        !stderr.contains("positive integer"),
        "a zero window is unset, not a config error: {stderr}"
    );
    assert_eq!(server.requests().len(), 1, "{stderr}");
}

/// ADR 0015: the flag's zero is a **usage error**, not the config's spelling of "unset". This
/// run gives the catalog a cached window (`C 20000, I 18000`, i.e. `B 15000`), so a host that
/// read `--context-window 0` as "not given" would start the turn with the catalog's window:
/// the test fails on the exit code, on the missing refusal and on the request the mock never
/// should have seen.
#[tokio::test]
async fn a_zero_context_window_flag_is_a_usage_error() {
    let (dir, ws) = budget_workspace("budget-zero-flag");
    // The cache the run would read (`--offline-catalog`), in the shape models.dev serves.
    let home = dir.join("radian");
    let cache = home.join("cache/catalog/models.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(
        &cache,
        r#"{"acme": {"id": "acme", "models": {"m": {"id": "m",
            "limit": {"context": 20000, "input": 18000, "output": 4096}}}}}"#,
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--catalog-provider",
                "acme",
                "--offline-catalog",
                "--context-window",
                "0",
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("--context-window must be at least 1 token"),
        "{stderr}"
    );
    assert!(!stdout.contains("done"), "{stdout}");
    assert_eq!(
        server.requests().len(),
        0,
        "the turn must not run: {stderr}"
    );
}

/// 2: over the input budget `B` the request is **still sent** and the turn completes
/// (ADR 0018 replaced the stop). `B = C - R = 20000 - 16384 = 3616`, and the reported input
/// (9000) is far over it.
#[tokio::test]
async fn an_over_budget_request_is_still_sent_and_the_turn_completes() {
    let (_dir, ws) = budget_workspace("budget-over");
    // C = 20000, so R = 5000 and B = 15000 (ADR 0018); the reported input (20000)
    // is over it.
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 20000),
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
                "--yes",
                "--context-window",
                "20000",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("B 15000 from --context-window"), "{stderr}");
    assert_eq!(
        server.requests().len(),
        2,
        "an over-budget request must still be sent: {stderr}"
    );
}

/// 3: a context window that cannot hold the output reservation is a startup error, before the
/// session file or any request (ADR 0018). With adaptive `R` it needs the
/// request's own output to reach `C`: Anthropic sends `max_tokens = 16384` by default, so an
/// 8192-token window cannot hold the reservation.
#[tokio::test]
async fn a_context_window_at_or_below_the_reservation_is_rejected() {
    let (_dir, ws) = budget_workspace("budget-too-small");
    let server = anth::MockServer::start(vec![]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "claude-x",
                "--provider",
                "anthropic",
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "8192",
                "-p",
                "go",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("not larger than the output reservation"),
        "{stderr}"
    );
    assert!(server.requests().is_empty(), "{stderr}");
}

/// 4: an over-budget turn records the normal outcome: no `context_budget`, and no
/// `ContextBudgetReached` audit event is written (ADR 0018). The event type still exists for
/// reading old journals.
#[tokio::test]
async fn an_over_budget_turn_records_a_normal_outcome() {
    let sessions = fresh_session_dir();
    let (_dir, ws) = budget_workspace("budget-durable");
    // C = 20000 -> B = 15000; the reported input (20000) is over it.
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 20000),
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
                "--yes",
                "--context-window",
                "20000",
                "--session-id",
                "s-budget",
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

    let raw = std::fs::read_to_string(sessions.join("s-budget.jsonl")).unwrap();
    assert!(
        !raw.contains("ContextBudgetReached"),
        "no stop event may be written:\n{raw}"
    );
    assert!(
        !raw.contains("\"context_budget\""),
        "the outcome must not be a budget stop:\n{raw}"
    );
    assert!(raw.contains("\"completed\""), "{raw}");
}

/// Acceptance 1 (CLI): a `--workspace` that does not exist is a workspace error that names
/// the path, does not blame the sandbox, does not suggest `full-access`, and creates no
/// session file (dogfooding D1/D2).
#[tokio::test]
async fn a_missing_workspace_is_a_workspace_error_and_leaves_no_session() {
    let dir = scratch("missing-ws");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let missing = dir.join("no-such-workspace");
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--workspace",
                missing.to_str().unwrap(),
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "missing-ws",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains(missing.to_str().unwrap()), "{stderr}");
    assert!(
        !stderr.contains("sandbox backend"),
        "a bad workspace must not blame the sandbox: {stderr}"
    );
    assert!(
        !stderr.contains("--profile"),
        "a bad workspace must not suggest a profile (old or new name): {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert_session_dir_empty(&session_dir);
}

/// ADR 0020: a `--session-dir` that is an ancestor of the workspace refuses at startup with
/// the same exit code as the other sandbox refusals, names the problem (not the backend), and
/// leaves no session file.
#[tokio::test]
async fn a_session_dir_above_the_workspace_is_refused_and_leaves_no_session() {
    let dir = scratch("nested-ws");
    let session_dir = dir.join("sessions");
    let ws = session_dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "nested-ws",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("workspace is inside"), "{stderr}");
    assert!(
        !stderr.contains("sandbox backend"),
        "a nested workspace must not blame the backend: {stderr}"
    );
    assert!(
        !stderr.contains("--profile"),
        "a nested workspace must not suggest a profile: {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    // The workspace lives inside the session dir, so the dir is not empty; the point is that
    // no session file was created.
    assert!(
        !session_dir.join("nested-ws.jsonl").exists(),
        "a failed start left a session file behind"
    );
}

/// Acceptance 2 and 4 (CLI): a failed sandbox probe refuses, leaves no session file, and the
/// same `--session-id` with corrected parameters then creates the session (dogfooding D2).
#[tokio::test]
async fn a_failed_probe_leaves_no_session_and_the_retry_creates_one() {
    let dir = scratch("probe-no-session");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;
    let base_url = server.base_url.clone();
    let ws_str = ws.to_str().unwrap().to_owned();
    let sessions = session_dir.to_str().unwrap().to_owned();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                &base_url,
                "--workspace",
                &ws_str,
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "hi",
                "--session-dir",
                &sessions,
                "--session-id",
                "retry",
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
    assert_session_dir_empty(&session_dir);

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                &base_url,
                "--workspace",
                &ws_str,
                "--yes",
                "-p",
                "hi",
                "--session-dir",
                &sessions,
                "--session-id",
                "retry",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        session_dir.join("retry.jsonl").exists(),
        "the corrected retry created the session"
    );
}

/// Acceptance 3 (CLI): a malformed `--mcp-config` fails before the session file exists.
#[tokio::test]
async fn a_bad_mcp_config_leaves_no_session() {
    let dir = scratch("bad-mcp");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let bad = write_file(&dir, "bad.json", "{\"servers\": [], \"bogus\": 1}");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--workspace",
                ws.to_str().unwrap(),
                "--mcp-config",
                bad.to_str().unwrap(),
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "bad-mcp",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains(bad.to_str().unwrap()), "{stderr}");
    assert_session_dir_empty(&session_dir);
}

// ---------- provider selection: anthropic (Phase 7 / anthost) ----------

/// `--provider anthropic` talks the Messages API: `/v1/messages`, `x-api-key` and
/// `anthropic-version`, and the reply is shown.
#[tokio::test]
async fn anthropic_provider_sends_a_messages_request_and_shows_the_reply() {
    let server = anth::MockServer::start(vec![anth_text_reply("ANTHROPIC-REPLY-OK")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "claude-x", "--provider", "anthropic", "-p", "hi"],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ANTHROPIC-REPLY-OK"), "{stdout}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some("sk-ant-test-key"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    let body = requests[0].json();
    assert_eq!(body["model"], "claude-x");
    // `max_tokens` is required by the Messages API and the host fills it in.
    assert_eq!(body["max_tokens"], 16384, "{body}");
    // ADR 0026 D11: the default level is `medium`, and the anthropic descriptor's provider
    // map sends it as the existing `adaptive` setting.
    assert_eq!(body["thinking"], json!({"type": "adaptive"}), "{body}");
}

/// Without `--api-key-env`, `--provider anthropic` reads `ANTHROPIC_API_KEY`; the key never
/// appears in the output or the session file.
#[tokio::test]
async fn anthropic_reads_anthropic_api_key_env_by_default_and_never_leaks_it() {
    let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--session-id",
                "anthro-env",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-default-env-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        server.requests()[0].header("x-api-key"),
        Some("sk-ant-default-env-key")
    );
    for text in [&stdout, &stderr] {
        assert!(
            !text.contains("sk-ant-default-env-key"),
            "the key leaked into the output: {text}"
        );
    }
    let session = std::fs::read_to_string(dir.join("anthro-env.jsonl")).unwrap();
    assert!(
        !session.contains("sk-ant-default-env-key"),
        "the key reached the session file"
    );
}

/// ADR 0026 D11 (review A1): every level has its own wire value on Anthropic — `adaptive` for
/// the default `medium`, `enabled:<budget>` for the rest — and a legacy
/// `--thinking enabled:<budget>` keeps the user's budget. `off` sends no `thinking` key,
/// asserted in the messages test below.
#[tokio::test]
async fn anthropic_thinking_modes_are_sent_in_the_body() {
    for (mode, expected) in [
        ("adaptive", json!({"type": "adaptive"})),
        ("high", json!({"type": "enabled", "budget_tokens": 4096})),
        ("max", json!({"type": "enabled", "budget_tokens": 12288})),
        (
            "enabled:8192",
            json!({"type": "enabled", "budget_tokens": 8192}),
        ),
    ] {
        let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "claude",
                    "--provider",
                    "anthropic",
                    "--thinking",
                    mode,
                    "-p",
                    "hi",
                ],
                &[
                    ("ANTHROPIC_API_KEY", "sk-ant-thinking"),
                    ("RADIAN_BASE_URL", &server.base_url),
                ],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{mode}: {stderr}");
        assert_eq!(server.requests()[0].json()["thinking"], expected, "{mode}");
    }
}

/// The session config snapshot names the real provider; the default provider is unchanged.
#[tokio::test]
async fn the_session_snapshot_names_the_real_provider() {
    // anthropic
    let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--session-id",
                "snap-ant",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-snap"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let session = std::fs::read_to_string(dir.join("snap-ant.jsonl")).unwrap();
    assert!(session.contains("\"provider\":\"anthropic\""), "{session}");

    // openai-compatible (the default, no --provider flag)
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "snap-oai",
                "-p",
                "hi",
            ],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let session = std::fs::read_to_string(dir.join("snap-oai.jsonl")).unwrap();
    assert!(
        session.contains("\"provider\":\"openai-compatible\""),
        "{session}"
    );
}

/// The default provider still sends an OpenAI-compatible request: `/v1/chat/completions`
/// (the adapter's path) and a bearer token.
#[tokio::test]
async fn the_default_provider_still_sends_an_openai_request() {
    let server = MockServer::start(vec![text_reply("OPENAI-REPLY")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("OPENAI-REPLY"), "{stdout}");
    let request = &server.requests()[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
}

/// `--thinking enabled:<budget>` is checked before the session file exists: the budget must be
/// `>= 1024` and `< --max-tokens` (Anthropic's rule, wire notes §2.3). Both a budget equal to
/// `max_tokens` (16384 default) and one below 1024 are usage errors and leave no session file.
#[tokio::test]
async fn the_legacy_enabled_budget_is_accepted_and_clamped_like_high() {
    // ADR 0026 D11 (review A1): the legacy `enabled:<budget>` is accepted for one version with
    // a warning and the user's budget is sent **as-is** (not replaced by the level's budget).
    let server = anth::MockServer::start(vec![anth_text_reply("LEGACY-OK")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--thinking",
                "enabled:8192",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-budget"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("LEGACY-OK"), "{stdout}");
    assert!(
        stderr.contains("deprecated"),
        "the legacy spelling warns: {stderr}"
    );
    let body = server.requests()[0].json();
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192}),
        "{body}"
    );
}

/// Review B1: a **level's** budget is clamped to `--max-tokens - 1` instead of failing the
/// build, and the completion hides the levels only when no budget can fit at all. The user's
/// explicit legacy budget is not clamped.
#[tokio::test]
async fn a_level_budget_clamps_to_max_tokens() {
    // `--max-tokens 4096` + `--thinking max` (12288 in the map) -> 4095 on the wire.
    let server = anth::MockServer::start(vec![anth_text_reply("CLAMPED")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--max-tokens",
                "4096",
                "--thinking",
                "max",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-clamp"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("CLAMPED"), "{stdout}");
    let body = server.requests()[0].json();
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 4095}),
        "{body}"
    );

    // `--max-tokens 1024` leaves no room for any budget: an `enabled` level clamps to a
    // budget-less one, so the request has no `enabled` setting.
    let server = anth::MockServer::start(vec![anth_text_reply("NO-ROOM")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--max-tokens",
                "1024",
                "--thinking",
                "high",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-floor"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("NO-ROOM"), "{stdout}");
    let body = server.requests()[0].json();
    assert!(
        !body["thinking"].to_string().contains("enabled"),
        "no enabled budget fits 1024: {body}"
    );

    // The user's explicit legacy budget is **not** clamped: over-limit stays a usage error.
    let dir = fresh_session_dir();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--max-tokens",
                "4096",
                "--thinking",
                "enabled:99999",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-legacy")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("out of range"), "{stderr}");
}

// ---------- model catalog: the startup window line (ADR 0018) ----------

const CATALOG_JSON: &str = r#"{
  "opencode": {
    "id": "opencode",
    "models": {
      "deepseek-v4.1-flash": {"id": "deepseek-v4.1-flash",
        "limit": {"context": 1000000, "input": 922000, "output": 384000}}
    }
  }
}"#;

/// The explicit window is named on startup and wins over the catalog. ADR 0018 shows `C`, `I`,
/// `output`, `R`, and the effective input budget `B` separately.
#[tokio::test]
async fn the_startup_line_names_an_explicit_window() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--context-window",
                "20000",
                "--offline-catalog",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    // C = 20000 -> R = 5000 (a quarter of the window), B = 15000.
    assert!(stderr.contains("B 15000 from --context-window"), "{stderr}");
    assert!(stderr.contains("R 5000"), "{stderr}");
}

/// `--no-auto-compact` is shown on the startup line; the default is on (ADR 0018).
#[tokio::test]
async fn the_startup_line_names_compaction_state() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--context-window",
                "20000",
                "--offline-catalog",
                "--no-auto-compact",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("automatic compaction off"), "{stderr}");

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--context-window",
                "20000",
                "--offline-catalog",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("automatic compaction on"), "{stderr}");
}

/// With no window and an empty cache, there is no budget and the startup says so.
#[tokio::test]
async fn the_startup_line_is_unknown_without_a_window() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let home = scratch("no-window-home");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--offline-catalog",
                "-p",
                "hi",
            ],
            &[("HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[budget] window unknown"), "{stderr}");
    assert!(stderr.contains("no budget"), "{stderr}");
}

/// The catalog line names the provider/model and the numbers, and the numbers show separately.
#[tokio::test]
async fn the_startup_line_names_the_catalog() {
    let provider = MockServer::start(vec![text_reply("ok")]).await;
    let catalog = MockServer::start(vec![Reply::status(200, CATALOG_JSON)]).await;
    let catalog_url = format!("{}/api.json", catalog.base_url);
    let home = scratch("catalog-home");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "deepseek-v4.1-flash",
                "--base-url",
                &provider.base_url,
                "--catalog-provider",
                "opencode",
                "-p",
                "hi",
            ],
            &[
                ("HOME", home.to_str().unwrap()),
                ("RADIAN_CATALOG_URL", catalog_url.as_str()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("B 850000 from models.dev (opencode/deepseek-v4.1-flash"),
        "{stderr}"
    );
    assert!(stderr.contains("C 1000000"), "{stderr}");
    assert!(stderr.contains("I 922000"), "{stderr}");
    assert!(stderr.contains("output 384000"), "{stderr}");
    // R is the output actually sent: min(384000, max(150000, min(16384, 1000000 / 4)))
    // = 150000.
    assert!(stderr.contains("R 150000"), "{stderr}");
    assert_eq!(
        provider.requests()[0].json()["max_tokens"],
        json!(150000),
        "the catalog cap is sent as max_tokens"
    );
    assert_eq!(catalog.requests().len(), 1, "the catalog was fetched once");
}

/// ADR 0018 §2: an explicit `max_tokens` from `RADIAN_EXTRA_BODY` wins on the wire, so the
/// reservation counts it instead of the computed chat cap. C = 128000 with O = 384000 would
/// send 19200; the body's 32000 is what the request sends, so R = 32000 and B = 96000.
#[tokio::test]
async fn an_explicit_max_tokens_raises_the_reservation_it_counts() {
    let provider = MockServer::start(vec![text_reply("ok")]).await;
    let home = common::process_dir("radian-exec-override-home");
    let cache = home.join("cache/catalog");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("models.json"), CATALOG_JSON).unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "deepseek-v4.1-flash",
                "--base-url",
                &provider.base_url,
                "--catalog-provider",
                "opencode",
                "--context-window",
                "128000",
                "-p",
                "hi",
            ],
            &[
                ("RADIAN_HOME", home.to_str().unwrap()),
                ("RADIAN_EXTRA_BODY", r#"{"max_tokens":32000}"#),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("output 384000"), "{stderr}");
    assert!(stderr.contains("R 32000"), "{stderr}");
    assert!(stderr.contains("B 96000"), "{stderr}");
    assert_eq!(
        provider.requests()[0].json()["max_tokens"],
        json!(32000),
        "the explicit value is what the request sends"
    );
}

// ---------- named providers (ADR 0026 D10) ----------

/// Writes an `auth.json` whose entries are **named providers**: each name carries its own
/// key and its own endpoint in `env` (ADR 0026 D10).
fn write_named_auth_json(root: &Path, entries: &[(&str, &str, &str)]) {
    let dir = root.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut map = serde_json::Map::new();
    for (name, key, base_url) in entries {
        map.insert(
            (*name).to_string(),
            serde_json::json!({
                "type": "api_key",
                "key": key,
                "env": { "RADIAN_BASE_URL": base_url, "RADIAN_KIND": "openai-compatible" }
            }),
        );
    }
    let path = dir.join("auth.json");
    std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Two custom endpoints, two keys: each named provider starts against its own mock server and
/// sends **its own** key (D10's whole point — the slots are no longer shared). And when the
/// config also declares the same name, the config wins with a one-line warning.
#[tokio::test]
async fn named_providers_keep_separate_keys_and_config_wins_over_auth() {
    // Two replies each: the third sub-test reuses the mocks (the config-wins run).
    let corp = MockServer::start(vec![text_reply("CORP"), text_reply("CORP")]).await;
    let relay = MockServer::start(vec![text_reply("RELAY"), text_reply("RELAY")]).await;
    let home = fresh_session_dir();
    write_named_auth_json(
        &home,
        &[
            ("corp", "sk-corp-111", &corp.base_url),
            ("relay", "sk-relay-222", &relay.base_url),
        ],
    );
    let envs = |extra: &[(&str, &str)]| {
        let mut all: Vec<(String, String)> = vec![
            ("RADIAN_HOME".into(), home.to_string_lossy().into_owned()),
            (
                "RADIAN_SESSION_DIR".into(),
                fresh_session_dir().to_string_lossy().into_owned(),
            ),
            (
                "HOME".into(),
                fresh_session_dir().to_string_lossy().into_owned(),
            ),
            ("RADIAN_CATALOG_URL".into(), common::fast_catalog_url()),
        ];
        all.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        all
    };

    // `corp` talks to its own endpoint with its own key.
    let child = spawn_radian_raw(
        &["--provider", "corp", "--model", "m-1", "-p", "hi"],
        &envs(&[]),
    );
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "CORP\n");
    let requests = corp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-corp-111"),
        "corp must send its own key"
    );
    assert_eq!(relay.requests().len(), 0, "relay is untouched");

    // `relay` likewise, and `corp` still sees exactly one request.
    let child = spawn_radian_raw(
        &["--provider", "relay", "--model", "m-1", "-p", "hi"],
        &envs(&[]),
    );
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "RELAY\n");
    let requests = relay.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-relay-222"),
        "relay must send its own key"
    );
    assert_eq!(corp.requests().len(), 1, "relay ran, corp did not");

    // The config's `[providers.corp]` wins over the auth.json entry of the same name, with a
    // warning naming the conflict (D10). The config points `corp` at the *relay* mock.
    let config_dir = home.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "schema_version = 1\n[model]\nprovider = \"corp\"\nmodel = \"m-1\"\n\
             [providers.corp]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n",
            relay.base_url
        ),
    )
    .unwrap();
    let child = spawn_radian_raw(&["--model", "m-1", "-p", "hi"], &envs(&[]));
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "RELAY\n", "the config's base_url won");
    assert!(
        stderr.contains("[provider]"),
        "the conflict must warn once: {stderr}"
    );
    assert!(stderr.contains("corp"), "{stderr}");
}

/// A `spawn_radian` that takes the environment verbatim (the helper above derives it from key
/// env vars; named providers carry their own in `auth.json`).
fn spawn_radian_raw(args: &[&str], envs: &[(String, String)]) -> Child {
    Command::new(common::radian_bin())
        .arg("exec")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn radian")
}

// ---------- every provider table at startup (ADR 0026 D10) ----------

/// The environment a named-provider run needs: its own `~/.radian` root and session directory,
/// a fresh `HOME`, and the fast-failing catalog sink. `RADIAN_CONFIG` is not set, so the
/// config can only come from `RADIAN_HOME/config/config.toml`.
fn provider_home_envs(home: &Path) -> Vec<(String, String)> {
    vec![
        ("RADIAN_HOME".into(), home.to_string_lossy().into_owned()),
        (
            "HOME".into(),
            fresh_session_dir().to_string_lossy().into_owned(),
        ),
        (
            "RADIAN_SESSION_DIR".into(),
            fresh_session_dir().to_string_lossy().into_owned(),
        ),
        ("RADIAN_CATALOG_URL".into(), common::fast_catalog_url()),
    ]
}

/// Writes `<home>/config/config.toml` (the file `RADIAN_HOME` selects).
fn write_home_config(home: &Path, text: &str) {
    let dir = home.join("config");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

/// A `[providers.orphan]` rules-only table is checked at **startup**, whichever provider the run
/// selects: with no `auth.json` entry named `orphan` it is refused with one line naming
/// `providers.orphan`, exit code 2 like every other config refusal, and no request is sent.
#[tokio::test]
async fn a_rules_only_provider_table_without_an_auth_entry_is_refused_at_startup() {
    let server = MockServer::start(vec![text_reply("SHOULD-NOT-RUN")]).await;
    let dir = fresh_session_dir();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        concat!(
            "schema_version = 1\n",
            "[model]\n",
            "provider = \"openai-compatible\"\n",
            "model = \"m\"\n",
            "[providers.orphan]\n",
            "prefer = [\"anthropic\"]\n",
        ),
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--base-url", &server.base_url, "-p", "hi"],
            &[
                ("RADIAN_CONFIG", config.to_str().unwrap()),
                ("OPENAI_API_KEY", "sk-test"),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(
        stderr.contains("adds to an `auth.json` provider that does not exist"),
        "{stderr}"
    );
    assert!(
        server.requests().is_empty(),
        "a refused table must send no request: {stderr}"
    );
}

/// `reasoning_replay` alone is rules-only by the documented definition, so an orphan table with
/// only that key is refused at startup too.
#[tokio::test]
async fn a_reasoning_replay_only_provider_table_without_an_auth_entry_is_refused_at_startup() {
    let dir = fresh_session_dir();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        concat!(
            "schema_version = 1\n",
            "[model]\n",
            "provider = \"openai-compatible\"\n",
            "model = \"m\"\n",
            "[providers.orphan]\n",
            "reasoning_replay = \"reasoning_content\"\n",
        ),
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["-p", "hi"],
            &[
                ("RADIAN_CONFIG", config.to_str().unwrap()),
                ("OPENAI_API_KEY", "sk-test"),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
}

/// With the same-name `auth.json` entry, the same rules-only table starts **while the run
/// selects another provider**, and adds no "config wins" warning (an overlay never warns). The
/// same config without the entry is refused by the same startup check, so this asserts the
/// startup-wide check: without it the second run starts and the turn runs.
#[tokio::test]
async fn a_rules_only_provider_table_with_an_auth_entry_starts_while_another_provider_is_selected()
{
    let server = MockServer::start(vec![text_reply("BUILT-IN"), text_reply("BUILT-IN")]).await;
    let orphan = MockServer::start(vec![text_reply("ORPHAN")]).await;
    let home = fresh_session_dir();
    write_named_auth_json(
        &home,
        &[
            ("openai-compatible", "sk-test", &server.base_url),
            ("orphan", "sk-orphan-1", &orphan.base_url),
        ],
    );
    write_home_config(
        &home,
        "schema_version = 1\n[providers.orphan]\nprefer = [\"anthropic\"]\n",
    );
    let args = ["--base-url", &server.base_url, "--model", "m-1", "-p", "hi"];
    let child = spawn_radian_raw(&args, &provider_home_envs(&home));
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "BUILT-IN\n");
    assert!(
        !stderr.contains("declared in both config and auth.json"),
        "an overlay must not warn: {stderr}"
    );
    assert!(
        !stderr.contains("adds to an `auth.json` provider"),
        "{stderr}"
    );
    assert!(
        orphan.requests().is_empty(),
        "the unselected overlay's provider must not be resolved: {stderr}"
    );

    // The same table, the same selected provider, without the entry: refused at startup.
    write_named_auth_json(&home, &[("openai-compatible", "sk-test", &server.base_url)]);
    let child = spawn_radian_raw(&args, &provider_home_envs(&home));
    let (code, _, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(
        stderr.contains("adds to an `auth.json` provider that does not exist"),
        "{stderr}"
    );
}

/// A full declaration (`kind` + `base_url`) needs no `auth.json` entry: the startup check skips
/// it, so with a rules-only table beside it the refusal names only the rules-only one, and with
/// the full table alone the run selects another provider and starts. Dropping the `is_full`
/// guard makes the check refuse `corp` instead.
#[tokio::test]
async fn a_full_provider_table_without_an_auth_entry_is_not_refused_at_startup() {
    let server = MockServer::start(vec![text_reply("BUILT-IN")]).await;
    let corp = MockServer::start(vec![text_reply("CORP")]).await;
    let home = fresh_session_dir();
    write_named_auth_json(&home, &[("openai-compatible", "sk-test", &server.base_url)]);
    let full = format!(
        "schema_version = 1\n[providers.corp]\nkind = \"openai-compatible\"\n\
         base_url = \"{}\"\n",
        corp.base_url
    );

    // Both tables, neither name in `auth.json`: the refusal names the rules-only table only.
    write_home_config(
        &home,
        &format!("{full}[providers.orphan]\nprefer = [\"anthropic\"]\n"),
    );
    let child = spawn_radian_raw(
        &["--base-url", &server.base_url, "--model", "m-1", "-p", "hi"],
        &provider_home_envs(&home),
    );
    let (code, _, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(
        !stderr.contains("providers.corp"),
        "a full declaration is not rules-only: {stderr}"
    );

    // The full table alone: it starts while another provider is selected, and is never resolved.
    write_home_config(&home, &full);
    let child = spawn_radian_raw(
        &["--base-url", &server.base_url, "--model", "m-1", "-p", "hi"],
        &provider_home_envs(&home),
    );
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "BUILT-IN\n");
    assert!(
        corp.requests().is_empty(),
        "the unselected full declaration must not be resolved: {stderr}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{stderr}");
    assert_eq!(requests[0].json()["model"], "m-1");
}

/// A read-only configuration (a newer `schema_version`) gets the same refusal as every other
/// provider-table refusal: the reader's own provider errors refuse in read-only mode too, and
/// this check runs after the parse.
#[tokio::test]
async fn a_read_only_config_with_an_orphan_rules_only_table_is_refused_at_startup() {
    let dir = fresh_session_dir();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        concat!(
            "schema_version = 2\n",
            "[providers.orphan]\n",
            "prefer = [\"anthropic\"]\n",
        ),
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["-p", "hi"],
            &[
                ("RADIAN_CONFIG", config.to_str().unwrap()),
                ("OPENAI_API_KEY", "sk-test"),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
}

// ---------- RADIAN_EXTRA_BODY (ADR 0026 D13) ----------

/// The environment value reaches the request body through the host, and startup names the
/// keys applied — never their values.
#[tokio::test]
async fn the_extra_body_env_reaches_the_request_and_the_startup_line_names_the_keys() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_EXTRA_BODY", r#"{"thinking":{"type":"enabled"}}"#)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "ok\n");
    let body = server.requests()[0].json();
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    assert!(
        stderr.contains("[body] RADIAN_EXTRA_BODY sets: thinking"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("enabled"),
        "startup names the keys, never their values: {stderr}"
    );
}

/// A reserved key, a non-object and an output cap the reservation cannot hold are startup
/// refusals naming the variable (and the key), before any session file exists. Exit code 2 is
/// the code every startup refusal uses.
#[tokio::test]
async fn a_bad_extra_body_stops_startup_with_the_named_error() {
    for (value, needle) in [
        ("{\"messages\":[]}", "messages"),
        ("[1]", "object"),
        // ADR 0018 §2: the request would send this output while `R` could only hold `u32::MAX`.
        ("{\"max_completion_tokens\":5000000000}", "4294967295"),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    "http://127.0.0.1:9/v1",
                    "-p",
                    "hi",
                ],
                &[("RADIAN_EXTRA_BODY", value)],
            ),
            "",
        )
        .await;
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("RADIAN_EXTRA_BODY"), "{value}: {stderr}");
        assert!(stderr.contains(needle), "{value}: {stderr}");
    }
}

/// ADR 0026 D13: reserved keys are checked per route. A chat route accepts `max_tokens` and
/// the captured request carries it; the Anthropic route reserves it, so the same value stops
/// startup with exit 2 and a line naming the variable and the key.
#[tokio::test]
async fn the_extra_body_max_tokens_is_allowed_on_a_chat_route_and_refused_on_anthropic() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_EXTRA_BODY", r#"{"max_tokens":32000}"#)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "ok\n");
    let body = server.requests()[0].json();
    assert_eq!(body["max_tokens"], 32000, "{body}");

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "claude-x", "--provider", "anthropic", "-p", "hi"],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_EXTRA_BODY", r#"{"max_tokens":32000}"#),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("RADIAN_EXTRA_BODY"), "{stderr}");
    assert!(stderr.contains("max_tokens"), "{stderr}");
}

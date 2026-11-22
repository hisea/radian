//! End-to-end tests of `radian exec` with MCP servers: the user `[mcp]` table and the project `.mcp.json`.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_openai_compatible::testing::MockServer;
use serde_json::json;
use std::path::{Path, PathBuf};
use support::*;

// ---------- user [mcp] (ADR 0008 §8) ----------

/// Writes a user `config.toml` whose `[mcp]` points one `fx` server at the fixture,
/// logging to `log`. `form` is `"servers"` (`[[mcp.servers]]`) or `"mcpServers"`
/// (`[mcp.mcpServers.fx]`).
fn mcp_user_config(dir: &Path, form: &str, log: &Path) -> PathBuf {
    let fixture = fixture_bin();
    let body = match form {
        "servers" => format!(
            "schema_version = 1\n[[mcp.servers]]\nid = \"fx\"\ncommand = \"{}\"\n\
             [mcp.servers.env]\nFIXTURE_LOG = \"{}\"\n",
            fixture.display(),
            log.display()
        ),
        _ => format!(
            "schema_version = 1\n[mcp.mcpServers.fx]\ncommand = \"{}\"\n\
             [mcp.mcpServers.fx.env]\nFIXTURE_LOG = \"{}\"\n",
            fixture.display(),
            log.display()
        ),
    };
    write_file(dir, "user-config.toml", &body)
}

/// `[mcp]` with `[[mcp.servers]]` reaches the session: the fixture echo runs with no
/// `--mcp-config` on the command line.
#[tokio::test]
async fn mcp_servers_from_the_user_config_run() {
    let dir = scratch("mcp-user-servers");
    let log = dir.join("fixture.log");
    let config = mcp_user_config(&dir, "servers", &log);
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

/// `[mcp]` with `[mcp.mcpServers.fx]` reaches the session the same way.
#[tokio::test]
async fn mcp_mcp_servers_from_the_user_config_run() {
    let dir = scratch("mcp-user-shared");
    let log = dir.join("fixture.log");
    let config = mcp_user_config(&dir, "mcpServers", &log);
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

/// On an id collision the `--mcp-config` file wins: only its server process ever starts
/// (the config's `FIXTURE_LOG` stays empty), and one `[mcp]` warning names the id.
#[tokio::test]
async fn an_mcp_id_collision_uses_the_command_line_with_one_warning() {
    let dir = scratch("mcp-collision");
    let config_log = dir.join("config-fixture.log");
    let config_path = mcp_user_config(&dir, "servers", &config_log);
    let file_log = dir.join("file-fixture.log");
    let mut file_env = serde_json::Map::new();
    file_env.insert(
        "FIXTURE_LOG".to_owned(),
        serde_json::Value::String(file_log.to_string_lossy().into_owned()),
    );
    let file = write_file(
        &dir,
        "mcp.json",
        &json!({"servers": [{
            "id": "fx",
            "command": fixture_bin(),
            "env": file_env,
        }]})
        .to_string(),
    );
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config_path.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                file.to_str().unwrap(),
                "-p",
                "go",
                "--yes",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
    // The file's server ran; the config's never started.
    assert!(
        !log_lines(&file_log).is_empty(),
        "the winning server must log its methods"
    );
    assert!(
        std::fs::read_to_string(&config_log)
            .unwrap_or_default()
            .is_empty(),
        "the config's colliding server must never start"
    );
    let collisions: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("[mcp]") && line.contains("--mcp-config"))
        .collect();
    assert_eq!(collisions.len(), 1, "{stderr}");
    assert!(collisions[0].contains("`fx`"), "{stderr}");
}

/// Both `[mcp]` forms at once is a config error: startup refuses before any session file
/// exists, like a malformed `--mcp-config`.
#[tokio::test]
async fn both_mcp_config_forms_are_refused_before_any_session() {
    let dir = scratch("mcp-both-forms");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let config = write_file(
        &dir,
        "user-config.toml",
        "schema_version = 1\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n[mcp.mcpServers]\n",
    );
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "mcp-both-forms",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("cannot be combined"), "{stderr}");
    assert_session_dir_empty(&session_dir);
}

/// `--no-config-mcp` (the `explore` subagent child's flag) ignores the config file's
/// `[mcp]`: the fixture server never starts, so the model is told there is no such
/// tool. The same config without the flag starts the server (the control, so the test
/// cannot pass for the wrong reason: a broken fixture path would fail the control, not
/// silently pass the flag case).
#[tokio::test]
async fn no_config_mcp_ignores_the_config_mcp_section() {
    for (name, no_config_mcp) in [("flag", true), ("control", false)] {
        let dir = scratch(&format!("no-config-mcp-{name}"));
        let log = dir.join("fixture.log");
        let config = mcp_user_config(&dir, "servers", &log);
        let server = MockServer::start(echo_then("done")).await;
        let mut args = vec![
            "--config",
            config.to_str().unwrap(),
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "go",
            "--yes",
        ];
        if no_config_mcp {
            args.push("--no-config-mcp");
        }

        let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;

        assert_eq!(code, 0, "{name}: {stderr}");
        let started = !std::fs::read_to_string(&log).unwrap_or_default().is_empty();
        if no_config_mcp {
            assert!(!started, "{name}: the config server must never start");
            assert_ne!(
                tool_result_content(&server, 1),
                r#"{"hello":"mcp"}"#,
                "{name}: no MCP tool may be offered"
            );
        } else {
            assert!(started, "{name}: the control must start the config server");
            assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
        }
    }
}

// ---------- project .mcp.json (slice B2) ----------

/// Writes `<ws>/.mcp.json` pointing one `fx` server at the fixture, in the shared
/// `mcpServers` form, with `extra_env` merged into the server's `env`.
fn mcp_project_config(ws: &Path, log: &Path, extra_env: &[(&str, &str)]) -> PathBuf {
    let mut env = serde_json::Map::new();
    env.insert(
        "FIXTURE_LOG".to_owned(),
        json!(log.to_string_lossy().into_owned()),
    );
    for (key, value) in extra_env {
        env.insert((*key).to_owned(), json!(value));
    }
    write_file(
        ws,
        ".mcp.json",
        &json!({"mcpServers": {"fx": {"command": fixture_bin(), "env": env}}}).to_string(),
    )
}

/// A minimal user config with no `[mcp]` section.
fn minimal_user_config(dir: &Path) -> PathBuf {
    write_file(dir, "user-config.toml", "schema_version = 1\n")
}

/// B2: an untrusted workspace starts no project server process (the fixture log stays
/// empty) and the startup line says the file was not loaded; `--trust` loads it (the
/// control, so the test cannot pass for the wrong reason: a broken fixture path would
/// fail the trusted case, not silently pass the untrusted one). The mutation (untrusted
/// loads too) fails the untrusted assertions.
#[tokio::test]
async fn project_mcp_loads_only_when_trusted() {
    for (name, trust) in [("untrusted", false), ("trusted", true)] {
        let dir = scratch(&format!("project-mcp-{name}"));
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let log = dir.join("fixture.log");
        mcp_project_config(&ws, &log, &[]);
        let config = minimal_user_config(&dir);
        let server = MockServer::start(echo_then("done")).await;
        let mut args = vec![
            "--config",
            config.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "go",
            "--yes",
        ];
        if trust {
            args.push("--trust");
        }
        let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 0, "{name}: {stderr}");
        let started = !std::fs::read_to_string(&log).unwrap_or_default().is_empty();
        if trust {
            assert!(
                stderr.contains("project .mcp.json: loaded 1 server"),
                "{name}: {stderr}"
            );
            assert!(started, "{name}: the project server must start");
            assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
        } else {
            assert!(!started, "{name}: untrusted must start no project server");
            assert!(
                stderr.contains("project .mcp.json not loaded (untrusted)"),
                "{name}: {stderr}"
            );
            assert_ne!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
        }
    }
}

/// B2: a trusted project that edits its `.mcp.json` gets the new content on restart with
/// no new question: run 1 loads the fixture server, run 2 (only a remote entry left)
/// loads nothing and warns skipped. The trust decision is seeded in a private
/// `RADIAN_HOME`, like a previous `/trust` choice.
#[tokio::test]
async fn a_trusted_project_reloads_an_edited_mcp_json_without_reasking() {
    let dir = scratch("project-mcp-restart");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let canonical = ws.canonicalize().unwrap();
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("trust")).unwrap();
    std::fs::write(
        home.join("trust/trust.json"),
        format!(r#"{{"{}": true}}"#, canonical.to_string_lossy()),
    )
    .unwrap();
    let config = minimal_user_config(&dir);
    let home_str = home.to_str().unwrap().to_owned();

    // Run 1: one stdio server at the fixture.
    let log = dir.join("fixture.log");
    mcp_project_config(&ws, &log, &[]);
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[("RADIAN_HOME", home_str.as_str())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("project .mcp.json: loaded 1 server"),
        "run 1: {stderr}"
    );
    assert!(
        !std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "run 1: the project server must start"
    );
    assert!(
        !stderr.contains("trust them?"),
        "run 1: must not re-ask: {stderr}"
    );
    drop(server);

    // Run 2: the project file now holds only a remote entry.
    write_file(
        &ws,
        ".mcp.json",
        r#"{"mcpServers":{"web": {"type": "http", "url": "https://example.com/mcp"}}}"#,
    );
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[("RADIAN_HOME", home_str.as_str())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("project .mcp.json: loaded 0 servers"),
        "run 2 reads the edited file: {stderr}"
    );
    assert!(stderr.contains("skipped `web`"), "run 2: {stderr}");
    assert!(
        !stderr.contains("trust them?"),
        "run 2: must not re-ask: {stderr}"
    );
}

/// B2: a credential variable in a project entry expands to empty end to end (the fixture
/// `env` tool reads `TOKEN` back), while the same entry as a user `[mcp]` section expands
/// as today. The mutation (project entries expand credentials) fails the first assertion.
#[tokio::test]
async fn project_credential_variables_expand_to_empty_end_to_end() {
    let replies = || {
        vec![
            tool_reply("call_1", "fx__env", r#"{"name":"PROBE"}"#),
            text_reply("done"),
        ]
    };
    // The project entry: blanked.
    let dir = scratch("project-mcp-credential");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let log = dir.join("fixture.log");
    // `PROBE` (not a `*_TOKEN`-style sensitive name): the user config below must parse —
    // a bare string on a sensitive key is refused as a plaintext secret (ADR 0008 §3).
    // The blanking keys on the *variable*, not the key, so the probe still proves it.
    mcp_project_config(&ws, &log, &[("PROBE", "${ANTHROPIC_API_KEY}")]);
    let config = minimal_user_config(&dir);
    let server = MockServer::start(replies()).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--trust",
            ],
            &[("ANTHROPIC_API_KEY", "sk-b2-live-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        tool_result_content(&server, 1),
        "",
        "the project server must not see the shell credential: {stderr}"
    );
    drop(server);
    // The same entry as a user `[mcp]` section: expanded as today.
    let dir = scratch("user-mcp-credential");
    let log = dir.join("fixture.log");
    let fixture = fixture_bin();
    let config = write_file(
        &dir,
        "user-config.toml",
        &format!(
            "schema_version = 1\n[mcp.mcpServers.fx]\ncommand = \"{}\"\n\
             [mcp.mcpServers.fx.env]\nFIXTURE_LOG = \"{}\"\nPROBE = \"${{ANTHROPIC_API_KEY}}\"\n",
            fixture.display(),
            log.display(),
        ),
    );
    let server = MockServer::start(replies()).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[("ANTHROPIC_API_KEY", "sk-b2-live-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        tool_result_content(&server, 1),
        "sk-b2-live-test",
        "user entries keep today's expansion: {stderr}"
    );
}

/// B2: `--no-config-mcp` (the `explore` child's flag) skips the project file even when
/// trusted: no process starts, one line says so.
#[tokio::test]
async fn no_config_mcp_skips_the_project_file() {
    let dir = scratch("project-mcp-no-config");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let log = dir.join("fixture.log");
    mcp_project_config(&ws, &log, &[]);
    let config = minimal_user_config(&dir);
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--trust",
                "--no-config-mcp",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "the project server must never start"
    );
    assert!(
        stderr.contains("project .mcp.json skipped (--no-config-mcp)"),
        "{stderr}"
    );
}

/// B2: the project file loses to both user sources: `--mcp-config` wins the `fx`
/// collision and `[mcp]` wins the `cfg` collision, each with one warning naming the id
/// and the winning side; only the winning servers' processes ever start.
#[tokio::test]
async fn a_project_collision_uses_the_command_line_with_one_warning() {
    let dir = scratch("project-mcp-collision");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The project file holds both colliding ids.
    let project_fx_log = dir.join("project-fx.log");
    let project_cfg_log = dir.join("project-cfg.log");
    let fixture = fixture_bin();
    write_file(
        &ws,
        ".mcp.json",
        &json!({"mcpServers": {
            "fx": {"command": fixture,
                    "env": {"FIXTURE_LOG": project_fx_log.to_string_lossy().into_owned()}},
            "cfg": {"command": fixture,
                    "env": {"FIXTURE_LOG": project_cfg_log.to_string_lossy().into_owned()}},
        }})
        .to_string(),
    );
    // `[mcp]` holds `cfg`; `--mcp-config` holds `fx`.
    let cfg_log = dir.join("cfg.log");
    let config = write_file(
        &dir,
        "user-config.toml",
        &format!(
            "schema_version = 1\n[mcp.mcpServers.cfg]\ncommand = \"{}\"\n\
             [mcp.mcpServers.cfg.env]\nFIXTURE_LOG = \"{}\"\n",
            fixture.display(),
            cfg_log.display(),
        ),
    );
    let file_fx_log = dir.join("file-fx.log");
    let file = write_file(
        &dir,
        "mcp.json",
        &json!({"servers": [{
            "id": "fx",
            "command": fixture_bin(),
            "env": {"FIXTURE_LOG": file_fx_log.to_string_lossy().into_owned()},
        }]})
        .to_string(),
    );
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--mcp-config",
                file.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--trust",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    for (log, name) in [
        (&project_fx_log, "project fx"),
        (&project_cfg_log, "project cfg"),
    ] {
        assert!(
            std::fs::read_to_string(log).unwrap_or_default().is_empty(),
            "the {name} server must never start"
        );
    }
    for (log, name) in [(&file_fx_log, "command-line fx"), (&cfg_log, "config cfg")] {
        assert!(
            !std::fs::read_to_string(log).unwrap_or_default().is_empty(),
            "the {name} server must start"
        );
    }
    assert!(
        stderr
            .contains("server `fx` is set in both .mcp.json and --mcp-config; using --mcp-config"),
        "{stderr}"
    );
    assert!(
        stderr.contains("server `cfg` is set in both .mcp.json and [mcp]; using [mcp]"),
        "{stderr}"
    );
}

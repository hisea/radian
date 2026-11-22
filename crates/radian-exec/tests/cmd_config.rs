//! End-to-end tests of `radian config` and the configuration file: the `~/.radian/` layout, schema checks, `config import-pi`, old memory-era data.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_openai_compatible::testing::MockServer;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use support::*;

// ---------- ADR 0020: the `~/.radian/` layout ----------

/// `RADIAN_HOME` moves the whole root; sessions land in `<RADIAN_HOME>/sessions`.
#[tokio::test]
async fn radian_home_places_sessions_under_the_radian_root() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let dir = scratch("radian-home");
    let radian_home = dir.join("rh");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "h1",
                "-p",
                "hi",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        radian_home.join("sessions/h1.jsonl").exists(),
        "the session must be under <RADIAN_HOME>/sessions"
    );
}

/// `RADIAN_SESSION_DIR` still wins over `RADIAN_HOME` for the sessions directory only.
#[tokio::test]
async fn radian_session_dir_overrides_radian_home() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let dir = scratch("session-dir-wins");
    let radian_home = dir.join("rh");
    let custom = dir.join("custom-sessions");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "h1",
                "-p",
                "hi",
            ],
            &[
                ("RADIAN_HOME", radian_home.to_str().unwrap()),
                ("RADIAN_SESSION_DIR", custom.to_str().unwrap()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        custom.join("h1.jsonl").exists(),
        "RADIAN_SESSION_DIR must win"
    );
    assert!(
        !radian_home.join("sessions/h1.jsonl").exists(),
        "the default must not be used when RADIAN_SESSION_DIR is set"
    );
}

/// ADR 0020 acceptance: under `workspace`, the `shell` tool cannot read the sessions directory;
/// the denial comes back to the model as a tool error and the turn continues.
#[tokio::test]
async fn workspace_shell_cannot_read_the_sessions_directory() {
    let dir = scratch("shell-deny-sessions");
    let radian_home = dir.join("rh");
    let sessions = radian_home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("x.jsonl"), "SECRET-SESSION-CONTENT\n").unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let command = format!("cat {}/x.jsonl", sessions.display());
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({ "command": command }).to_string()),
        text_reply("SAW-DENIED"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_bare(
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
                "--session-id",
                "deny",
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SAW-DENIED"), "{stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        !result.contains("SECRET-SESSION-CONTENT"),
        "the sandbox leaked the session file: {result}"
    );
    assert!(
        result.contains("exit code:"),
        "a shell result was expected: {result}"
    );
}

/// ADR 0039 decision 1: a background job's output file lives in the session's own
/// `<sessions_dir>/<session-id>.jobs/` directory, which is inside the sessions directory the
/// sandbox denies reading. A sandboxed shell cannot read it.
#[tokio::test]
async fn workspace_shell_cannot_read_a_background_jobs_output_file() {
    let dir = scratch("shell-deny-jobs");
    let radian_home = dir.join("rh");
    let sessions = radian_home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let session_id = "deny-jobs";
    let jobs = sessions.join(format!("{session_id}.jobs"));
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join("job-1.out"), "SECRET-JOB-OUTPUT\n").unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let command = format!("cat {}/job-1.out", jobs.display());
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({ "command": command }).to_string()),
        text_reply("SAW-DENIED"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_bare(
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
                "--session-id",
                session_id,
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SAW-DENIED"), "{stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        !result.contains("SECRET-JOB-OUTPUT"),
        "the sandbox leaked a background job's output file: {result}"
    );
    assert!(
        result.contains("exit code:"),
        "a shell result was expected: {result}"
    );
}

/// ADR 0020 acceptance 2: a `--session-dir` that does not exist yet must still be denied. The
/// host creates it 0700 **before** building the sandbox, because Linux's bwrap can only mask
/// paths that already exist; without that, the session file (created after the sandbox) would
/// be readable. This is the case that only fails on Linux if the create-before-deny is missing.
#[tokio::test]
async fn workspace_shell_cannot_read_a_moved_sessions_directory() {
    let dir = scratch("shell-deny-moved");
    let radian_home = dir.join("rh");
    let custom = dir.join("custom-sessions"); // does not exist yet
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let command = format!("cat {}/moved.jsonl", custom.display());
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({ "command": command }).to_string()),
        text_reply("SAW-DENIED"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_bare(
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
                "--session-dir",
                custom.to_str().unwrap(),
                "--session-id",
                "moved",
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SAW-DENIED"), "{stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        !result.contains("SessionCreated"),
        "the sandbox read the session file: {result}"
    );
}

#[tokio::test]
async fn an_interactive_compact_is_a_command_and_compactfoo_is_a_message() {
    let server = MockServer::start(vec![text_reply("REPLY-TO-COMPACTFOO")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(&["--model", "m", "--base-url", &server.base_url], &[]),
        "/compact\n/compactfoo\n/compact note\n/exit\n",
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(stdout, "REPLY-TO-COMPACTFOO\n");
    // Both `/compact` and `/compact note` report a result; neither reaches the model.
    assert_eq!(stderr.matches("nothing to compact").count(), 2, "{stderr}");
    assert_eq!(server.requests().len(), 1, "only `/compactfoo` was a turn");
    assert_eq!(last_user_text(&messages_of(&server, 0)), "/compactfoo");
}

// ---------- the configuration file (ADR 0008) ----------

/// A `--config` file supplies the model (so `--model` is not needed), and a `--model` on the
/// command line overrides it. `RADIAN_CONFIG` is the fallback when `--config` is absent.
#[tokio::test]
async fn a_config_file_supplies_the_model_and_the_cli_overrides_it() {
    let dir = scratch("config-model");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[model]\nmodel = \"config-model\"\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("A"), text_reply("B"), text_reply("C")]).await;
    let sessions = dir.join("sessions");

    // No `--model`: the config supplies it.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
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
        stderr.contains("[config]"),
        "the startup line names the file: {stderr}"
    );
    assert_eq!(server.requests()[0].json()["model"], "config-model");

    // `--model` wins over the file.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "cli-model",
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests()[1].json()["model"], "cli-model");

    // `RADIAN_CONFIG` is the fallback.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[("RADIAN_CONFIG", config.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests()[2].json()["model"], "config-model");
}

/// A `schema_version` below the supported one is refused at startup, before any session file
/// exists (ADR 0008 §4: no v0→v1 path, and the file is never rewritten).
#[tokio::test]
async fn a_lower_schema_version_refuses_to_start_without_creating_a_session() {
    let dir = scratch("config-lower");
    let config = dir.join("config.toml");
    std::fs::write(&config, "schema_version = 0\n[model]\nmodel = \"m\"\n").unwrap();
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("schema_version"), "{stderr}");
    assert!(
        jsonl_snapshot(&sessions).is_empty(),
        "no session file may be created: {:?}",
        dir_names(&sessions)
    );
    assert!(server.requests().is_empty(), "no request is sent");
}

/// A plaintext secret in the config is refused, and the error never echoes the value.
#[tokio::test]
async fn a_plaintext_secret_in_the_config_is_refused_without_echoing_it() {
    let dir = scratch("config-secret");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[model]\nmodel = \"m\"\napi_key = \"sk-live-abcdef123456\"\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("api_key"), "{stderr}");
    assert!(
        !stderr.contains("sk-live"),
        "the value must not be echoed: {stderr}"
    );
}

/// The config reader takes `schema_version = 0` from **the harness's temp `RADIAN_HOME`** and
/// names that path: the reader and the harness agree on the root, so a test config lives in
/// the temp directory and never in the user's real one.
#[tokio::test]
async fn a_failing_config_in_the_temp_radian_home_is_read_and_named() {
    let dir = scratch("config-rh");
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    std::fs::write(&config, "schema_version = 0\n").unwrap();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "-p", "hi"],
            &[("RADIAN_HOME", dir.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("older than the supported"), "{stderr}");
    assert!(
        stderr.contains(&config.display().to_string()),
        "the refusal names the temp config: {stderr}"
    );
    // The refusal happens before any API key is read.
    assert!(!stderr.contains("no API key"), "{stderr}");
}

/// The harness must never read the user's real `~/.radian/config/config.toml`. `spawn_radian`
/// pins the whole root (`RADIAN_HOME`) and drops `RADIAN_CONFIG`, so a `HOME` whose
/// `.radian/config/config.toml` would refuse startup is **not** consulted. ADR 0008 §2: with
/// no config in the pinned root the run creates the commented default there and names it —
/// never the outside file.
#[tokio::test]
async fn the_harness_never_reads_a_config_outside_its_temp_root() {
    let dir = scratch("config-fake-home");
    let config_dir = dir.join(".radian").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "schema_version = 0\n").unwrap();
    let home = dir.join("rh");

    // A local mock keeps the run past the API-key check, so the `[config]` startup line is
    // reached.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[
                ("HOME", dir.to_str().unwrap()),
                ("RADIAN_HOME", home.to_str().unwrap()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("older than the supported"),
        "a config outside the harness root must not be read: {stderr}"
    );
    let expected = home.join("config/config.toml");
    assert!(
        stderr.contains(&format!(
            "created a default config at {}",
            expected.display()
        )),
        "the default is created inside the harness root: {stderr}"
    );
    assert!(
        stderr.contains(&format!("[config] {}", expected.display())),
        "the run reads the created file, not the outside one: {stderr}"
    );
    assert_eq!(server.requests().len(), 1, "the run reached the model");
}

// ---------- Pi configuration migration (Phase 5) ----------

/// A Pi agent directory: an endpoint, a model, a `${OPENAI_API_KEY}` reference, one plain
/// header, one interpolated header, a theme, a compaction preference and a keybinding. This
/// is a fixture in a temporary directory — the real `~/.pi` is never read.
fn write_pi_fixture(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for (name, text) in [
        ("settings.json", include_str!("fixtures/pi/settings.json")),
        ("models.json", include_str!("fixtures/pi/models.json")),
        (
            "keybindings.json",
            include_str!("fixtures/pi/keybindings.json"),
        ),
    ] {
        std::fs::write(dir.join(name), text).unwrap();
    }
}

/// Acceptance 1: a typical Pi directory drafts a configuration the Radian reader accepts, with
/// the right values, and `api_key` is an environment reference. Acceptance 2 (first half): a
/// literal `apiKey` becomes a suggested reference and its value is in neither stream.
#[tokio::test]
async fn config_import_pi_drafts_a_valid_configuration_and_never_shows_a_literal_key() {
    let dir = scratch("import-pi");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &stdout)
        .expect("the draft must be a valid Radian configuration");
    assert_eq!(
        config.model.base_url.as_deref(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(config.model.model.as_deref(), Some("gpt-5"));
    // ADR 0026 D2: no `model.api_key` in the draft; the report points at auth.json.
    assert!(!stdout.contains("api_key"), "{stdout}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert_eq!(config.tui.theme.as_deref(), Some("dark"));
    assert_eq!(config.compaction.auto, Some(false));
    assert_eq!(
        config.model.headers,
        vec![("x-tenant".to_string(), "team".to_string())]
    );
    assert!(
        config.skipped.is_empty(),
        "only reader-implemented keys: {:?}",
        config.skipped
    );
    // stdout is the draft alone; the report is on stderr.
    assert!(!stdout.contains("[import-pi]"), "{stdout}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert!(stderr.contains("x-trace"), "{stderr}");
    assert!(stderr.contains("defaultThinkingLevel"), "{stderr}");
    // Nothing was written.
    assert!(!dir.join("config").join("config.toml").exists());

    // A literal key: the draft references a suggested variable and the value is nowhere.
    let literal = scratch("import-pi-literal");
    let pi = literal.join("pi");
    std::fs::create_dir_all(&pi).unwrap();
    std::fs::write(
        pi.join("settings.json"),
        r#"{ "defaultProvider": "ollama" }"#,
    )
    .unwrap();
    std::fs::write(
        pi.join("models.json"),
        r#"{ "providers": { "ollama": {
             "baseUrl": "http://localhost:11434/v1",
             "api": "openai-completions",
             "apiKey": "sk-live-abcdef1234567890",
             "models": [{ "id": "qwen2.5-coder:7b" }] } } }"#,
    )
    .unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let _ = radian_host::parse_config(Path::new("/tmp/draft.toml"), &stdout).unwrap();
    assert!(!stdout.contains("api_key"), "{stdout}");
    assert!(stderr.contains("OLLAMA_API_KEY"), "{stderr}");
    assert!(!stdout.contains("sk-live"), "{stdout}");
    assert!(!stderr.contains("sk-live"), "{stderr}");
}

/// Acceptance 2 (second half): a `!command` value is listed and **not executed**.
#[tokio::test]
async fn config_import_pi_lists_a_command_key_and_does_not_run_it() {
    let dir = scratch("import-pi-command");
    let pi = dir.join("pi");
    std::fs::create_dir_all(&pi).unwrap();
    let marker = dir.join("PWNED");
    std::fs::write(pi.join("settings.json"), "{}").unwrap();
    std::fs::write(
        pi.join("models.json"),
        format!(
            r#"{{ "providers": {{ "acme": {{
                 "baseUrl": "https://acme.example/v1",
                 "api": "openai-completions",
                 "apiKey": "!touch {}",
                 "models": [{{ "id": "m" }}] }} }} }}"#,
            marker.display()
        ),
    )
    .unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!marker.exists(), "the `!command` must not have run");
    assert!(stderr.contains("!command"), "{stderr}");
    assert!(!stdout.contains("api_key"), "no key was migrated: {stdout}");
}

/// Acceptance 3: `auth.json` is never read. A file that cannot be read at all (mode `0000`)
/// must not make the migration fail, and its bytes must not appear anywhere.
#[cfg(unix)]
#[tokio::test]
async fn config_import_pi_never_reads_auth_json() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("import-pi-auth");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let auth = pi.join("auth.json");
    std::fs::write(&auth, r#"{"openai":{"type":"api_key","key":"sk-SECRET"}}"#).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o000)).unwrap();

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("SECRET") && !stderr.contains("SECRET"));
    assert!(stderr.contains("auth.json"), "{stderr}");
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Acceptance 4: `--write` creates the file owner-only and refuses to overwrite an existing
/// one (ADR 0008 §4: this version has no faithful write-back).
#[cfg(unix)]
#[tokio::test]
async fn config_import_pi_write_creates_the_file_and_refuses_to_overwrite() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("import-pi-write");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let home_arg = home.to_str().unwrap();
    let pi_arg = pi.to_str().unwrap();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi_arg, "--write"],
            &[("RADIAN_HOME", home_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let target = home.join("config").join("config.toml");
    assert!(stderr.contains(&target.display().to_string()), "{stderr}");
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the file is owner-only");
    let text = std::fs::read_to_string(&target).unwrap();
    radian_host::parse_config(&target, &text).expect("the written file reads back");

    let before = std::fs::read(&target).unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi_arg, "--write"],
            &[("RADIAN_HOME", home_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(std::fs::read(&target).unwrap(), before, "bytes unchanged");
}

/// Acceptance 5: `--from` is required (a clap usage error, exit 2) and a missing directory is
/// a failure (exit 1), not a silent empty draft.
#[tokio::test]
async fn config_import_pi_requires_from_and_an_existing_directory() {
    let (code, _, stderr) =
        feed_and_wait(spawn_radian_top(&["config", "import-pi"], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--from"), "{stderr}");

    let dir = scratch("import-pi-missing");
    let missing = dir.join("nope");
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", missing.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("nope"), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
}

/// `radian config import-claude` is removed: an unknown subcommand (usage error, exit 2),
/// and `radian config --help` lists `import-pi` but not it.
#[tokio::test]
async fn config_import_claude_is_an_unknown_subcommand() {
    let (code, _, stderr) =
        feed_and_wait(spawn_radian_top(&["config", "import-claude"], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("import-claude"), "{stderr}");

    let (code, stdout, stderr) =
        feed_and_wait(spawn_radian_top(&["config", "--help"], &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("import-pi"), "{stdout}");
    assert!(!stdout.contains("import-claude"), "{stdout}");
}

/// Acceptance 6: a closed stdout is a normal stop (D21), not a panic.
#[cfg(unix)]
#[test]
fn config_import_pi_survives_a_closed_stdout() {
    let dir = scratch("import-pi-sigpipe");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = std::process::Command::new(common::radian_bin())
        .args(["config", "import-pi", "--from", pi.to_str().unwrap()])
        .env("RADIAN_HOME", home.as_os_str())
        .env("HOME", home.as_os_str())
        .stdin(Stdio::null())
        .stdout(closed_stdout())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "stderr {stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// ADR 0024 Q5: `agent` has no interactive user, so it does **not** register `ask_user` and
/// says so on its startup line. The TUI is the only host that registers it.
#[tokio::test]
async fn the_startup_line_says_ask_user_is_off() {
    let (_dir, ws) = budget_workspace("ask-off");
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
    assert!(
        stderr.contains("ask_user: off (no interactive user)"),
        "the startup line must say ask_user is off: {stderr}"
    );
    // The model was never offered the tool: check the advertised tool list, not the raw
    // request body, because a tool name can also appear in system-prompt text or in another
    // tool's description.
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{stderr}");
    let names = advertised_tools(&server, 0);
    assert!(
        !names.iter().any(|name| name == "ask_user"),
        "the CLI must not advertise the ask_user tool: {names:?}"
    );
}

// ---------- old memory-era data (ADR 0021 was removed) ----------

/// An old config that still carries `[memory]` (the last memory-era shape, with the retired
/// `[memory.injection]` table too) starts the session: one retired-key warning, nothing else
/// changes. The values are ignored.
#[tokio::test]
async fn an_old_memory_config_table_warns_once_and_the_session_starts() {
    let (dir, ws, sessions) = subtree_scratch("old-memory-config");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[memory]\nenabled = true\nscopes = [\"project\", \"global\"]\n\
         index_max_lines = 200\nindex_max_bytes = 25600\nsessions_index_max_lines = 1000\n\
         [memory.injection]\nmax_tokens = 1000\n",
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
    assert_eq!(
        stderr.matches("[config] ignored key `memory`").count(),
        1,
        "{stderr}"
    );
    // No memory section reaches the model, and the user's memory directory is never created.
    let messages = messages_of(&server, 0);
    let system = messages.as_array().expect("messages")[0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!system.contains("<memory>"), "{system}");
}

/// An old config that still carries `[control]` (the process control port was removed, ADR
/// 0043) starts the session: one retired-key warning, nothing else changes, and no socket or
/// `[control]` startup line appears.
#[tokio::test]
async fn an_old_control_config_table_warns_once_and_the_session_starts() {
    let (dir, ws, sessions) = subtree_scratch("old-control-config");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[control]\nenabled = false\n",
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
    assert_eq!(
        stderr.matches("[config] ignored key `control`").count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains("[control]"), "{stderr}");
}

/// An old journal holding a `MemoryInjected` record and a `memory_addition` message still
/// opens: `--list`, `--export` (markdown and JSON) and `--resume` all work, and the replayed
/// history keeps the old message.
#[tokio::test]
async fn an_old_journal_with_memory_records_lists_exports_and_resumes() {
    let dir = scratch("old-memory-journal");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ts = "2026-10-13T00:00:00.000000Z";
    let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let event = |seq: u64, event: &str, critical: bool, data: Value| {
        json!({"schema_version":1,"session_id":"old-mem","seq":seq,"ts":ts,
               "event":event,"critical":critical,"data":data})
    };
    let lines = [
        event(
            0,
            "SessionCreated",
            true,
            json!({"workspace":canonical(&ws),"config_snapshot":{}}),
        ),
        event(
            1,
            "MessageAppended",
            true,
            json!({"index":0,"message":{"user":{"content":[{"text":{"text":"OLD-USER-TEXT"}}]}}}),
        ),
        event(
            2,
            "MemoryInjected",
            false,
            json!({"indexes":[{"scope":"global",
                "path":"/home/example/.radian/memory/global/MEMORY.md",
                "sha256":sha,"lines":1,"bytes":40}]}),
        ),
        event(
            3,
            "MessageAppended",
            true,
            json!({"index":1,"message":{"user":{
                "content":[{"text":{"text":"OLD-MEMORY-ADDITION"}}],
                "meta":{"source":"system","origin":{"kind":"memory_addition","records":[]}}}}}),
        ),
        event(
            4,
            "MessageAppended",
            true,
            json!({"index":2,"message":{"assistant":{"content":[{"text":{"text":"OLD-REPLY"}}]}}}),
        ),
    ];
    let mut text = String::new();
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    std::fs::write(sessions.join("old-mem.jsonl"), text).unwrap();

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    let with = |extra: &[&'static str]| {
        let mut args = base.to_vec();
        args.extend_from_slice(extra);
        args
    };
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&with(&["--list"]), &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("old-mem"), "{stdout}");

    let (code, stdout, stderr) =
        feed_and_wait(spawn_radian(&with(&["--export", "old-mem"]), &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("OLD-USER-TEXT"), "{stdout}");
    assert!(stdout.contains("OLD-REPLY"), "{stdout}");

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(&with(&["--export", "old-mem", "--format", "json"]), &[]),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let parsed: Value = serde_json::from_str(&stdout).expect("the JSON export is valid JSON");
    assert!(
        parsed["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "MemoryInjected"),
        "{stdout}"
    );

    let server = MockServer::start(vec![text_reply("NEW-REPLY")]).await;
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
                "--resume",
                "old-mem",
                "-p",
                "again",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let sent = server.requests()[0].body.clone();
    assert!(sent.contains("OLD-USER-TEXT"), "{sent}");
    assert!(sent.contains("OLD-MEMORY-ADDITION"), "{sent}");
}

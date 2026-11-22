//! End-to-end tests of `radian trust`, the project trust record, and the trust prompt that gates project resources (project skills).
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_openai_compatible::testing::MockServer;
use std::path::Path;
use std::process::Stdio;
use support::*;

// ---------- project trust (`radian trust status|on|off|forget|list`) ----------

/// Runs `radian trust <args>` with `RADIAN_HOME` (and `HOME`) pinned to a scratch root,
/// capturing stdout and stderr. Returns the exit code and both streams.
fn run_trust(home: &Path, ws: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full_args = vec!["trust", "--workspace", ws.to_str().unwrap()];
    full_args.extend(args);
    let output = std::process::Command::new(common::radian_bin())
        .args(&full_args)
        .env("RADIAN_HOME", home)
        .env("HOME", home)
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
        .env_remove("RADIAN_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run radian trust");
    (
        output.status.code().unwrap(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `radian trust status|on|off|forget|list` in a temp `RADIAN_HOME`: the output lines,
/// the exit codes, and the file written. The mutation (a write writes nothing) fails
/// the disk and follow-up status assertions.
#[test]
fn trust_commands_read_and_write_the_record() {
    let dir = scratch("trust-cli");
    let home = dir.join("home");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let key = canonical(&ws);

    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout.trim(),
        "trust: no saved decision (untrusted)",
        "{stdout}"
    );

    let (code, stdout, stderr) = run_trust(&home, &ws, &["on"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("takes effect at the next start"),
        "{stdout}"
    );
    assert!(stdout.starts_with("trusted "), "{stdout}");
    let store = std::fs::read_to_string(home.join("trust/trust.json")).unwrap();
    assert!(store.contains(&format!("\"{key}\": true")), "{store}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("trust on (saved for"), "{stdout}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["list"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), format!("{key}: on"), "{stdout}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["off"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("distrusted "), "{stdout}");
    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("trust off (saved for"), "{stdout}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["forget"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("takes effect at the next start"),
        "{stdout}"
    );
    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout.trim(),
        "trust: no saved decision (untrusted)",
        "{stdout}"
    );

    // Forgetting again is a no-op that writes nothing and says so.
    let (code, stdout, stderr) = run_trust(&home, &ws, &["forget"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("no saved trust decision"), "{stdout}");
    let (code, stdout, stderr) = run_trust(&home, &ws, &["list"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "no saved trust decisions", "{stdout}");

    // An unknown subcommand is a usage error, and `exec` runs the same dispatch.
    let output = std::process::Command::new(common::radian_bin())
        .args(["trust", "bogus"])
        .env("RADIAN_HOME", &home)
        .env("HOME", &home)
        .output()
        .expect("run radian trust bogus");
    assert_eq!(output.status.code(), Some(2));
    let exec = std::process::Command::new(common::radian_bin())
        .args([
            "exec",
            "--workspace",
            ws.to_str().unwrap(),
            "trust",
            "status",
        ])
        .env("RADIAN_HOME", &home)
        .env("HOME", &home)
        .output()
        .expect("run radian exec trust status");
    assert_eq!(
        exec.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&exec.stderr)
    );
    assert!(
        String::from_utf8_lossy(&exec.stdout).contains("no saved decision"),
        "{}",
        String::from_utf8_lossy(&exec.stdout)
    );
}

// ---------- trust prompts and project resources ----------

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

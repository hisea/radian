//! End-to-end tests of `radian login` and `radian logout`.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

use agent_provider_openai_compatible::testing::MockServer;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::*;

// ---------- radian login / radian logout (ADR 0026 D3) ----------

/// `radian login` refuses without a terminal: a key pasted into a pipe would be visible, so
/// the message points at `auth.json` instead (D3). Exit code 2 (usage-shaped).
#[tokio::test]
async fn login_without_a_terminal_refuses_and_points_at_auth_json() {
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["login", "openai"])
        .env("HOME", fresh_session_dir())
        .env("RADIAN_HOME", fresh_session_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .expect("spawn radian login");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("login needs a terminal"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    // The refusal must not echo the provider's default endpoint or any placeholder.
    assert!(!stderr.contains("sk-"), "{stderr}");
}

/// A private `RADIAN_HOME` with a two-key `opencode-go` entry (ADR 0026 D1).
fn home_with_two_keys() -> PathBuf {
    let home = fresh_session_dir();
    let credentials = home.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let auth = credentials.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"opencode-go": {"type": "api_key", "key": "sk-first",
                            "keys": [{"name": "team", "key": "sk-second"}]}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    home
}

/// `radian logout <provider> <name>` removes that key without asking; removing the first key
/// promotes the next one to `key` (ADR 0026 D1/D3).
#[tokio::test]
async fn logout_with_a_key_name_removes_one_and_promotes_the_next() {
    let home = home_with_two_keys();
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["logout", "opencode-go", "team"])
        .env("HOME", &home)
        .env("RADIAN_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("spawn radian logout");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["opencode-go"]["key"], "sk-first");
    assert!(auth["opencode-go"].get("keys").is_none());

    // The first key: the next one is promoted to `key`, so pi keeps reading the entry.
    let home = home_with_two_keys();
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["logout", "opencode-go", "first"])
        .env("HOME", &home)
        .env("RADIAN_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("spawn radian logout");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["opencode-go"]["key"], "sk-second");
}

/// An unknown key name is refused, naming the known ones, and nothing is removed.
#[tokio::test]
async fn logout_with_an_unknown_key_name_names_the_known_ones() {
    let home = home_with_two_keys();
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["logout", "opencode-go", "no-such-key"])
        .env("HOME", &home)
        .env("RADIAN_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("spawn radian logout");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no-such-key"), "{stderr}");
    assert!(
        stderr.contains("first") && stderr.contains("team"),
        "{stderr}"
    );
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["opencode-go"]["key"], "sk-first");
    assert_eq!(auth["opencode-go"]["keys"][0]["name"], "team");
}

/// Printed by the pty's shell after its trailing `stty -a`, so a reader that waits for it knows
/// it has the whole tty state (see [`PtyChild::wait_for_stty`]).
const STTY_END: &str = "STTY-END";

/// A minimal pty: the child runs `sh -c '"$@"; echo EXITED:$?; stty -a; echo STTY-END'`, so
/// the same pty reports the exit code and the terminal state **after** the app left (the
/// echo-restore invariant is only observable there). Raw bytes accumulate; the reader drains
/// them.
struct PtyChild {
    child: tokio::process::Child,
    master: std::fs::File,
}

impl PtyChild {
    fn spawn(args: &[&str], home: &Path) -> Self {
        let mut master_fd: libc::c_int = -1;
        let mut slave_fd: libc::c_int = -1;
        // SAFETY: openpty writes the two fds; the name and termios pointers may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // One dup per stdio slot: each raw fd is owned by exactly one `Stdio`, and the
        // `Command` closes both after spawn (the child's exec copies keep the slave open
        // until it exits, which is what gives the master its EOF).
        let stdout_fd = unsafe { libc::dup(slave_fd) };
        assert!(stdout_fd >= 0, "dup failed");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!(
                r#""$@"; echo "EXITED:$?"; stty -a; echo "{STTY_END}""#
            ))
            .arg("sh")
            .args(args)
            .env("HOME", home)
            .env("RADIAN_HOME", home)
            .env("RADIAN_SESSION_DIR", fresh_session_dir())
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .env_remove("RADIAN_CONFIG")
            .stdin(unsafe {
                // SAFETY: slave_fd is a fresh owned fd, moved into the Command.
                use std::os::fd::FromRawFd;
                std::process::Stdio::from_raw_fd(slave_fd)
            })
            .stdout(unsafe {
                // SAFETY: stdout_fd is a fresh owned fd, moved into the Command.
                use std::os::fd::FromRawFd;
                std::process::Stdio::from_raw_fd(stdout_fd)
            })
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("spawn in pty");
        // Non-blocking master, so `drain` never blocks (the first frame is bigger than the
        // pty buffer, and the app would otherwise block in its draw).
        // SAFETY: master_fd is a fresh fd; F_SETFL is async-signal-safe.
        unsafe { libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK) };
        // SAFETY: master_fd is a fresh fd.
        let master: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(master_fd) };
        Self { child, master }
    }

    /// Drains whatever is pending.
    fn drain(&mut self, buf: &mut Vec<u8>) {
        use std::io::Read;
        let mut chunk = [0u8; 8192];
        if let Ok(n) = self.master.read(&mut chunk) {
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn wait_for(&mut self, buf: &mut Vec<u8>, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.drain(buf);
            if String::from_utf8_lossy(buf).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the shell's trailing `stty -a` is complete and returns its text.
    ///
    /// `stty -a` writes to a tty line by line, and [`Self::drain`] copies at most one chunk
    /// per call, so its first line (`speed …`) can be in `buf` while the line that carries
    /// `icanon` and `echo` has not been read yet: asserting on that prefix fails on a healthy
    /// terminal under load. The shell prints [`STTY_END`] after `stty -a`, so this waits for
    /// that marker — everything before it is complete — and returns what followed the app's
    /// `EXITED:<code>` line.
    async fn wait_for_stty(
        &mut self,
        buf: &mut Vec<u8>,
        exited: &str,
        secs: u64,
    ) -> Option<String> {
        if !self.wait_for(buf, STTY_END, secs).await {
            return None;
        }
        let text = String::from_utf8_lossy(buf).into_owned();
        let after = text.split(exited).nth(1)?;
        Some(after.split(STTY_END).next().unwrap_or(after).to_owned())
    }

    fn write(&self, bytes: &[u8]) {
        use std::io::Write;
        let mut master = &self.master;
        let _ = master.write_all(bytes);
        let _ = master.flush();
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// `stty -a` shows the echo state as tokens (`echo`, `-echo`, `echoke`, …): check the tokens,
/// not substrings (`-echok` contains `-echo`).
fn assert_stty_has_echo(stty: &str) {
    let tokens: Vec<&str> = stty
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
    assert!(
        tokens.contains(&"icanon"),
        "canonical mode must be restored: {stty}"
    );
    assert!(
        tokens.contains(&"echo"),
        "echo must be restored (and `-echo` absent): {stty}"
    );
    assert!(!tokens.contains(&"-echo"), "echo is still off: {stty}");
}

/// The full `radian login custom` flow on a real pty: the key is **never** in the pty stream
/// (echo off), the verification runs against a local server, `auth.json` gains the entry
/// (with the endpoint in `env`, D10), and after the app exits the terminal's echo is **on**
/// again — the restore held on the normal exit path.
#[tokio::test]
async fn pty_agent_login_custom_hides_the_key_and_restores_echo() {
    use std::io::Write as _;
    // The verification endpoint: a one-shot 200 per connection.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
    });

    let home = fresh_session_dir();
    let credentials = home.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    let mut pty = PtyChild::spawn(&[&common::radian_bin_str(), "login", "custom"], &home);
    let mut seen = Vec::new();
    const KEY: &str = "sk-CLI-HIDDEN-99";
    assert!(
        pty.wait_for(&mut seen, "provider name", 10).await,
        "{}",
        String::from_utf8_lossy(&seen)
    );
    pty.write(b"corp\r");
    assert!(pty.wait_for(&mut seen, "protocol", 10).await);
    pty.write(b"1\r");
    assert!(pty.wait_for(&mut seen, "base URL", 10).await);
    pty.write(format!("http://{addr}\r").as_bytes());
    assert!(pty.wait_for(&mut seen, "API key", 10).await);
    pty.write(format!("{KEY}\r").as_bytes());
    assert!(
        pty.wait_for(&mut seen, "saved the key for `corp`", 10)
            .await,
        "{}",
        String::from_utf8_lossy(&seen)
    );
    assert!(
        pty.wait_for(&mut seen, "EXITED:0", 10).await,
        "{}",
        String::from_utf8_lossy(&seen)
    );

    // The key never crossed the wire in a visible form.
    assert!(
        !String::from_utf8_lossy(&seen).contains(KEY),
        "the key leaked: {}",
        String::from_utf8_lossy(&seen)
    );
    // The record is right, next to nothing else, 0600.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(saved["corp"]["key"], KEY);
    assert_eq!(
        saved["corp"]["env"]["RADIAN_BASE_URL"],
        format!("http://{addr}")
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(home.join("credentials/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    // Echo restored after the exit: `stty -a` must show `echo` enabled. Wait for the whole
    // `stty` output (its end marker), not just `EXITED:0` or stty's first line: the pty
    // delivers those bytes in several writes.
    let stty = pty
        .wait_for_stty(&mut seen, "EXITED:0", 5)
        .await
        .unwrap_or_else(|| panic!("no stty output: {}", String::from_utf8_lossy(&seen)));
    assert_stty_has_echo(&stty);
}

/// Ctrl-C **during the hidden key prompt** ends the process (130) and still restores the
/// echo — the invariant the brief calls out by name.
#[tokio::test]
async fn pty_agent_login_ctrl_c_during_the_key_prompt_restores_echo() {
    let home = fresh_session_dir();
    std::fs::create_dir_all(home.join("credentials")).unwrap();
    let mut pty = PtyChild::spawn(&[&common::radian_bin_str(), "login", "anthropic"], &home);
    let mut seen = Vec::new();
    assert!(
        pty.wait_for(&mut seen, "API key", 10).await,
        "{}",
        String::from_utf8_lossy(&seen)
    );
    pty.write(b"sk-partial");
    tokio::time::sleep(Duration::from_millis(100)).await;
    pty.write(&[0x03]); // Ctrl-C inside the echo-off window
    assert!(
        pty.wait_for(&mut seen, "EXITED:130", 10).await,
        "Ctrl-C did not end the login: {}",
        String::from_utf8_lossy(&seen)
    );
    // The terminal is back to cooked mode: echo on, canonical on. The whole `stty -a` output
    // is only in `seen` at its end marker.
    let stty = pty
        .wait_for_stty(&mut seen, "EXITED:130", 5)
        .await
        .unwrap_or_else(|| panic!("no stty output: {}", String::from_utf8_lossy(&seen)));
    assert_stty_has_echo(&stty);
}

/// ADR 0043: a headless `radian exec` mounts neither `git_commit` nor any task/lead/
/// named tool; its list is the built-in tools only.
#[tokio::test]
async fn a_headless_exec_mounts_no_orchestration_tools() {
    let server = MockServer::start(vec![text_reply("hi")]).await;
    // A private home: the tool set must not depend on the machine's real home.
    let home = scratch("headless-tools");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let names = advertised_tools(&server, 0);
    assert!(
        !names.iter().any(|name| name == "git_commit"),
        "`git_commit` must not be mounted: {names:?}"
    );
    for gone in [
        "spawn_task",
        "wait_task",
        "wait_any",
        "list_tasks",
        "task_status",
        "cancel_task",
        "remove_task",
        "integrate_task",
        "answer_task",
        "deny_task_approval",
        "escalate_approval",
        "send_agent",
        "wait_agent",
    ] {
        assert!(
            !names.iter().any(|name| name == gone),
            "`{gone}` must not be mounted: {names:?}"
        );
    }
    // The exact set: a headless session with no `--workspace` mounts the
    // built-in `skill` (its `theme-import` skill is compiled in), `subagent` (ADR 0043
    // decision 1a — a parent mounts it even with no workspace; only `explore` needs one) and
    // `goal` (ADR 0047 — host-level like `ask_user`, answering `no_active_goal` without one),
    // and nothing else.
    let mut got = names.clone();
    got.sort();
    assert_eq!(
        got,
        ["goal", "skill", "subagent"],
        "the exact tool set of a fresh headless session: {names:?}"
    );
}

/// The intent-stop validator end to end (backlog `turn-end-continuation.md` §B
/// acceptance): a turn ending with "announcing a next step but no tool call" opens
/// exactly one more round carrying the fixed continuation text; removing the
/// validator (or the mock's second reply going unused) fails the request count.
#[tokio::test]
async fn intent_stop_continues_an_intent_only_turn_end_once() {
    let dir = scratch("intent-stop-fire");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        text_reply("Now the docs-only ADR updates."),
        text_reply("Done."),
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
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one continuation opens exactly one more round"
    );
    let second = last_user_text(&messages_of(&server, 1));
    assert!(
        second.contains(radian_host::intent_stop::INTENT_STOP_TEXT),
        "the fixed continuation text rides the new round: {second}"
    );
}

/// Ordinary turn ends hold the validator end to end: a done report, a question, and a
/// long report whose last sentence matches the rule each end the run with no further
/// round. A validator that fires on any of them adds a request and fails this.
#[tokio::test]
async fn ordinary_turn_ends_do_not_continue() {
    for end in [
        "Done.".to_owned(),
        "Is that all you need?".to_owned(),
        format!("{}\n\nNext: the reviewer decides.", "x".repeat(300)),
    ] {
        let dir = scratch("intent-stop-hold");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let server = MockServer::start(vec![text_reply(&end)]).await;
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
        assert_eq!(code, 0, "{end}: {stderr}");
        assert_eq!(server.requests().len(), 1, "no continuation after {end:?}");
    }
}

/// A waiting text holds while its background job runs: the notice — not the validator —
/// opens the next round. The validator fires within milliseconds of the turn end while
/// the notice needs the job's two seconds, so a broken suppression shows up as a
/// fourth request carrying the fixed text (or a run out of mock replies).
#[tokio::test]
async fn waiting_on_a_running_job_does_not_continue() {
    let dir = scratch("intent-stop-waiting");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "c1",
            "shell",
            &json!({"command": "sleep 2", "background": true}).to_string(),
        ),
        text_reply("Waiting for the completion notice."),
        text_reply("Done."),
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
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "prompt, post-tool and post-notice rounds, no validator round"
    );
    for (index, request) in requests.iter().enumerate() {
        assert!(
            !request.json().to_string().contains("announced a next step"),
            "request {index} must not carry the validator text"
        );
    }
}

/// `[hooks] intent_stop = false` turns the built-in validator off end to end: an
/// intent-only turn end opens no further round.
#[tokio::test]
async fn intent_stop_off_in_config_does_not_continue() {
    let dir = scratch("intent-stop-off");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let config = dir.join("radian.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[hooks]\nintent_stop = false\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("Now the docs-only ADR updates.")]).await;
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
            &[("RADIAN_CONFIG", config.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        server.requests().len(),
        1,
        "the switch turns the validator off"
    );
}

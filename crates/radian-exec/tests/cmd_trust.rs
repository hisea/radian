//! End-to-end tests of `radian trust` and the project trust record.
//! The spawn helpers are in `support/mod.rs`. Every test drives the real `radian` binary
//! (ADR 0032).

mod common;
mod support;

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

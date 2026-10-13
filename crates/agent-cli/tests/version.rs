//! `--version` prints `<program> <version>` and exits before any setup runs: no key, no
//! session file, no network. So this test runs the binary as an ordinary command, like the
//! plain `std::process::Command` tests in `tests/perf_agent.rs`.
//!
//! The version printed must be the one Cargo knows: `CARGO_PKG_VERSION` of this package, not a
//! literal baked into the source.

use std::process::Command;

#[test]
fn version_prints_the_program_name_and_the_cargo_package_version() {
    // The same environment discipline as `spawn_agent`: no key, and never the user's real
    // session directory. `--version` must not need either.
    let output = Command::new(env!("CARGO_BIN_EXE_agent"))
        .arg("--version")
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", std::env::temp_dir())
        .output()
        .expect("run --version");

    assert_eq!(output.status.code(), Some(0), "exit code");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "--version must print the package version {}: stdout {stdout:?} stderr {stderr:?}",
        env!("CARGO_PKG_VERSION")
    );
    // The program name is the binary's, not the package's (`agent`, not `agent-cli`).
    assert_eq!(
        stdout.trim_end(),
        format!("agent {}", env!("CARGO_PKG_VERSION")),
        "stdout {stdout:?} stderr {stderr:?}"
    );
    assert!(stderr.is_empty(), "stderr {stderr:?}");
}

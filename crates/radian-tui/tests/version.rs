//! `--version` prints `<program> <version>` and exits **without** starting the app or touching
//! the terminal, so this test runs the binary as an ordinary command, like `tests/list.rs`.
//!
//! The version printed must be the one Cargo knows: `CARGO_PKG_VERSION` of this package, not a
//! literal baked into the source.

use std::process::Command;

#[test]
fn version_prints_the_program_name_and_the_cargo_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_radian-tui"))
        .arg("--version")
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
    // `<program> <version>`, exactly, on one line: a script parses this.
    assert_eq!(
        stdout.trim_end(),
        format!("radian-tui {}", env!("CARGO_PKG_VERSION")),
        "stdout {stdout:?} stderr {stderr:?}"
    );
    assert!(stderr.is_empty(), "stderr {stderr:?}");
}

//! `--version` prints `<program> <version>` and exits **without** starting the app or touching
//! the terminal, so this test runs the binary as an ordinary command, like `tests/list.rs`.
//!
//! The version printed must be the one Cargo knows: `CARGO_PKG_VERSION` of this package, not a
//! literal baked into the source. The `rad` alias is the same binary, so its output is
//! identical (ADR 0026 slice 0b).

use std::process::Command;

fn version_of(bin: &str) -> String {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .expect("run --version");
    assert_eq!(output.status.code(), Some(0), "exit code");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "--version must print the package version {}: stdout {stdout:?} stderr {stderr:?}",
        env!("CARGO_PKG_VERSION")
    );
    assert!(stderr.is_empty(), "stderr {stderr:?}");
    stdout
}

#[test]
fn version_prints_the_program_name_and_the_cargo_package_version() {
    let stdout = version_of(env!("CARGO_BIN_EXE_radian"));
    // `<program> <version>`, exactly, on one line: a script parses this.
    assert_eq!(
        stdout.trim_end(),
        format!("radian {}", env!("CARGO_PKG_VERSION")),
        "stdout {stdout:?}"
    );
}

/// The `rad` alias is a second `[[bin]]` on the same `src/main.rs`; `--version` must match the
/// `radian` binary byte for byte.
#[test]
fn the_rad_alias_prints_the_same_version_as_radian() {
    let radian = version_of(env!("CARGO_BIN_EXE_radian"));
    let rad = version_of(env!("CARGO_BIN_EXE_rad"));
    assert_eq!(rad, radian, "rad --version must equal radian --version");
}

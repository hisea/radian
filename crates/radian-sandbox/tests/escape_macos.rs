//! The macOS escape matrix. Treated as the authoritative macOS regression: it needs a real
//! `sandbox-exec`. On other platforms the file compiles to nothing (the Linux run is the
//! `sandbox-escape` driver executed in the VM).
#![cfg(target_os = "macos")]

/// The system launcher, also used by ADR 0016's acceptance 1 below.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The whole matrix, plus ADR 0016 acceptance 1. Both need a process that is **not**
/// already in a sandbox: a nested `sandbox-exec` cannot start at all, and the kernel would
/// answer `true` for "already confined". They live in one test for that reason, and inside
/// another sandbox it prints the fixed marker and returns (decision 6) — `cargo` counts it as
/// passed, so a report must list the marker as "not run". A platform with no `sandbox-exec` is
/// handled by this file's `cfg` (it compiles to nothing), as the ADR says.
#[test]
fn macos_escape_matrix() {
    if radian_sandbox::confined_by_outer_sandbox() {
        println!("radian-test: skipped inside a sandbox: macos_escape_matrix");
        return;
    }

    // ADR 0016 acceptance 1: the kernel answers for the caller, so the check runs in
    // a fresh process. This one is outside any sandbox; a `(version 1)(allow default)` child
    // must report `true`. `sandbox_check(getpid(), NULL, 0)` was measured on macOS 27,
    // 2026-10-16: 0 outside, 1 under `sandbox-exec` with either profile kind.
    assert!(
        !radian_sandbox::confined_by_outer_sandbox(),
        "this test process is not in a sandbox, so the kernel must answer false"
    );
    let output = std::process::Command::new(SANDBOX_EXEC)
        .args([
            "-p",
            "(version 1)(allow default)",
            env!("CARGO_BIN_EXE_sandbox-confined"),
        ])
        .output()
        .expect("sandbox-exec runs the helper");
    assert!(
        output.status.success(),
        "sandbox-exec refused the helper: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "true",
        "a process under sandbox-exec must report that it is confined"
    );

    let matrix = radian_sandbox::harness::run_matrix().expect("the macOS backend must probe");
    eprintln!("{}", matrix.render_text());
    let failures = matrix.failures();
    assert!(
        failures.is_empty(),
        "{} escape cases failed",
        failures.len()
    );
}

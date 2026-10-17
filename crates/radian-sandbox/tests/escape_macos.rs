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

/// ADR 0016 acceptance 1, in a form that cannot skip itself green.
///
/// The acceptance asks that the kernel answer `false` outside a sandbox and `true` under
/// `sandbox-exec`. Above, `macos_escape_matrix` checks that too, but it first calls
/// `confined_by_outer_sandbox()` in the test process and returns when it reads `true`. So a
/// mutation that makes that function answer `true` unconditionally (for example `result > 0` →
/// `result >= 0`, so a 0 return counts as confined) turns the whole check into the "skip inside a
/// sandbox" return, and the acceptance passes without asking the kernel anything. Here the answer
/// is read from the `sandbox-confined` helper's own stdout, so a wrong answer is an assertion
/// failure, never a skip.
///
/// In-sandbox detection is structural and does **not** go through the function under test: macOS
/// refuses to apply a second profile inside a `(deny default)` sandbox (`sandbox-exec:
/// sandbox_apply: Operation not permitted`, measured), which is the same reason the escape matrix
/// cannot run inside one. When the kernel refused the nested `sandbox-exec`, this test prints the
/// fixed marker and returns (decision 6); `cargo` counts it as passed, so the marker must be
/// listed as "not run".
#[test]
fn macos_confined_probe_is_false_outside_and_true_under_sandbox_exec() {
    // A fresh process under `sandbox-exec -p '(version 1)(allow default)'` must read the kernel's
    // `sandbox_check(getpid(), NULL, 0)` answer of 1 through the helper.
    let nested = std::process::Command::new(SANDBOX_EXEC)
        .args([
            "-p",
            "(version 1)(allow default)",
            env!("CARGO_BIN_EXE_sandbox-confined"),
        ])
        .output()
        .expect("sandbox-exec runs the helper");
    if !nested.status.success() {
        // The kernel refused the profile, so this test process is itself inside a `(deny default)`
        // sandbox where no second profile can be applied. Assert the refusal is the measured one,
        // so an unrelated `sandbox-exec` failure outside a sandbox is not swallowed as a skip.
        assert!(
            String::from_utf8_lossy(&nested.stderr).contains("sandbox_apply"),
            "sandbox-exec failed but not with the in-sandbox refusal: {}",
            String::from_utf8_lossy(&nested.stderr)
        );
        println!(
            "radian-test: skipped inside a sandbox: \
             macos_confined_probe_is_false_outside_and_true_under_sandbox_exec"
        );
        return;
    }
    assert_eq!(
        String::from_utf8_lossy(&nested.stdout).trim(),
        "true",
        "a process under sandbox-exec must report that it is confined"
    );

    // The same helper run directly is outside any sandbox: `sandbox_check(getpid(), NULL, 0)`
    // answers 0, so a correct `confined_by_outer_sandbox()` prints exactly `false`. The
    // `result >= 0` mutation (a 0 return counted as confined) prints `true` here and fails this
    // assertion.
    let direct = std::process::Command::new(env!("CARGO_BIN_EXE_sandbox-confined"))
        .output()
        .expect("the confined probe runs");
    assert!(
        direct.status.success(),
        "the confined probe exited with {}: {}",
        direct.status,
        String::from_utf8_lossy(&direct.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&direct.stdout).trim(),
        "false",
        "outside any sandbox the kernel answers 0; a 0 return must not be treated as confined"
    );
}

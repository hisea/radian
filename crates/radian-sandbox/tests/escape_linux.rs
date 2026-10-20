//! The Linux escape matrix, for a machine that has `bwrap` and a Rust toolchain. On this
//! repository the Linux run is performed by the `sandbox-escape` driver built for the VM
//! (see `docs/`), because the VM has no toolchain; this file keeps the same matrix runnable
//! in Linux CI. On other targets it compiles to nothing.
#![cfg(target_os = "linux")]

#[test]
fn linux_escape_matrix() {
    // ADR 0016 puts the escape matrix in the "skip inside a sandbox"
    // list: it holds resources outside the sandbox on purpose. Linux answers `false` today
    // (revision 2 decision 7 leaves bwrap nesting unverified), so this is the same shape the
    // macOS matrix uses, ready for the day Linux has a signal.
    if radian_sandbox::confined_by_outer_sandbox() {
        println!("radian-test: skipped inside a sandbox: linux_escape_matrix");
        return;
    }
    let matrix = radian_sandbox::harness::run_matrix().expect("the bwrap backend must probe");
    eprintln!("{}", matrix.render_text());
    // Slice 0: the seccomp filter denies every Unix socket in the `workspace` tier, so the
    // pathname and abstract denial cases pass like any other case (the old KNOWN-LIMIT for
    // `workspace_denied` is gone: a sandbox that connects is an escape, not a limit).
    for name in ["workspace_denied", "abstract_denied"] {
        let case = matrix
            .cases
            .iter()
            .find(|c| c.group == "unix_socket" && c.name == name)
            .unwrap_or_else(|| panic!("the matrix must contain unix_socket / {name}"));
        assert!(
            case.passed(),
            "unix_socket / {name} must pass: {}",
            case.sandbox.detail
        );
        assert_eq!(
            case.verdict(),
            radian_sandbox::harness::Verdict::Pass,
            "unix_socket / {name} must be a pass, not a limit",
        );
    }
    // The whitelist stays denied on Linux (seccomp cannot whitelist by path); macOS keeps
    // enforcing it as an allow.
    let whitelisted = matrix
        .cases
        .iter()
        .find(|c| c.group == "unix_socket" && c.name == "whitelisted_denied")
        .expect("the matrix must contain unix_socket / whitelisted_denied on Linux");
    assert!(
        whitelisted.passed(),
        "unix_socket / whitelisted_denied must pass: {}",
        whitelisted.sandbox.detail
    );
    let failures = matrix.failures();
    assert!(
        failures.is_empty(),
        "{} escape cases failed",
        failures.len()
    );
}

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
    let failures = matrix.failures();
    assert!(
        failures.is_empty(),
        "{} escape cases failed",
        failures.len()
    );
}

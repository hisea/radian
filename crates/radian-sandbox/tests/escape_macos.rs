//! The macOS escape matrix. Treated as the authoritative macOS regression: it needs a real
//! `sandbox-exec`. On other platforms the file compiles to nothing (the Linux run is the
//! `sandbox-escape` driver executed in the VM).
#![cfg(target_os = "macos")]

#[test]
fn macos_escape_matrix() {
    let matrix = radian_sandbox::harness::run_matrix().expect("the macOS backend must probe");
    eprintln!("{}", matrix.render_text());
    let failures = matrix.failures();
    assert!(
        failures.is_empty(),
        "{} escape cases failed",
        failures.len()
    );
}

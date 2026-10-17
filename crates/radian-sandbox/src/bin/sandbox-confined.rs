//! Prints what the kernel answers for **this** process to
//! `radian_sandbox::confined_by_outer_sandbox` — `true` or `false`, then exits 0.
//!
//! ADR 0016 acceptance 1: `sandbox_check` answers for the caller, so the check has to
//! run in the process under test. `tests/escape_macos.rs` starts this binary under
//! `/usr/bin/sandbox-exec -p '(version 1)(allow default)'` and requires `true`, and requires the
//! test process itself (no outer sandbox) to get `false`.

use std::process::ExitCode;

fn main() -> ExitCode {
    println!("{}", radian_sandbox::confined_by_outer_sandbox());
    ExitCode::SUCCESS
}

//! A **structural** test for ADR 0043: the control port's handlers run on the runtime and
//! must use the async `Inbox`, never `LocalSession`'s `block_on` wrappers.
//!
//! It is deliberately narrow and honest about what it is: it reads one file's source and greps
//! it, like `radian-tui/tests/purity.rs`. It cannot prove the handlers never block (an
//! `.await` on a blocking call would still slip through); it catches the obvious regression —
//! a `block_on` reappearing in `control.rs`.

/// `src/control.rs` with comment lines removed, so prose that *describes* the rule does not
/// trip the check.
fn control_code() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/control.rs");
    let source = std::fs::read_to_string(path).expect("control.rs is readable");
    source
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("//!"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn control_rs_never_blocks_on_the_runtime() {
    let code = control_code();
    for forbidden in ["block_on", "spawn_blocking"] {
        assert!(
            !code.contains(forbidden),
            "src/control.rs must not contain `{forbidden}`: the handlers run on the runtime \
             and use the async Inbox (ADR 0043)"
        );
    }
}

/// The file really is the control port (a guard against the module moving or the test
/// silently passing on an empty string).
#[test]
fn control_rs_still_defines_the_server() {
    let code = control_code();
    assert!(
        code.contains("pub struct ControlServer"),
        "control.rs must define the server"
    );
    assert!(code.contains("fn dispatch("), "and the request dispatcher");
}

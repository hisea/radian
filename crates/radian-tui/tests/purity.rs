//! A **structural** test for ADR 0023 U3: `App::update` is pure, so `src/app.rs` must not
//! import or call the things that would make it impure.
//!
//! It is deliberately narrow and honest about what it is: it reads one file's source and greps
//! it. It cannot prove purity (a pure-looking call could still do I/O); it catches the obvious
//! regressions — a `std::fs` import, a `MemoryService` field, a `tokio` handle, an `@`
//! expansion call — which is what the ADR asks for.

/// The file, with comment lines removed so the prose that *describes* the rule does not trip
/// the checks.
fn app_code() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs");
    let source = std::fs::read_to_string(path).expect("app.rs is readable");
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
fn app_rs_is_pure() {
    let code = app_code();
    for forbidden in [
        // No filesystem, no process, no network: those are effects.
        "std::fs",
        "std::process",
        "std::net",
        // No service and no async runtime handle.
        "MemoryService",
        "tokio",
        // The `@` work reads files and directories, so it lives in the loop.
        "mention::expand",
        "mention::complete",
    ] {
        assert!(
            !code.contains(forbidden),
            "src/app.rs must not contain `{forbidden}`: it belongs to the event loop (ADR 0023 U3)"
        );
    }
}

/// The file really is the one with the entry point (a guard against the module moving or the
/// test silently passing on an empty string).
#[test]
fn app_rs_still_defines_update() {
    let code = app_code();
    assert!(
        code.contains("pub fn update(&mut self, input: Input) -> UpdateResult"),
        "app.rs must define the one entry point"
    );
    assert!(code.contains("pub enum Effect"), "and the effect type");
}

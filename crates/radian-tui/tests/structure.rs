//! Structural assertions for ADR 0023 U5 (the Session Facade and the projection split).
//!
//! They read source text and grep it, which is exactly as narrow as it sounds: the compiler
//! already proves the fields are gone, and this pins **where** the session facts live and
//! **who** establishes a session, so a later change cannot quietly move them back.

/// A file with comment lines removed, so the prose that describes the rule does not trip it.
fn code(path: &str) -> String {
    let source = std::fs::read_to_string(path).expect("the source is readable");
    source
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("//!"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn app_rs() -> String {
    code(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"))
}

fn main_rs() -> String {
    code(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli.rs"))
}

/// `App` holds UI state only: every session fact moved to `SessionProjection`, and the app
/// keeps a read handle.
#[test]
fn app_holds_no_session_facts() {
    let code = app_rs();
    for forbidden in [
        "pub turn:",
        "pub context_window:",
        "pub budget_report:",
        "pub tier:",
        "pub usage_input:",
        "pub usage_output:",
        "pub mcp_configured:",
        "pub mcp_open:",
    ] {
        assert!(
            !code.contains(forbidden),
            "`{forbidden}` is a session fact: it belongs to SessionProjection (ADR 0023 U5)"
        );
    }
    assert!(
        code.contains("pub projection: Arc<SessionProjection>"),
        "App must hold the projection as a read handle"
    );
}

/// `cli.rs` (the command body; `main.rs`/`rad.rs` only enter it) does not establish the
/// session itself: `radian_host::open_session` does, so both binaries share one path.
#[test]
fn main_does_not_establish_a_session() {
    let code = main_rs();
    for forbidden in [
        "SessionJournal::create",
        "SessionJournal::resume",
        "Driver::open",
        "Inbox::durable",
        "McpToolProvider::new",
        "MemoryService::open",
        "MemoryTools::new",
        "PermissionHooks::new",
        "HostTools::new",
    ] {
        assert!(
            !code.contains(forbidden),
            "`{forbidden}` establishes a session: it belongs to radian_host::open_session"
        );
    }
    assert!(
        code.contains("radian_host::open_session("),
        "cli.rs must call the shared open_session"
    );
}

/// ADR 0016: `[sandbox] allow_unix_sockets` can only be enforced by the sandbox, so
/// the sockets the TUI resolved must reach `build_sandbox`. A call site that forwards an empty
/// list silently ignores the user's config, and the compiler cannot tell the two apart.
#[test]
fn main_forwards_the_allowed_unix_sockets_to_the_sandbox() {
    let code = main_rs();
    let call = code
        .split_once("radian_tui::profile::build_sandbox(")
        .expect("cli.rs builds the sandbox")
        .1
        .split_once(")?;")
        .expect("the build_sandbox call ends with `)?;`")
        .0;
    assert!(
        call.contains("&allow_unix_sockets"),
        "the sandbox must get the resolved `[sandbox] allow_unix_sockets` (ADR 0016):\n{call}"
    );
}

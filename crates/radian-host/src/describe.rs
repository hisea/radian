//! The startup description of the permission tier and the sandbox, shared by both hosts so
//! their wording cannot drift (dogfooding D5).
//!
//! The two hosts keep only the two phrases that are honestly different: `agent` calls the
//! confined subject "commands and built-in tools" and can talk about MCP, `radian-tui` calls
//! it "shell and the built-in tools". Everything else — including the sentence that must not
//! be weakened, "MCP servers are NOT sandboxed" (ADR 0012 decision 4) — is here once.

use radian_sandbox::{Sandbox, SandboxMode};

/// The host-specific wording. `radian_host::CLI_WORDING` / `radian_host::TUI_WORDING` are the
/// two in the tree; a host may pass its own.
#[derive(Debug, Clone, Copy)]
pub struct Wording {
    /// Appended to the `safe` label after "no command execution" (e.g. `, no MCP tools`).
    pub safe_note: &'static str,
    /// Subject of the confined-mode sentence (e.g. `commands and built-in tools`).
    pub confined_subject: &'static str,
}

/// `agent-cli`'s wording: the line prompt has MCP tools, so it names them.
pub const CLI_WORDING: Wording = Wording {
    safe_note: ", no MCP tools",
    confined_subject: "commands and built-in tools",
};

/// `radian-tui`'s wording: the full-screen host talks about its shell.
pub const TUI_WORDING: Wording = Wording {
    safe_note: "",
    confined_subject: "shell and the built-in tools",
};

/// The two startup lines: what was chosen, and what it actually means. The wording follows
/// ADR 0012 decision 4 and must **not** claim credential isolation.
pub fn describe(
    mode: SandboxMode,
    sandbox: Option<&Sandbox>,
    wording: Wording,
) -> (String, String) {
    match (mode, sandbox) {
        (SandboxMode::Safe, _) => (
            format!(
                "safe (no sandbox needed: no command execution{})",
                wording.safe_note
            ),
            "writes and command execution are denied; the read/list/git tools are confined to \
             the workspace by the tools themselves"
                .into(),
        ),
        (SandboxMode::WorkspaceWrite, Some(sandbox)) => (
            format!(
                "workspace-write · sandbox {} (probe: {})",
                sandbox.backend_name(),
                sandbox.probe().detail
            ),
            format!(
                "{}: writes are confined to the workspace, repository metadata is read-only, \
                 sensitive directories are not readable by default, and the network is off. MCP \
                 servers are NOT sandboxed in this build and run with your full access. The IPC \
                 boundary is not tightened, so this is not a complete credential boundary",
                wording.confined_subject
            ),
        ),
        (SandboxMode::FullAccess, _) => (
            "full-access (chosen explicitly; never a default)".into(),
            "no sandbox: commands can read and write anywhere you can, including the network"
                .into(),
        ),
        // Unreachable: an enforced mode always has a sandbox or the build failed.
        (SandboxMode::WorkspaceWrite, None) => (
            "workspace-write (sandbox missing)".into(),
            "the sandbox is missing; this must not happen".into(),
        ),
    }
}

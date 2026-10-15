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
                "workspace · sandbox {} (probe: {})",
                sandbox.backend_name(),
                sandbox.probe().detail
            ),
            workspace_sandbox_facts(wording),
        ),
        (SandboxMode::FullAccess, _) => (
            "full (chosen explicitly; never a default)".into(),
            "no sandbox: commands can read and write anywhere you can, including the network"
                .into(),
        ),
        // Unreachable: an enforced mode always has a sandbox or the build failed.
        (SandboxMode::WorkspaceWrite, None) => (
            "workspace (sandbox missing)".into(),
            "the sandbox is missing; this must not happen".into(),
        ),
    }
}

/// The `workspace` tier's "what it actually means" sentence. It does not depend on the
/// backend, so it takes only the wording (and is testable without building a `Sandbox`).
/// "network is on" is plain text: this string is printed as-is by both hosts, so it must
/// carry no Markdown (ADR 0027; the model-facing wording in `radian-coding` is separate).
fn workspace_sandbox_facts(wording: Wording) -> String {
    format!(
        "{}: writes are confined to the workspace, repository metadata is read-only, \
         sensitive directories are not readable by default, and the network is on \
         (outbound and local-loopback connect, bind and listen are allowed). MCP \
         servers are NOT sandboxed in this build and run with your full access. The IPC \
         boundary is not tightened, so this is not a complete credential boundary",
        wording.confined_subject
    )
}

/// The startup summary's word for the sandbox. `workspace` names the **real** backend in
/// plain words — `macOS sandbox` for `macos-seatbelt`, `Linux sandbox` for `linux-bwrap`, and
/// the backend's own name for one it does not know — so it can never be read as "no sandbox".
/// The exact backend name is still in the expanded `profile:` line. `safe` has nothing to
/// sandbox (`no exec`: no command execution at all); `full` is `no sandbox`.
pub fn sandbox_word(mode: SandboxMode, backend: Option<&str>) -> String {
    match mode {
        SandboxMode::Safe => "no exec".to_string(),
        SandboxMode::WorkspaceWrite => match backend {
            Some("macos-seatbelt") => "macOS sandbox".to_string(),
            Some("linux-bwrap") => "Linux sandbox".to_string(),
            // A backend this build does not know: say its real name, never "none".
            Some(other) => other.to_string(),
            None => "sandbox missing".to_string(),
        },
        SandboxMode::FullAccess => "no sandbox".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The summary word spells the backend in plain words on both platforms, and an unknown
    /// backend keeps its own name. A word hardcoded to one platform would pass on that
    /// platform and be wrong on the other.
    #[test]
    fn the_summary_word_names_the_backend_in_plain_words() {
        assert_eq!(
            sandbox_word(SandboxMode::WorkspaceWrite, Some("macos-seatbelt")),
            "macOS sandbox"
        );
        assert_eq!(
            sandbox_word(SandboxMode::WorkspaceWrite, Some("linux-bwrap")),
            "Linux sandbox"
        );
        // An unknown backend is shown by its real name, not hidden behind a generic word.
        assert_eq!(
            sandbox_word(SandboxMode::WorkspaceWrite, Some("freebsd-capsicum")),
            "freebsd-capsicum"
        );
        // A missing backend in an enforced mode must not read as "no sandbox".
        assert_eq!(
            sandbox_word(SandboxMode::WorkspaceWrite, None),
            "sandbox missing"
        );
    }

    /// `safe` is "no exec" (there is no command execution to sandbox), `full` is
    /// "no sandbox" (chosen explicitly).
    #[test]
    fn the_summary_word_names_what_the_tier_means() {
        assert_eq!(sandbox_word(SandboxMode::Safe, None), "no exec");
        assert_eq!(sandbox_word(SandboxMode::FullAccess, None), "no sandbox");
    }

    /// The facts sentence is printed as plain text by both hosts, so it carries no Markdown
    /// emphasis (`**on**` used to show as literal stars).
    #[test]
    fn the_workspace_facts_have_no_markdown() {
        let facts = workspace_sandbox_facts(TUI_WORDING);
        assert!(facts.contains("network is on"), "{facts}");
        assert!(!facts.contains("**"), "no Markdown emphasis: {facts}");
        assert!(facts.contains("MCP servers are NOT sandboxed"), "{facts}");
        assert!(
            workspace_sandbox_facts(CLI_WORDING).contains("commands and built-in tools:"),
            "the CLI wording names its own confined subject"
        );
    }
}

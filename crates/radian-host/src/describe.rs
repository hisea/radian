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

/// The startup lines for the `inherited` backend (ADR 0016): the process
/// is already confined, radian adds nothing, and **nothing about the outer sandbox's rules can
/// be claimed** — radian does not know whether it denies reads, keeps `.git` read-only, has the
/// network on or off, or restricts Unix sockets. The `workspace` tier's facts sentence would
/// claim four things radian is not enforcing, so it is not used here.
const INHERITED_PROFILE_LINE: &str = "workspace · sandbox inherited (an outer sandbox confines this process; radian's own rules \
     are not applied)";
const INHERITED_FACTS: &str = "this session runs inside an outer sandbox that radian did not \
                               create; radian adds no sandbox of its own and does not know what \
                               the outer one allows, so the guarantees in force are the outer \
                               sandbox's";

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
        (SandboxMode::WorkspaceWrite, Some(sandbox)) if sandbox.backend_name() == "inherited" => (
            INHERITED_PROFILE_LINE.to_string(),
            INHERITED_FACTS.to_string(),
        ),
        (SandboxMode::WorkspaceWrite, Some(sandbox)) => (
            format!(
                "workspace · sandbox {} (probe: {})",
                sandbox.backend_name(),
                sandbox.probe().detail
            ),
            workspace_sandbox_facts(wording, sandbox.git_writable_except_exec()),
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
/// backend, so it takes only the wording and the git answer (and is testable without building a
/// `Sandbox`). `git_writable` is the sandbox's **effective** answer
/// ([`radian_sandbox::Sandbox::git_writable_except_exec`], ADR 0038 decision 4), not the
/// policy's request: a task agent (or a backend without the carve-out, Linux) keeps the
/// read-only wording, and only a session that can really commit with native git gets the
/// writable one. "network is on" is plain text: this string is printed as-is by both hosts, so
/// it must carry no Markdown (ADR 0027; the model-facing wording in `radian-coding` is
/// separate).
fn workspace_sandbox_facts(wording: Wording, git_writable: bool) -> String {
    let git = if git_writable {
        "repository metadata is writable so native git can commit, branch and stash there, \
         except the entries git can execute (config and hooks), which stay read-only"
    } else {
        "repository metadata is read-only"
    };
    format!(
        "{}: writes are confined to the workspace, {git}, \
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
        let facts = workspace_sandbox_facts(TUI_WORDING, false);
        assert!(facts.contains("network is on"), "{facts}");
        assert!(!facts.contains("**"), "no Markdown emphasis: {facts}");
        assert!(facts.contains("MCP servers are NOT sandboxed"), "{facts}");
        assert!(
            workspace_sandbox_facts(CLI_WORDING, false).contains("commands and built-in tools:"),
            "the CLI wording names its own confined subject"
        );
    }

    /// ADR 0038 decision 4: the facts sentence follows the sandbox's **effective** git access,
    /// not the policy's request. A session that can really commit with native git says so (and
    /// that config and hooks stay read-only); a task agent (or any backend without the
    /// carve-out, Linux) keeps the read-only wording. This is the line the startup block shows
    /// and the `<runtime>` layer carries.
    #[test]
    fn the_workspace_facts_follow_the_effective_git_access() {
        let writable = workspace_sandbox_facts(TUI_WORDING, true);
        assert!(
            writable.contains("native git can commit, branch and stash"),
            "{writable}"
        );
        assert!(writable.contains("config and hooks"), "{writable}");
        assert!(
            !writable.contains("repository metadata is read-only"),
            "{writable}"
        );

        let read_only = workspace_sandbox_facts(TUI_WORDING, false);
        assert!(
            read_only.contains("repository metadata is read-only"),
            "{read_only}"
        );
        assert!(!read_only.contains("native git can commit"), "{read_only}");
    }

    /// ADR 0016 / acceptance 6: an `inherited` sandbox says only that an
    /// outer sandbox confines the process. It must **not** carry the `workspace` tier's facts
    /// sentence, which would claim a deny-read list, a read-only `.git`, a network state, a
    /// Unix-socket whitelist and an unsandboxed MCP — none of which radian is enforcing here.
    #[test]
    fn the_inherited_sandbox_line_claims_no_local_rules() {
        let dir =
            std::env::temp_dir().join(format!("radian-describe-inherited-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sandbox = Sandbox::inherited(&dir);
        let (line, facts) = describe(SandboxMode::WorkspaceWrite, Some(&sandbox), TUI_WORDING);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(line.contains("sandbox inherited"), "{line}");
        // The line is the exact one the ADR names, not the `workspace` tier's `(probe: …)`
        // line with `inherited` as the backend name in it: the wording is the whole point here.
        assert!(
            line.contains("an outer sandbox confines this process"),
            "{line}"
        );
        assert!(
            line.contains("radian's own rules are not applied"),
            "{line}"
        );
        assert!(facts.contains("outer sandbox"), "{facts}");
        for forbidden in [
            "not readable",
            "read-only",
            "network is on",
            "network is off",
            "unix socket",
            "Unix socket",
            "MCP",
        ] {
            assert!(!line.contains(forbidden), "{forbidden:?} in {line}");
            assert!(!facts.contains(forbidden), "{forbidden:?} in {facts}");
        }
        // The sentence that names the tier's local rules must not appear at all. This is the
        // mutation: reusing `workspace_sandbox_facts` for `inherited`.
        assert!(
            !facts.contains("writes are confined to the workspace"),
            "{facts}"
        );
    }

    /// A backend whose effective git answer is fixed, so the wiring from `describe` through
    /// `Sandbox::git_writable_except_exec` is testable without a real probe.
    struct GitBackend(bool);

    impl radian_sandbox::SandboxBackend for GitBackend {
        fn name(&self) -> &'static str {
            "test-git"
        }

        fn probe(&self) -> Result<radian_sandbox::ProbeReport, radian_sandbox::SandboxError> {
            Ok(radian_sandbox::ProbeReport {
                backend: self.name(),
                detail: "test".into(),
            })
        }

        fn command(
            &self,
            policy: &radian_sandbox::SandboxPolicy,
            program: &std::ffi::OsStr,
            args: &[std::ffi::OsString],
        ) -> Result<std::process::Command, radian_sandbox::SandboxError> {
            let mut command = std::process::Command::new(program);
            command.args(args).current_dir(&policy.workspace);
            Ok(command)
        }

        fn git_writable_except_exec(&self, _policy: &radian_sandbox::SandboxPolicy) -> bool {
            self.0
        }
    }

    static GIT_WRITABLE: GitBackend = GitBackend(true);
    static GIT_READ_ONLY: GitBackend = GitBackend(false);

    /// The startup line reads the **sandbox's** effective answer, not a hand-passed flag: with a
    /// backend that enforces the carve-out the facts say native git can commit, and with one
    /// that does not (the Linux shape) they stay read-only. Swapping the two is the mutation
    /// this catches.
    #[test]
    fn the_startup_line_reports_the_sandboxes_effective_git_access() {
        let dir = std::env::temp_dir().join(format!("radian-describe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let writable = Sandbox::from_backend(&GIT_WRITABLE, SandboxMode::WorkspaceWrite, &dir)
            .expect("a test sandbox");
        let (_, facts) = describe(SandboxMode::WorkspaceWrite, Some(&writable), TUI_WORDING);
        assert!(
            facts.contains("native git can commit, branch and stash"),
            "{facts}"
        );

        let read_only = Sandbox::from_backend(&GIT_READ_ONLY, SandboxMode::WorkspaceWrite, &dir)
            .expect("a test sandbox");
        let (_, facts) = describe(SandboxMode::WorkspaceWrite, Some(&read_only), TUI_WORDING);
        assert!(
            facts.contains("repository metadata is read-only"),
            "{facts}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! The sandbox-denial marker (slice 1 of `sandbox-auto-allow-shell`).
//!
//! When a sandboxed command fails **because of the sandbox** — a write outside the
//! workspace or into a protected `.git` entry, a read of the deny-read list, a denied
//! network connect, a denied Unix socket — the `shell` tool's result carries the fixed
//! marker `sandbox denied: <category>`, classified here from errno text and the policy's
//! paths. When the failure cannot be told apart from an ordinary command failure, there
//! is no marker: over-marking would teach the model that its own bugs are the sandbox.
//!
//! What each backend actually returns (measured for Linux on this box, bwrap 0.11.1;
//! macOS seatbelt is the expected text, **untested on this box**):
//!
//! | category | Linux (bwrap + seccomp, verified) | macOS (seatbelt, expected) |
//! |---|---|---|
//! | `filesystem-write` | `EROFS` ("Read-only file system") or `EACCES` ("Permission denied") writing outside the workspace | deny rule → `EPERM` ("Operation not permitted") |
//! | `git` | `EROFS`/`EACCES` writing a discovered `.git` path | deny rule → `EPERM` |
//! | `sensitive-read` | `ENOENT` ("No such file or directory") for a masked directory (tmpfs over it); a masked file reads as empty `/dev/null`, i.e. no failure at all | deny rule → `EPERM` |
//! | `network` | `ENETUNREACH` ("Network is unreachable") with the network off | deny rule → `EPERM` (expected) |
//! | `unix-socket` | seccomp denies `socket(AF_UNIX, …)` and `io_uring_setup` with `EPERM` ("Operation not permitted") | `(deny network-outbound (remote unix-socket))` → refused connect (expected) |
//!
//! The classifier is deliberately ordered: a path match against the policy's lists wins
//! over a bare errno, because `EPERM` alone is ambiguous (seccomp's socket deny and
//! seatbelt's read deny share it). A command that never mentions a listed path and fails
//! with `EPERM` reads as `unix-socket` on Linux, where the seccomp filter is the only
//! thing in the sandbox that produces it.

use crate::policy::SandboxPolicy;

/// The fixed marker prefix the `shell` tool puts in a result refused by the sandbox.
pub const MARKER_PREFIX: &str = "sandbox denied: ";

/// Why the sandbox refused a command. The string form is what the marker carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialCategory {
    /// A write outside the workspace.
    FilesystemWrite,
    /// A write into a discovered `.git` entry the backend keeps read-only.
    Git,
    /// A read of the deny-read list.
    SensitiveRead,
    /// A network connect the sandbox denied.
    Network,
    /// A Unix-socket connect (or socket creation) the sandbox denied.
    UnixSocket,
}

impl DenialCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FilesystemWrite => "filesystem-write",
            Self::Git => "git",
            Self::SensitiveRead => "sensitive-read",
            Self::Network => "network",
            Self::UnixSocket => "unix-socket",
        }
    }

    /// The full marker line for a shell result.
    pub fn marker(self) -> String {
        format!("{MARKER_PREFIX}{}", self.as_str())
    }
}

/// Classifies a failed sandboxed command's stderr. `command` is the shell text (used only
/// to match the policy's listed paths); `stderr` is the command's combined failure text.
/// Returns `None` for an ordinary command failure, which carries no marker.
pub fn classify_denial(
    policy: &SandboxPolicy,
    command: &str,
    stderr: &str,
) -> Option<DenialCategory> {
    if mentions_any(command, &policy.deny_read) && looks_like_errno(stderr) {
        return Some(DenialCategory::SensitiveRead);
    }
    if mentions_any(command, &policy.read_only)
        && (stderr.contains("Read-only file system")
            || stderr.contains("Permission denied")
            || stderr.contains("Operation not permitted"))
    {
        return Some(DenialCategory::Git);
    }
    if stderr.contains("Operation not permitted") {
        return Some(DenialCategory::UnixSocket);
    }
    if stderr.contains("Read-only file system") || stderr.contains("Permission denied") {
        return Some(DenialCategory::FilesystemWrite);
    }
    if stderr.contains("Network is unreachable") {
        return Some(DenialCategory::Network);
    }
    None
}

/// Whether `stderr` carries any errno text at all (a bare empty stderr with a listed path
/// is not enough: a masked file reads as empty `/dev/null` without failing, and must not
/// be marked).
fn looks_like_errno(stderr: &str) -> bool {
    stderr.contains("Operation not permitted")
        || stderr.contains("Permission denied")
        || stderr.contains("No such file or directory")
        || stderr.contains("Read-only file system")
        || stderr.contains("Network is unreachable")
}

/// Whether the shell text names one of `paths` (a substring match on the canonical path:
/// best effort — a command can spell a path through a symlink or a relative form the
/// policy does not list, and then there is honestly no marker).
fn mentions_any(command: &str, paths: &[std::path::PathBuf]) -> bool {
    paths
        .iter()
        .any(|path| command.contains(&path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        let ws = std::env::temp_dir().join(format!("radian-deny-test-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        SandboxPolicy::new(crate::policy::SandboxMode::WorkspaceWrite, &ws).unwrap()
    }

    /// EROFS writing outside the workspace reads as `filesystem-write`. Removing the
    /// read-only-filesystem arm makes this ordinary, so this test is that mutation's
    /// catcher.
    #[test]
    fn an_outside_write_with_erofs_is_a_filesystem_write() {
        let policy = policy();
        assert_eq!(
            classify_denial(
                &policy,
                "echo x > /tmp/somewhere/else.txt",
                "/bin/sh: 1: cannot create /tmp/somewhere/else.txt: Read-only file system"
            ),
            Some(DenialCategory::FilesystemWrite)
        );
    }

    /// A write into a discovered `.git` entry reads as `git`, not as a plain outside
    /// write. Collapsing the two loses the category, so this test pins the split.
    #[test]
    fn a_git_write_is_git_not_filesystem_write() {
        let mut policy = policy();
        let git = policy.workspace.join(".git").join("hooks");
        policy.read_only.push(git.clone());
        let command = format!("echo x > {}", git.join("evil").display());
        assert_eq!(
            classify_denial(
                &policy,
                &command,
                "/bin/sh: 1: cannot create it: Read-only file system"
            ),
            Some(DenialCategory::Git)
        );
    }

    /// A masked deny-read directory fails with ENOENT; with the path in the command it
    /// reads as `sensitive-read`. Dropping the deny-list arm loses it.
    #[test]
    fn a_masked_sensitive_read_is_a_sensitive_read() {
        let mut policy = policy();
        let secret =
            std::env::temp_dir().join(format!("radian-deny-secret-{}", std::process::id()));
        std::fs::create_dir_all(&secret).unwrap();
        let secret = std::fs::canonicalize(&secret).unwrap();
        policy.deny_read.push(secret.clone());
        let command = format!("cat {}", secret.join("id").display());
        assert_eq!(
            classify_denial(&policy, &command, "cat: No such file or directory"),
            Some(DenialCategory::SensitiveRead)
        );
        let _ = std::fs::remove_dir_all(&secret);
    }

    /// `EPERM` with no listed path is the seccomp socket deny on Linux
    /// (`socket(AF_UNIX, …)` and `io_uring_setup` return it). Nothing else in a Linux
    /// `workspace` sandbox produces it.
    #[test]
    fn a_bare_eperm_is_a_unix_socket() {
        let policy = policy();
        assert_eq!(
            classify_denial(
                &policy,
                "python3 -c 'import socket'",
                "PermissionError: [Errno 1] Operation not permitted"
            ),
            Some(DenialCategory::UnixSocket)
        );
    }

    /// `EPERM` for a deny-listed path is the read deny, not a socket: the path match
    /// wins over the bare errno (macOS seatbelt denies reads with `EPERM`; expected,
    /// untested on this box).
    #[test]
    fn an_eperm_on_a_deny_listed_path_is_a_sensitive_read() {
        let mut policy = policy();
        let secret = std::env::temp_dir();
        policy.deny_read.push(secret.clone());
        let command = format!("cat {}/some-key", secret.display());
        assert_eq!(
            classify_denial(&policy, &command, "cat: Operation not permitted"),
            Some(DenialCategory::SensitiveRead)
        );
    }

    /// An unreachable network reads as `network`. A refused connection does not: with no
    /// listener the sandbox cannot be told apart from an ordinary failure, and there is
    /// honestly no marker.
    #[test]
    fn an_unreachable_network_is_network_and_a_refused_one_is_nothing() {
        let policy = policy();
        assert_eq!(
            classify_denial(&policy, "connect", "Network is unreachable"),
            Some(DenialCategory::Network)
        );
        assert_eq!(
            classify_denial(&policy, "connect", "Connection refused"),
            None
        );
    }

    /// An ordinary command failure (`exit 3`, a missing binary, a failing test) carries
    /// no marker. Marking everything would teach the model its own bugs are the sandbox.
    #[test]
    fn an_ordinary_failure_has_no_marker() {
        let policy = policy();
        for stderr in ["", "assertion failed", "command not found", "FAILED"] {
            assert_eq!(
                classify_denial(&policy, "cargo test", stderr),
                None,
                "{stderr:?}"
            );
        }
    }
}

//! The sandbox-denial marker (slice 1 of `sandbox-auto-allow-shell`).
//!
//! When a sandboxed command fails **because of the sandbox** — a write outside the
//! workspace or into a protected `.git` entry, a read of the deny-read list, a denied
//! network connect, a denied Unix socket — the `shell` tool's result carries the fixed
//! marker `sandbox denied: <category>`, classified here from errno text and the policy's
//! paths. When the failure cannot be told apart from an ordinary command failure, there
//! is no marker: over-marking would teach the model that its own bugs are the sandbox.
//!
//! What each backend actually returns (Linux measured with bwrap 0.11.1; macOS measured
//! on macOS 27.0.1 arm64 with `sandbox-exec`):
//!
//! | category | Linux (bwrap + seccomp) | macOS (seatbelt) |
//! |---|---|---|
//! | `filesystem-write` | `EROFS` ("Read-only file system") or `EACCES` ("Permission denied") writing outside the workspace | deny rule → `EPERM`: `/bin/sh: <path>: Operation not permitted` |
//! | `git` | `EROFS`/`EACCES` writing a discovered `.git` path | deny rule → `EPERM`: `/bin/sh: <git path>: Operation not permitted`, or git's own `could not lock config file .git/config: Operation not permitted` / `Unable to create '.../.git/index.lock': Operation not permitted` |
//! | `sensitive-read` | `ENOENT` ("No such file or directory") for a masked directory (a masked file reads as empty `/dev/null`, i.e. no failure at all) | deny rule → `EPERM`: `cat: <deny path>: Operation not permitted` |
//! | `network` | `ENETUNREACH` ("Network is unreachable") with the network off | deny rule → `EPERM`: `PermissionError: [Errno 1] Operation not permitted` on `connect` with the network off (indistinguishable from the other EPERM denials, so unmarked — see below) |
//! | `unix-socket` | seccomp denies `socket(AF_UNIX, …)` and `io_uring_setup` with `EPERM` ("Operation not permitted") | `(deny network-outbound (remote unix-socket))` → refused connect: `PermissionError: [Errno 1] Operation not permitted`. Only the connect is denied: creating an unconnected `AF_UNIX` socket succeeds, so a create-only probe is not a denial at all |
//!
//! Seatbelt returns `EPERM` for a write, a read, a network connect and a socket connect
//! alike, so on macOS a bare `EPERM` is ambiguous and carries **no marker**. The command
//! text and the policy's paths disambiguate the way the Linux arms do: a path under
//! `deny_read` reads as `sensitive-read`, a discovered `.git` path (or a native git failure
//! on a `.git/` entry) reads as `git`, an `AF_UNIX` connect reads as `unix-socket`, and a
//! redirect/write target outside the workspace reads as `filesystem-write`. Anything still
//! ambiguous is an ordinary failure.
//!
//! The classifier is deliberately ordered: a path match against the policy's lists wins
//! over a bare errno, because `EPERM` alone is ambiguous (seccomp's socket deny and
//! seatbelt's read deny share it). A command that never mentions a listed path and fails
//! with `EPERM` reads as `unix-socket` on Linux, where the seccomp filter is the only
//! thing in the sandbox that produces it.

use std::path::{Path, PathBuf};

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
///
/// The mapping is backend-aware: each OS builds with one sandbox backend (seatbelt on
/// macOS, bwrap + seccomp on Linux), and the two report different errnos for the same
/// denial. The caller (`radian-coding`'s `shell` tool) needs no change: it already passes
/// the sandbox's policy, and the policy's paths are what disambiguate seatbelt's
/// catch-all `EPERM`.
pub fn classify_denial(
    policy: &SandboxPolicy,
    command: &str,
    stderr: &str,
) -> Option<DenialCategory> {
    #[cfg(target_os = "macos")]
    {
        classify_macos(policy, command, stderr)
    }
    #[cfg(not(target_os = "macos"))]
    {
        classify_linux(policy, command, stderr)
    }
}

/// The Linux mapping, unchanged: the seccomp filter is the only thing in a `workspace`
/// sandbox that produces `EPERM`, so a bare `EPERM` is the socket deny; `EROFS`/`EACCES`
/// are outside writes (or git writes, when the command names a discovered `.git` path).
#[cfg(not(target_os = "macos"))]
fn classify_linux(policy: &SandboxPolicy, command: &str, stderr: &str) -> Option<DenialCategory> {
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

/// The macOS mapping, from what seatbelt was measured to return (see the table above):
/// `EPERM` for every denial, so a bare `EPERM` with no path and no address family in the
/// command is ambiguous and unmarked. The policy's paths disambiguate: `stderr` is matched
/// as well as `command`, because native git names the denied `.git` entry (`.../.git/
/// index.lock`) in its own error while the command never spells it.
fn classify_macos(policy: &SandboxPolicy, command: &str, stderr: &str) -> Option<DenialCategory> {
    if mentions_listed_path(&policy.deny_read, command, stderr) && looks_like_errno(stderr) {
        return Some(DenialCategory::SensitiveRead);
    }
    if mentions_listed_path(&policy.read_only, command, stderr)
        && (stderr.contains("Read-only file system")
            || stderr.contains("Permission denied")
            || stderr.contains("Operation not permitted"))
    {
        return Some(DenialCategory::Git);
    }
    // A native git invocation denied on a `.git/` entry it never spells: `git config
    // core.hooksPath` fails as `could not lock config file .git/config`, and the command
    // names only the value being set (which may itself live outside the workspace). Without
    // this the write-target arm below would mis-mark it `filesystem-write`.
    if is_native_git_denied_on_dot_git(command, stderr) {
        return Some(DenialCategory::Git);
    }
    if stderr.contains("Network is unreachable") {
        return Some(DenialCategory::Network);
    }
    if !stderr.contains("Operation not permitted") {
        return None;
    }
    // From here the failure is an `EPERM`, which on seatbelt is every denial at once.
    // Only the address family names a socket connect: a command spelling `AF_UNIX` that
    // fails with `EPERM` was refused the connect (creating the socket alone succeeds).
    if command.contains("AF_UNIX") {
        return Some(DenialCategory::UnixSocket);
    }
    // A redirect/write target outside the workspace is the denied write. A network connect
    // denied with the network off also lands here as `EPERM`, but with no path in the
    // command it cannot be told apart and stays unmarked.
    if mentions_path_outside_workspace(policy, command) {
        return Some(DenialCategory::FilesystemWrite);
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
#[cfg(not(target_os = "macos"))]
fn mentions_any(command: &str, paths: &[std::path::PathBuf]) -> bool {
    paths
        .iter()
        .any(|path| command.contains(&path.display().to_string()))
}

/// Whether `command` or `stderr` names one of `paths`, resolving the `/var` ↔
/// `/private/var` (and `/tmp` ↔ `/private/tmp`) alias first: the shell prints the path as
/// spelled while the policy holds the canonical form, so a plain substring match misses
/// every denial on macOS. Each absolute-path token in the text is resolved through its
/// nearest existing ancestor and compared by components, so neither side needs spelling
/// the alias the other used.
fn mentions_listed_path(paths: &[PathBuf], command: &str, stderr: &str) -> bool {
    paths.iter().any(|listed| {
        let spelled = listed.display().to_string();
        command.contains(&spelled)
            || stderr.contains(&spelled)
            || text_names_resolved(listed, command)
            || text_names_resolved(listed, stderr)
    })
}

/// Whether any absolute-path token in `text` resolves to `listed` or under it.
fn text_names_resolved(listed: &Path, text: &str) -> bool {
    absolute_tokens(text).any(|token| resolved_is_listed_or_under(listed, token))
}

/// Whether the resolved form of the absolute-path token `token` is `listed` or lives under
/// it. Resolution is through the nearest existing ancestor, so a denied target that does
/// not exist yet (a redirect creating a new file) still matches the listed directory.
fn resolved_is_listed_or_under(listed: &Path, token: &str) -> bool {
    let path = Path::new(token);
    let resolved =
        crate::policy::canonicalize_existing_prefix(path).unwrap_or_else(|| path.to_path_buf());
    resolved == *listed || resolved.starts_with(listed)
}

/// Whether `command` names a redirect/write target outside the workspace: an absolute
/// path that resolves outside the workspace (and outside the deny/read-only lists, which
/// the earlier arms already claimed), or a relative path that lexically escapes it (a
/// `../outside/...` redirect). `/dev/null` is excluded: the profile always allows it, so
/// it can never be the denied target. Only the command is scanned, never `stderr`: a
/// Python traceback embeds its stdlib paths, which resolve outside the workspace without
/// naming any target at all.
fn mentions_path_outside_workspace(policy: &SandboxPolicy, command: &str) -> bool {
    if absolute_tokens(command).any(|token| {
        if token == "/dev/null" {
            return false;
        }
        let path = Path::new(token);
        let resolved =
            crate::policy::canonicalize_existing_prefix(path).unwrap_or_else(|| path.to_path_buf());
        !resolved.starts_with(&policy.workspace)
            && !policy
                .deny_read
                .iter()
                .any(|listed| resolved == *listed || resolved.starts_with(listed))
            && !policy
                .read_only
                .iter()
                .any(|listed| resolved == *listed || resolved.starts_with(listed))
    }) {
        return true;
    }
    relative_tokens(command).any(|token| {
        let joined = policy.workspace.join(token);
        !lexical_normalize(&joined).starts_with(&policy.workspace)
    })
}

/// Whether `command` is a native git invocation failing on a `.git/` entry it never
/// spells: the word `git` in the command, a `.git/` path in `stderr`, and an `EPERM`.
/// (The discovered-path arm above already covers commands that spell the entry.)
fn is_native_git_denied_on_dot_git(command: &str, stderr: &str) -> bool {
    if !stderr.contains("Operation not permitted") || !stderr.contains(".git/") {
        return false;
    }
    command
        .split(|c: char| {
            matches!(
                c,
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '"' | '\''
            )
        })
        .any(|token| token == "git" || token.ends_with("/git"))
}

/// Absolute-path tokens in free text: runs starting at `/` up to a character that cannot
/// be in a printed path. The shell and Python tracebacks quote and punctuate paths
/// (`'…/.git/index.lock':`, `s.connect("/tmp/x")`), so quotes, brackets, commas and the
/// colon all terminate a token.
fn absolute_tokens(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            let mut j = i + 1;
            while j < bytes.len()
                && !matches!(
                    bytes[j],
                    b' ' | b'\t'
                        | b'\n'
                        | b'"'
                        | b'\''
                        | b'('
                        | b')'
                        | b','
                        | b';'
                        | b'|'
                        | b'&'
                        | b'<'
                        | b'>'
                        | b'`'
                        | b'$'
                        | b':'
                )
            {
                j += 1;
            }
            if j > i + 1 {
                tokens.push(&text[i..j]);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    tokens.into_iter()
}

/// Relative-path tokens in a command: whitespace- and operator-separated words that name
/// a path below or beside the working directory (`../outside/x`, `sub/file`). A bare
/// `..` (a directory change, not a write target) does not count.
fn relative_tokens(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| {
            matches!(
                c,
                ' ' | '\t'
                    | '\n'
                    | ';'
                    | '|'
                    | '&'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '\''
                    | '"'
                    | '`'
                    | '$'
                    | ','
            )
        })
        .filter(|token| {
            let token = token.trim();
            !token.is_empty()
                && token.contains('/')
                && !token.starts_with('/')
                && (token.starts_with("..") || !token.starts_with('.'))
        })
}

/// The lexical form of `path` with `.` removed and `..` applied, without touching the
/// filesystem (the target may not exist, and resolving symlinks would answer a different
/// question: where the shell *spelled* the target, not where it leads).
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component::{CurDir, Normal, ParentDir, Prefix, RootDir};
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Prefix(_) | RootDir | Normal(_) => out.push(component),
            CurDir => {}
            ParentDir => {
                out.pop();
            }
        }
    }
    out
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
    #[cfg(target_os = "linux")]
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
        let command = format!("cat {}", secret.join("key").display());
        assert_eq!(
            classify_denial(&policy, &command, "cat: No such file or directory"),
            Some(DenialCategory::SensitiveRead)
        );
        let _ = std::fs::remove_dir_all(&secret);
    }

    /// `EPERM` with no listed path is the seccomp socket deny on Linux
    /// (`socket(AF_UNIX, …)` and `io_uring_setup` return it). Nothing else in a Linux
    /// `workspace` sandbox produces it.
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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

    /// Seatbelt denies an outside write with `EPERM` naming the target: with the target
    /// outside the workspace in the command, it reads as `filesystem-write`, not
    /// `unix-socket` (the old Linux mapping). Measured:
    /// `/bin/sh: <path>: Operation not permitted`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_outside_write_with_eperm_is_a_filesystem_write() {
        let policy = policy();
        let outside =
            std::env::temp_dir().join(format!("radian-deny-outside-{}", std::process::id()));
        let target = outside.join("escaped.txt");
        let command = format!("echo x > {}", target.display());
        assert_eq!(
            classify_denial(
                &policy,
                &command,
                &format!("/bin/sh: {}: Operation not permitted", target.display())
            ),
            Some(DenialCategory::FilesystemWrite)
        );
    }

    /// The same through a `..` redirect: the relative target escapes the workspace, so it
    /// reads as `filesystem-write` too.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_dotdot_write_with_eperm_is_a_filesystem_write() {
        let policy = policy();
        let command = format!(
            "cd {} && printf x > ../outside/dotdot.txt",
            policy.workspace.display()
        );
        assert_eq!(
            classify_denial(
                &policy,
                &command,
                "/bin/sh: ../outside/dotdot.txt: Operation not permitted"
            ),
            Some(DenialCategory::FilesystemWrite)
        );
    }

    /// Seatbelt denies a write into a discovered `.git` entry with `EPERM` naming it:
    /// with the entry in the command, it reads as `git`. Measured:
    /// `/bin/sh: <git path>: Operation not permitted`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_git_write_with_eperm_is_git() {
        let mut policy = policy();
        let git = policy.workspace.join(".git").join("hooks");
        policy.read_only.push(git.clone());
        let command = format!("echo x > {}", git.join("evil").display());
        assert_eq!(
            classify_denial(
                &policy,
                &command,
                &format!(
                    "/bin/sh: {}: Operation not permitted",
                    git.join("evil").display()
                )
            ),
            Some(DenialCategory::Git)
        );
    }

    /// Native git names the locked `.git` entry in its own error while the command never
    /// spells it: `git add` denied on the index reads as `git`. Measured:
    /// `fatal: Unable to create '.../.git/index.lock': Operation not permitted`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_native_git_lock_failure_is_git() {
        let mut policy = policy();
        let git = policy.workspace.join(".git");
        policy.read_only.push(git.clone());
        let command = format!("cd {} && git add new.txt", policy.workspace.display());
        let stderr = format!(
            "fatal: Unable to create '{}/index.lock': Operation not permitted",
            git.display()
        );
        assert_eq!(
            classify_denial(&policy, &command, &stderr),
            Some(DenialCategory::Git)
        );
    }

    /// `git config core.hooksPath` is denied on `.git/config`, which the command never
    /// spells — and the value being set may itself live outside the workspace. The
    /// `.git/` entry in git's own error keeps it `git`, not `filesystem-write`. Measured:
    /// `error: could not lock config file .git/config: Operation not permitted`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_git_config_hookspath_denial_is_git_not_a_write() {
        let mut policy = policy();
        let git = policy.workspace.join(".git");
        policy.read_only.push(git.clone());
        let command = format!(
            "cd {} && git config core.hooksPath /tmp/elsewhere",
            policy.workspace.display()
        );
        assert_eq!(
            classify_denial(
                &policy,
                &command,
                "error: could not lock config file .git/config: Operation not permitted"
            ),
            Some(DenialCategory::Git)
        );
    }

    /// Seatbelt denies a deny-listed read with `EPERM` naming it: with the path in the
    /// command, it reads as `sensitive-read`. The shell prints the path as spelled
    /// (`/var/...`) while the policy holds the canonical form (`/private/var/...`); the
    /// match resolves the alias. Measured: `cat: <deny path>: Operation not permitted`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_read_deny_with_eperm_is_a_sensitive_read_through_the_alias() {
        let mut policy = policy();
        let secret =
            std::env::temp_dir().join(format!("radian-deny-secret-macos-{}", std::process::id()));
        std::fs::create_dir_all(&secret).unwrap();
        let canonical = std::fs::canonicalize(&secret).unwrap();
        policy.deny_read.push(canonical);
        // Spelled, not resolved: this is the form the shell prints.
        let key = secret.join("key");
        let command = format!("cat {}", key.display());
        assert_eq!(
            classify_denial(
                &policy,
                &command,
                &format!("cat: {}: Operation not permitted", key.display())
            ),
            Some(DenialCategory::SensitiveRead)
        );
        let _ = std::fs::remove_dir_all(&secret);
    }

    /// The Linux `an_eperm_on_a_deny_listed_path` twin for macOS: a deny-listed path
    /// spelled canonically reads as `sensitive-read` (the alias test above covers the
    /// `/var` spelling).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_eperm_on_a_canonically_spelled_deny_path_is_a_sensitive_read() {
        let mut policy = policy();
        let secret = std::env::temp_dir();
        let canonical = std::fs::canonicalize(&secret).unwrap();
        policy.deny_read.push(canonical.clone());
        let command = format!("cat {}/some-key", canonical.display());
        assert_eq!(
            classify_denial(&policy, &command, "cat: Operation not permitted"),
            Some(DenialCategory::SensitiveRead)
        );
    }

    /// Seatbelt denies a Unix-socket connect with `EPERM`, and only the address family
    /// names the operation: a command spelling `AF_UNIX` that fails with `EPERM` was
    /// refused the connect (creating the socket alone succeeds on macOS). The socket
    /// path lives outside the workspace, so this arm must win over the write-target arm
    /// below it. Measured: `PermissionError: [Errno 1] Operation not permitted`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_unix_socket_connect_with_eperm_is_a_unix_socket() {
        let policy = policy();
        let command = "python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); \
             s.connect(\"/private/tmp/radian-esc-1.sock\")'";
        assert_eq!(
            classify_denial(
                &policy,
                command,
                "PermissionError: [Errno 1] Operation not permitted"
            ),
            Some(DenialCategory::UnixSocket)
        );
    }

    /// On seatbelt a bare `EPERM` — no listed path, no address family — is every denial
    /// at once (a write, a read, a socket), so it carries **no marker**. Mapping it back
    /// to `unix-socket` unconditionally (the old Linux mapping) fails this test, so this
    /// test is that mutation's catcher.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_bare_eperm_has_no_marker() {
        let policy = policy();
        assert_eq!(
            classify_denial(
                &policy,
                "python3 -c 'import socket'",
                "PermissionError: [Errno 1] Operation not permitted"
            ),
            None
        );
    }

    /// With the network off seatbelt denies a loopback connect with `EPERM`, and the
    /// command names no path and no socket family: it cannot be told apart and stays
    /// unmarked. Measured: `PermissionError: [Errno 1] Operation not permitted` on
    /// `sock.connect(sa)`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_network_deny_with_eperm_has_no_marker() {
        let policy = policy();
        assert_eq!(
            classify_denial(
                &policy,
                "python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\",9),2)'",
                "PermissionError: [Errno 1] Operation not permitted"
            ),
            None
        );
    }

    /// A bare `EACCES`/`EROFS` with no listed path is an ordinary permission failure on
    /// macOS — seatbelt denies with `EPERM`, never these — so unlike Linux it carries no
    /// marker.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_bare_eacces_has_no_marker() {
        let policy = policy();
        assert_eq!(
            classify_denial(
                &policy,
                "echo x > /tmp/somewhere/else.txt",
                "/bin/sh: 1: cannot create /tmp/somewhere/else.txt: Permission denied"
            ),
            None
        );
    }
}

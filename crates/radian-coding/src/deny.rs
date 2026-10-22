//! The `full`-tier deny set for the file tools (ADR 0039 decision 4).
//!
//! In the `full` tier `read`, `grep` and `glob` also read any other absolute or `~` path,
//! except this deny set: the sandbox's [`DEFAULT_DENY_READ`](radian_sandbox::DEFAULT_DENY_READ)
//! entries under `$HOME`, and the `$RADIAN_HOME` paths the sandbox denies (the actual
//! sessions directory, `memory/`, `credentials/` and `trust/`). Both halves come from the
//! same source the sandbox policy uses
//! ([`default_deny_read_for_home`](radian_sandbox::default_deny_read_for_home) plus the
//! host's deny-read list), so the two can never differ.
//!
//! The deny check runs on the `realpath`ed path, so a symlink into a denied directory
//! refuses; the file itself opens with `O_NOFOLLOW` on its last component.

use std::path::{Path, PathBuf};

/// The denied paths, canonicalized the way the sandbox policy canonicalizes them.
#[derive(Debug, Clone, Default)]
pub struct DenySet {
    paths: Vec<PathBuf>,
}

impl DenySet {
    /// Nothing is denied. Tests and embedders that never exercise the `full` tier use this;
    /// the tier check refuses outside paths before the set is consulted.
    pub fn empty() -> Self {
        Self { paths: Vec::new() }
    }

    /// The production set: the sandbox's default deny-read entries for `home` (usually the
    /// process `HOME`) plus the host's extra denied paths (the actual sessions directory,
    /// `memory/`, `credentials/`, `trust/` under `$RADIAN_HOME`). Each extra is resolved
    /// through [`canonicalize_existing_prefix`](radian_sandbox::canonicalize_existing_prefix)
    /// exactly like [`SandboxPolicy::deny_read`](radian_sandbox::SandboxPolicy::deny_read),
    /// so an entry for a not-yet-created directory still binds.
    pub fn from_home_and_extra(home: Option<PathBuf>, extra: &[PathBuf]) -> Self {
        let mut paths = match home {
            Some(home) => radian_sandbox::default_deny_read_for_home(&home),
            None => Vec::new(),
        };
        for path in extra {
            if let Some(resolved) = radian_sandbox::canonicalize_existing_prefix(path)
                && !paths.contains(&resolved)
            {
                paths.push(resolved);
            }
        }
        paths.sort();
        paths.dedup();
        Self { paths }
    }

    /// Whether `canonical` (already `realpath`ed by the caller) is denied: itself or under
    /// one of the denied paths. Returns the matching denied path for the refusal message.
    pub fn denies(&self, canonical: &Path) -> Option<&Path> {
        radian_sandbox::deny_read_match(&self.paths, canonical).map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_set_denies_nothing() {
        let set = DenySet::empty();
        assert!(set.denies(Path::new("/etc/passwd")).is_none());
        assert!(set.denies(Path::new("/")).is_none());
    }

    #[test]
    fn an_extra_path_denies_itself_and_its_children_but_not_its_siblings() {
        let dir = std::env::temp_dir().join(format!("radian-deny-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        // Canonicalize: the probed path is `realpath`ed before the check, so the test must
        // compare against the same spelling (on macOS `/tmp` is a link to `/private/tmp`).
        let dir = std::fs::canonicalize(&dir).unwrap();
        let set = DenySet::from_home_and_extra(None, &[dir.join("sessions")]);
        assert_eq!(
            set.denies(&dir.join("sessions")),
            Some(dir.join("sessions").as_path())
        );
        assert_eq!(
            set.denies(&dir.join("sessions").join("s.jsonl")),
            Some(dir.join("sessions").as_path())
        );
        assert!(set.denies(&dir.join("other")).is_none());
        // `/a/bc` is not inside `/a/b`.
        assert!(set.denies(&dir.join("sessions-backup")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

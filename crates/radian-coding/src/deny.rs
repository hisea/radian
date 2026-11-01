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
//!
//! One exception (ADR 0039 decision 4): the memory scope directories the file tools opened as
//! writable roots. The exception is derived from those roots' canonical paths, so a scope that
//! was never created, or one replaced after the roots were opened, cannot widen it to the
//! memory directory. Everything else under `memory/` keeps the deny text, in the file tools;
//! the sandbox policy (the shell) has no exception and denies the whole directory.

use std::path::{Path, PathBuf};

use crate::memory_roots::MemoryRoots;

/// The denied paths, canonicalized the way the sandbox policy canonicalizes them, plus the
/// one documented exception: the writable memory scopes (ADR 0039 decision 4).
#[derive(Debug, Clone, Default)]
pub struct DenySet {
    paths: Vec<PathBuf>,
    /// The memory scope directories the file tools reach as roots. A path under one of them is
    /// never denied **here**; the sandbox policy (the shell) has no exception and still denies
    /// all of `memory/`.
    except: Vec<PathBuf>,
}

impl DenySet {
    /// Nothing is denied. Tests and embedders that never exercise the `full` tier use this;
    /// the tier check refuses outside paths before the set is consulted.
    pub fn empty() -> Self {
        Self {
            paths: Vec::new(),
            except: Vec::new(),
        }
    }

    /// Adds the memory scope directories the file tools were given as roots (ADR 0039
    /// decision 4): the deny set keeps `memory/` for the shell, but the scope directories the
    /// tools actually opened are reachable through their own roots and so are the exception
    /// here. The exception is the roots' canonical paths, never a path resolved again: a scope
    /// that was never created (or whose creation failed), or one replaced by a link after the
    /// roots were opened, is not an exception and cannot widen it to the memory directory.
    pub fn allowing(mut self, memory_roots: &MemoryRoots) -> Self {
        self.except
            .extend(memory_roots.canonical_paths().map(Path::to_path_buf));
        self.except.sort();
        self.except.dedup();
        self
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
        Self {
            paths,
            except: Vec::new(),
        }
    }

    /// Whether `canonical` (already `realpath`ed by the caller) is denied: itself or under
    /// one of the denied paths, unless it is in a memory scope (the documented exception).
    /// Returns the matching denied path for the refusal message.
    pub fn denies(&self, canonical: &Path) -> Option<&Path> {
        if radian_sandbox::deny_read_match(&self.except, canonical).is_some() {
            return None;
        }
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

    #[test]
    fn an_unavailable_scope_is_not_an_exception() {
        let dir = std::env::temp_dir().join(format!("radian-deny-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = dir.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("projects.jsonl"), "{\"key\":\"x\"}\n").unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let memory = dir.join("memory");
        // `global/` was never created (or its creation failed), so the tool set has no root
        // for it. It must not except the memory directory in its place: the registry and
        // credentials stay denied.
        let roots = crate::MemoryRoots::from_dirs(&[memory.join("global")]);
        let set =
            DenySet::from_home_and_extra(None, std::slice::from_ref(&memory)).allowing(&roots);
        assert_eq!(
            set.denies(&memory.join("projects.jsonl")),
            Some(memory.as_path())
        );
        assert_eq!(
            set.denies(&memory.join("credentials/x")),
            Some(memory.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_scope_replaced_after_setup_does_not_except_the_memory_directory() {
        let dir = std::env::temp_dir().join(format!("radian-deny-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = dir.join("memory");
        let global = memory.join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(memory.join("projects.jsonl"), "{\"key\":\"x\"}\n").unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let memory = dir.join("memory");
        let global = memory.join("global");
        // The root is opened first; then the scope directory is replaced by a link to the
        // memory directory before the deny set is built. Going through the opened root is
        // harmless (its fd is the old directory), but resolving the path again would except
        // the whole memory directory.
        let roots = crate::MemoryRoots::from_dirs(std::slice::from_ref(&global));
        std::fs::remove_dir(&global).unwrap();
        std::os::unix::fs::symlink(&memory, &global).unwrap();
        let set =
            DenySet::from_home_and_extra(None, std::slice::from_ref(&memory)).allowing(&roots);
        assert_eq!(
            set.denies(&memory.join("projects.jsonl")),
            Some(memory.as_path())
        );
        assert_eq!(
            set.denies(&memory.join("credentials/x")),
            Some(memory.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_memory_scope_is_the_exception_but_its_parent_and_siblings_are_denied() {
        let dir = std::env::temp_dir().join(format!("radian-deny-except-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = dir.join("memory");
        std::fs::create_dir_all(memory.join("global")).unwrap();
        std::fs::create_dir_all(memory.join("projects/P")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let memory = dir.join("memory");
        let global = memory.join("global");
        let project = memory.join("projects/P");
        let roots = crate::MemoryRoots::from_dirs(&[global.clone(), project.clone()]);
        let set =
            DenySet::from_home_and_extra(None, std::slice::from_ref(&memory)).allowing(&roots);
        assert!(set.denies(&global.join("x.md")).is_none());
        assert!(set.denies(&project.join("MEMORY.md")).is_none());
        // Everything else under `memory/` is still the sandbox's deny set.
        assert_eq!(
            set.denies(&memory.join("projects.jsonl")),
            Some(memory.as_path())
        );
        assert_eq!(
            set.denies(&memory.join("credentials/x")),
            Some(memory.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

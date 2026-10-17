//! Project trust: the record, the decision resolution and the trigger (ADR 0031 slice A).
//!
//! Trust decides **whether a workspace's own resources are loaded** (ADR 0031 T1). This module
//! owns only the durable record and the pure resolution; it changes no loading behaviour, so
//! the slices that gate project skills (片 E) build on it without touching this file.
//!
//! - **Record**: `~/.radian/trust/trust.json` (`$RADIAN_HOME/trust/trust.json`), a JSON object
//!   `{ "<canonical directory>": true | false }`, sorted keys, written atomically `0600` in a
//!   `0700` directory (T2). A malformed file (not an object, or a value that is not a bool) is
//!   **rejected whole** and warned about — never partially adopted (fail-closed: ask again
//!   rather than guess a trust).
//! - **Resolution**: the command line wins, then the **nearest ancestor** (the directory itself
//!   included) with a saved decision, then "no decision" (untrusted). Paths are canonicalized
//!   before both storing and looking up, so a symlink and its target share one decision (T5).
//! - **Trigger**: a workspace needs a decision only when it holds a resource from T1's list;
//!   today that list is `<workspace>/.radian/skills/<x>/SKILL.md` and the project declaration
//!   `<workspace>/.radian/config.toml` (ADR 0043 decision 1). A bare `.radian/` does not
//!   trigger (T1).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::home::trust_dir_for;

/// One saved decision's value. Stored as a JSON bool; `Trust`/`Untrust` are the typed form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Trust,
    Untrust,
}

impl Decision {
    pub fn as_bool(self) -> bool {
        matches!(self, Decision::Trust)
    }

    pub fn from_bool(value: bool) -> Self {
        if value {
            Decision::Trust
        } else {
            Decision::Untrust
        }
    }

    /// The `trust.json` spelling (`true`/`false`).
    pub fn as_str(self) -> &'static str {
        if self.as_bool() { "trust" } else { "untrust" }
    }
}

/// Where a resolved decision came from. `/trust` (片 C) shows this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `--trust`/`--no-trust`: this process only, never written.
    Cli,
    /// A saved decision for the working directory itself.
    Saved(PathBuf),
    /// A saved decision for an ancestor of the working directory.
    Ancestor(PathBuf),
    /// No saved decision and no command line: untrusted (fail-safe).
    Default,
    /// The user chose "this session only": no file was written (ADR 0031 T3).
    Session,
}

/// One resolved decision: what applies, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub decision: Decision,
    pub source: Source,
}

/// What a session starts with (ADR 0031 T3). The host computes it before `open_session`; the
/// session stores it so the resources it gates and the startup line agree. `needs_ask` is
/// true when there is no saved decision, no command-line override, and the workspace has
/// project resources: an interactive host asks once, before the first turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub workspace: PathBuf,
    pub resolved: Resolved,
    pub needs_ask: bool,
}

impl Setup {
    /// The setup for a workspace with no saved decision and no override. `needs_ask` is the
    /// caller's (a non-interactive host passes `false` and takes the untrusted default).
    pub fn untrusted(workspace: &Path, needs_ask: bool) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            resolved: Resolved {
                decision: Decision::Untrust,
                source: Source::Default,
            },
            needs_ask,
        }
    }
}

/// The six choices the startup prompt offers (ADR 0031 T3 revision by ADR 0043 decision 2;
/// owner ruling 6). The order is the display order; `UntrustHere` is the default highlight.
/// `TrustHereSandbox` trusts this directory **and** remembers `workspace` for it in
/// `~/.radian/access/access.json`, the same record `/access` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    TrustHere,
    TrustHereSandbox,
    TrustParent,
    TrustSession,
    UntrustHere,
    UntrustSession,
}

impl Choice {
    pub const ALL: [Choice; 6] = [
        Choice::TrustHere,
        Choice::TrustHereSandbox,
        Choice::TrustParent,
        Choice::TrustSession,
        Choice::UntrustHere,
        Choice::UntrustSession,
    ];

    /// Whether this choice writes `trust.json` (the session-only ones do not).
    pub fn persistent(self) -> bool {
        !matches!(self, Choice::TrustSession | Choice::UntrustSession)
    }

    pub fn decision(self) -> Decision {
        match self {
            Choice::TrustHere
            | Choice::TrustHereSandbox
            | Choice::TrustParent
            | Choice::TrustSession => Decision::Trust,
            Choice::UntrustHere | Choice::UntrustSession => Decision::Untrust,
        }
    }

    /// The one-line label, with the parent path when it applies. `None` for
    /// [`Choice::TrustParent`] when the workspace has no parent (the host then omits it).
    pub fn label(self, workspace: &Path) -> Option<String> {
        Some(match self {
            Choice::TrustHere => format!("Trust this directory ({})", workspace.display()),
            Choice::TrustHereSandbox => {
                "Trust this directory, and use the sandbox for this project".to_owned()
            }
            Choice::TrustParent => format!(
                "Trust the parent directory ({})",
                workspace.parent()?.display()
            ),
            Choice::TrustSession => "Trust this session only".to_owned(),
            Choice::UntrustHere => format!("Do not trust this directory ({})", workspace.display()),
            Choice::UntrustSession => "Do not trust, this session only".to_owned(),
        })
    }
}

/// Applies a startup choice (ADR 0031 T3): the persistent choices write `trust.json` (each
/// write audits once), the session-only ones do not. Returns the session's new resolved state.
pub fn apply(choice: Choice, workspace: &Path, audit: Audit<'_>) -> Result<Resolved, String> {
    let resolved = resolved_for(choice, workspace)?;
    if !choice.persistent() {
        return Ok(resolved);
    }
    let path = trust_file().ok_or("no RADIAN_HOME or HOME to place trust.json under")?;
    match choice {
        Choice::TrustHere | Choice::UntrustHere => {
            set_at(&path, workspace, resolved.decision, audit)?;
        }
        // ADR 0043 decision 2: this choice also remembers the sandbox for the project, in the
        // same per-project record `/access` writes. The trust decision is what makes the
        // project's own resources load; the access record is what keeps the sandbox on next
        // time.
        Choice::TrustHereSandbox => {
            set_at(&path, workspace, resolved.decision, audit)?;
            crate::access::remember_for(workspace, radian_permissions::Preset::WorkspaceWrite)?;
        }
        Choice::TrustParent => {
            let parent = workspace
                .parent()
                .ok_or("the workspace has no parent directory")?;
            set_at(&path, parent, resolved.decision, audit)?;
            // The child's own decision is cleared so the parent's trust is inherited, matching
            // pi's "trust parent" option.
            forget_at(&path, workspace, audit)?;
        }
        Choice::TrustSession | Choice::UntrustSession => unreachable!("not persistent"),
    }
    Ok(resolved)
}

/// The session's resolved state for a choice, **without** writing anything. The host uses it
/// for the pre-session setup, then [`apply`] persists and audits after the session exists.
pub fn resolved_for(choice: Choice, workspace: &Path) -> Result<Resolved, String> {
    let decision = choice.decision();
    Ok(match choice {
        Choice::TrustSession | Choice::UntrustSession => Resolved {
            decision,
            source: Source::Session,
        },
        Choice::TrustHere | Choice::TrustHereSandbox | Choice::UntrustHere => Resolved {
            decision,
            source: Source::Saved(canonical(workspace)),
        },
        Choice::TrustParent => {
            let parent = workspace
                .parent()
                .ok_or("the workspace has no parent directory")?;
            Resolved {
                decision,
                source: Source::Ancestor(canonical(parent)),
            }
        }
    })
}

/// The record: canonical directory path → decision.
pub type Store = BTreeMap<String, bool>;

/// One audit record for a trust write (ADR 0031 T2; the journal write is a later slice). The
/// shape is a path and the decision, like pi's store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustChanged {
    pub path: PathBuf,
    pub decision: Decision,
}

/// The audit callback a write takes. `None`-like no-op callers pass `&|_| {}`.
pub type Audit<'a> = &'a (dyn Fn(&ProjectTrustChanged) + Send + Sync);

/// `~/.radian/trust/` for this process, created `0700` (ADR 0020). `None` without a home.
pub fn trust_dir() -> Option<PathBuf> {
    Some(crate::home::private(trust_dir_for(
        &crate::home::radian_home()?,
    )))
}

/// `~/.radian/trust/trust.json` for this process.
pub fn trust_file() -> Option<PathBuf> {
    Some(trust_dir()?.join(FILE_NAME))
}

/// The file name inside `trust/`.
pub const FILE_NAME: &str = "trust.json";

/// Reads the record at `path`. `Err` is the one-line warning for a **whole-file** rejection; a
/// missing file is `Ok(empty)` (T2).
pub fn read_at(path: &Path) -> Result<Store, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::new()),
        Err(error) => return Err(format!("{}: cannot read: {error}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{}: not valid JSON: {error}", path.display()))?;
    let serde_json::Value::Object(map) = value else {
        return Err(format!(
            "{}: the trust store must be a JSON object of path → true|false",
            path.display()
        ));
    };
    let mut store = Store::new();
    for (key, value) in map {
        let Some(decision) = value.as_bool() else {
            // Fail-closed: one bad value rejects the file, it is never partly adopted.
            return Err(format!(
                "{}: the value for {key:?} must be true or false",
                path.display()
            ));
        };
        store.insert(key, decision);
    }
    Ok(store)
}

/// Reads this process's record. `Err` is the warning for a whole-file rejection; the caller
/// treats it as an empty store and warns (fail-closed).
pub fn read() -> Result<Store, String> {
    match trust_file() {
        Some(path) => read_at(&path),
        None => Ok(Store::new()),
    }
}

/// Writes the record at `path` atomically: sorted keys, `0600`, `0700` directory (T2).
pub fn write_at(path: &Path, store: &Store) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        radian_session::ensure_private_dir(dir)?;
    }
    let sorted: serde_json::Map<String, serde_json::Value> = store
        .iter()
        .map(|(key, decision)| (key.clone(), serde_json::Value::Bool(*decision)))
        .collect();
    let mut bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(sorted))
        .unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    crate::atomic_write::write_atomic(path, &bytes, Some(0o600))
}

/// Sets one directory's decision at `path`, writes the record and calls `audit` once (T2). The
/// directory is canonicalized first, so a symlink writes the target's key. A corrupt or
/// unreadable record is an error (ADR 0031 T2): it is never silently replaced with an empty
/// store, which would destroy every other directory's decision.
pub fn set_at(path: &Path, dir: &Path, decision: Decision, audit: Audit<'_>) -> Result<(), String> {
    let canonical = canonical(dir);
    let key = canonical.to_string_lossy().into_owned();
    let mut store = read_at(path)?;
    store.insert(key, decision.as_bool());
    write_at(path, &store).map_err(|error| format!("{}: cannot write: {error}", path.display()))?;
    audit(&ProjectTrustChanged {
        path: canonical,
        decision,
    });
    Ok(())
}

/// Removes one directory's saved decision (the `/trust forget` path, 片 C). Removing a key that
/// is not there is not an error and writes nothing. A corrupt record is an error, like
/// [`set_at`].
pub fn forget_at(path: &Path, dir: &Path, audit: Audit<'_>) -> Result<(), String> {
    let canonical = canonical(dir);
    let key = canonical.to_string_lossy().into_owned();
    let mut store = read_at(path)?;
    if store.remove(&key).is_none() {
        return Ok(());
    }
    write_at(path, &store).map_err(|error| format!("{}: cannot write: {error}", path.display()))?;
    audit(&ProjectTrustChanged {
        path: canonical,
        decision: Decision::Untrust,
    });
    Ok(())
}

/// Resolves the decision for `cwd` (T5): the command line wins, then the **nearest** saved
/// ancestor (the directory itself included), then no decision. Both sides are canonicalized.
pub fn resolve(cwd: &Path, saved: &Store, cli: Option<Decision>) -> Resolved {
    if let Some(decision) = cli {
        return Resolved {
            decision,
            source: Source::Cli,
        };
    }
    let mut dir = canonical(cwd);
    loop {
        let key = dir.to_string_lossy().into_owned();
        if let Some(&value) = saved.get(&key) {
            let source = if dir == canonical(cwd) {
                Source::Saved(dir)
            } else {
                Source::Ancestor(dir)
            };
            return Resolved {
                decision: Decision::from_bool(value),
                source,
            };
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    Resolved {
        decision: Decision::Untrust,
        source: Source::Default,
    }
}

/// Resolves the startup setup (ADR 0031 T3): read this process's store, apply the command-line
/// override, and set `needs_ask` when there is no saved decision, no override, and the
/// workspace has project resources. `interactive` gates the ask: a non-interactive host takes
/// the untrusted default and prints the one-line hint itself. The `Err` is the warning for a
/// store that was rejected whole (the caller warns and continues with the untrusted default).
pub fn load_for(
    workspace: &Path,
    cli: Option<Decision>,
    interactive: bool,
) -> (Setup, Option<String>) {
    let (saved, warning) = match read() {
        Ok(saved) => (saved, None),
        Err(warning) => (Store::new(), Some(warning)),
    };
    let resolved = resolve(workspace, &saved, cli);
    let needs_ask =
        interactive && resolved.source == Source::Default && has_project_resources(workspace);
    (
        Setup {
            workspace: workspace.to_path_buf(),
            resolved,
            needs_ask,
        },
        warning,
    )
}

/// The startup line (ADR 0031 T3/T6): what the trust decision is, and where it came from.
/// Same style as ADR 0024 Q5's `ask_user: off`.
pub fn status_line(setup: &Setup) -> String {
    let source = match &setup.resolved.source {
        Source::Cli => "command line".to_owned(),
        Source::Saved(path) => format!("saved for {}", path.display()),
        Source::Ancestor(path) => format!("inherited from {}", path.display()),
        Source::Session => "this session only".to_owned(),
        Source::Default => "default".to_owned(),
    };
    if setup.resolved.decision.as_bool() {
        format!("project resources: loaded (trusted: {source})")
    } else {
        format!("project resources: not loaded (untrusted: {source})")
    }
}

/// The one line a non-interactive host prints when project resources are present but
/// untrusted (ADR 0031 T3).
pub fn untrusted_notice(workspace: &Path) -> String {
    format!(
        "project resources not loaded (untrusted): pass --trust to load {}, or run interactively",
        workspace.display()
    )
}

/// Whether `workspace` holds a resource from T1's list. The list (ADR 0031 T1, shrunk by ADR
/// 0043 decision 1 to the project's own skills and its project-level configuration):
///
/// - a project-level skill: `<workspace>/.radian/skills/<x>/SKILL.md` (or directly under `skills/`);
/// - the project declaration: `<workspace>/.radian/config.toml`.
///
/// A bare `.radian/` does not trigger, and `.radian-sandbox/` is ignored. The check is a **file**
/// question: a malformed file is still something the loader would read, so it still triggers.
/// The walk reuses the skills module's ignore rules.
pub fn has_project_resources(workspace: &Path) -> bool {
    let dot_radian = workspace.join(".radian");
    crate::skills::has_skill_file(&dot_radian.join("skills"))
        || crate::project::path(workspace).is_file()
}

/// The canonical form of `path`: symlinks resolved, `.`/`..` removed. Falls back to the path
/// as given when it cannot be canonicalized (a path that does not exist yet), matching
/// `memory::identity`'s rule. Public so the trigger and a caller's own display agree.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A temp root; the store lives at `<root>/trust/trust.json`.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "radian-trust-test-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn path(&self) -> PathBuf {
            self.root.join("trust").join(FILE_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// One saved decision per directory, under a real (canonicalized) tree.
    fn saved(entries: &[(&Path, bool)]) -> Store {
        entries
            .iter()
            .map(|(dir, decision)| (canonical(dir).to_string_lossy().into_owned(), *decision))
            .collect()
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let scratch = Scratch::new("missing");
        assert_eq!(read_at(&scratch.path()).unwrap(), Store::new());
    }

    #[test]
    fn a_bad_file_is_rejected_whole_not_partly_adopted() {
        let scratch = Scratch::new("bad");
        let path = scratch.path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for text in [
            "not json",
            "[1, 2]",
            "{\"a\": 1}",
            "{\"a\": true, \"b\": \"yes\"}",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(read_at(&path).is_err(), "{text} must be rejected");
        }
    }

    #[test]
    fn the_record_round_trips_sorted_and_private() {
        let scratch = Scratch::new("round-trip");
        let mut store = Store::new();
        store.insert("/z".to_owned(), true);
        store.insert("/a".to_owned(), false);
        write_at(&scratch.path(), &store).unwrap();
        assert_eq!(read_at(&scratch.path()).unwrap(), store);
        let text = std::fs::read_to_string(scratch.path()).unwrap();
        let a = text.find("\"/a\"").unwrap();
        let z = text.find("\"/z\"").unwrap();
        assert!(a < z, "keys are sorted: {text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::metadata(scratch.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let dir = std::fs::metadata(scratch.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file, 0o600, "trust.json is 0600");
            assert_eq!(dir, 0o700, "trust/ is 0700");
        }
    }

    #[test]
    fn the_command_line_wins_over_every_saved_decision() {
        let root = Scratch::new("cli");
        let saved = saved(&[(&root.root, true)]);
        let resolved = resolve(&root.root, &saved, Some(Decision::Untrust));
        assert_eq!(resolved.decision, Decision::Untrust);
        assert_eq!(resolved.source, Source::Cli);
    }

    #[test]
    fn the_nearest_ancestor_wins_and_a_child_false_overrides_a_parent_true() {
        let root = Scratch::new("nearest");
        let child = root.root.join("repo/sub");
        std::fs::create_dir_all(&child).unwrap();
        // The parent is trusted, the child is not: the child's own `false` wins.
        let store = saved(&[(&root.root, true), (&child, false)]);
        let resolved = resolve(&child, &store, None);
        assert_eq!(resolved.decision, Decision::Untrust);
        assert_eq!(resolved.source, Source::Saved(canonical(&child)));
        // A directory below the trusted repo inherits the repo's decision.
        let deep = root.root.join("repo/other");
        std::fs::create_dir_all(&deep).unwrap();
        let resolved = resolve(&deep, &saved(&[(&root.root, true)]), None);
        assert_eq!(resolved.decision, Decision::Trust);
        assert_eq!(resolved.source, Source::Ancestor(canonical(&root.root)));
    }

    #[test]
    fn no_saved_decision_resolves_untrusted() {
        let root = Scratch::new("none");
        let resolved = resolve(&root.root, &Store::new(), None);
        assert_eq!(resolved.decision, Decision::Untrust);
        assert_eq!(resolved.source, Source::Default);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_and_its_target_share_one_decision() {
        let root = Scratch::new("symlink");
        let real = root.root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Saved under the real path; resolved through the symlink.
        let resolved = resolve(&link, &saved(&[(&real, true)]), None);
        assert_eq!(resolved.decision, Decision::Trust);
        // And a write through the symlink stores the real (canonical) key.
        let scratch = Scratch::new("symlink-write");
        let audit = Mutex::new(Vec::new());
        let callback = |change: &ProjectTrustChanged| audit.lock().unwrap().push(change.clone());
        set_at(&scratch.path(), &link, Decision::Trust, &callback).unwrap();
        let store = read_at(&scratch.path()).unwrap();
        assert!(store.contains_key(&canonical(&real).to_string_lossy().into_owned()));
        assert_eq!(audit.lock().unwrap()[0].path, canonical(&real));
    }

    #[test]
    fn a_write_audits_once_and_forget_clears_it() {
        let scratch = Scratch::new("audit");
        let dir = scratch.root.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let audit = Mutex::new(Vec::new());
        let callback = |change: &ProjectTrustChanged| audit.lock().unwrap().push(change.clone());
        set_at(&scratch.path(), &dir, Decision::Trust, &callback).unwrap();
        set_at(&scratch.path(), &dir, Decision::Untrust, &callback).unwrap();
        assert_eq!(audit.lock().unwrap().len(), 2);
        assert_eq!(audit.lock().unwrap()[1].decision, Decision::Untrust);
        forget_at(&scratch.path(), &dir, &callback).unwrap();
        assert!(
            !read_at(&scratch.path())
                .unwrap()
                .contains_key(&canonical(&dir).to_string_lossy().into_owned())
        );
        // Forgetting again is a no-op (and does not audit).
        let before = audit.lock().unwrap().len();
        forget_at(&scratch.path(), &dir, &callback).unwrap();
        assert_eq!(audit.lock().unwrap().len(), before);
    }

    /// ADR 0031 T2 (review fix): a corrupt `trust.json` is an error, never silently replaced
    /// with an empty store. The write must not happen, so every other directory's decision
    /// survives.
    #[test]
    fn a_corrupt_record_is_not_overwritten() {
        let scratch = Scratch::new("corrupt");
        let dir = scratch.root.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(scratch.path().parent().unwrap()).unwrap();
        let bytes = b"{not json".to_vec();
        std::fs::write(scratch.path(), &bytes).unwrap();
        let audit = Mutex::new(Vec::new());
        let callback = |change: &ProjectTrustChanged| audit.lock().unwrap().push(change.clone());

        let error = set_at(&scratch.path(), &dir, Decision::Trust, &callback).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        assert_eq!(
            std::fs::read(scratch.path()).unwrap(),
            bytes,
            "the corrupt file was overwritten"
        );
        assert!(
            audit.lock().unwrap().is_empty(),
            "no audit on a failed write"
        );

        let error = forget_at(&scratch.path(), &dir, &callback).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        assert_eq!(std::fs::read(scratch.path()).unwrap(), bytes);
    }

    #[test]
    fn the_trigger_is_a_project_skill_and_not_a_bare_dot_radian() {
        let root = Scratch::new("trigger");
        let ws = root.root.join("ws");
        // A bare `.radian/` (even with other files) does not trigger.
        std::fs::create_dir_all(ws.join(".radian")).unwrap();
        assert!(!has_project_resources(&ws));
        // A `.radian-sandbox/` is not a resource.
        std::fs::create_dir_all(ws.join(".radian-sandbox")).unwrap();
        assert!(!has_project_resources(&ws));
        // A `SKILL.md` under `.radian/skills/<x>/` triggers.
        let skill = ws.join(".radian/skills/greet");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: greet\n---\n").unwrap();
        assert!(has_project_resources(&ws));

        // ADR 0043 decision 2: the project declaration itself is a project resource.
        let other = root.root.join("ws-config");
        std::fs::create_dir_all(other.join(".radian")).unwrap();
        assert!(!has_project_resources(&other));
        std::fs::write(other.join(".radian/config.toml"), "[permissions]\n").unwrap();
        assert!(has_project_resources(&other));
    }
}

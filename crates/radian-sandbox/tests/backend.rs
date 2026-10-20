//! The "unavailable backend fails, never degrades" contract and the policy builder. These
//! tests are platform independent: they inject backends.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use radian_sandbox::{
    NoSandbox, ProbeReport, Sandbox, SandboxBackend, SandboxError, SandboxMode, SandboxPolicy,
};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-sandbox-test-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Canonicalize so the workspace is the real path the backend would see.
        Self(std::fs::canonicalize(&dir).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Unavailable {
    probes: AtomicUsize,
}

impl SandboxBackend for Unavailable {
    fn name(&self) -> &'static str {
        "unavailable-test"
    }
    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Err(SandboxError::BackendUnavailable {
            backend: self.name(),
            detail: "the test backend does not exist".into(),
        })
    }
    fn command(
        &self,
        _policy: &SandboxPolicy,
        _program: &OsStr,
        _args: &[std::ffi::OsString],
    ) -> Result<Command, SandboxError> {
        unreachable!("an unavailable backend is never asked for a command")
    }
}

struct ProbeFails {
    probes: AtomicUsize,
}

impl SandboxBackend for ProbeFails {
    fn name(&self) -> &'static str {
        "probe-fails-test"
    }
    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Err(SandboxError::ProbeFailed {
            backend: self.name(),
            detail: "the deny rule did not take effect".into(),
        })
    }
    fn command(
        &self,
        _policy: &SandboxPolicy,
        _program: &OsStr,
        _args: &[std::ffi::OsString],
    ) -> Result<Command, SandboxError> {
        unreachable!("a failing probe is never followed by a command")
    }
}

#[test]
fn an_unavailable_backend_is_an_error_and_never_a_downgrade() {
    let dir = TempDir::new("unavailable");
    let backend: &'static Unavailable = Box::leak(Box::new(Unavailable {
        probes: AtomicUsize::new(0),
    }));
    let result = Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, dir.path());
    match result {
        Err(SandboxError::BackendUnavailable { backend, .. }) => {
            assert_eq!(backend, "unavailable-test")
        }
        other => panic!("expected BackendUnavailable, got {:?}", other.err()),
    }
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        1,
        "the backend must have been probed before use"
    );
}

#[test]
fn a_failed_probe_is_an_error_not_a_downgrade() {
    let dir = TempDir::new("probe-fails");
    let backend: &'static ProbeFails = Box::leak(Box::new(ProbeFails {
        probes: AtomicUsize::new(0),
    }));
    let err = Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, dir.path()).unwrap_err();
    assert!(matches!(err, SandboxError::ProbeFailed { .. }), "{err}");
    assert_eq!(backend.probes.load(Ordering::SeqCst), 1);
}

#[test]
fn full_access_is_transparent_and_unprobed() {
    let dir = TempDir::new("full-access");
    let backend: &'static ProbeFails = Box::leak(Box::new(ProbeFails {
        probes: AtomicUsize::new(0),
    }));
    // Enforced modes fail...
    assert!(Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, dir.path()).is_err());
    // ... FullAccess does not even need the backend's probe.
    let sandbox = Sandbox::from_backend(backend, SandboxMode::FullAccess, dir.path()).unwrap();
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        1,
        "only the WorkspaceWrite call probed"
    );
    let cmd = sandbox.command("echo", ["hi"]).unwrap();
    assert_eq!(cmd.get_program(), OsStr::new("echo"));
    assert_eq!(sandbox.backend_name(), "none");
}

#[test]
fn no_sandbox_backend_wraps_nothing() {
    let dir = TempDir::new("none");
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, dir.path()).unwrap();
    let cmd = NoSandbox
        .command(&policy, OsStr::new("/bin/true"), &[])
        .unwrap();
    assert_eq!(cmd.get_program(), OsStr::new("/bin/true"));
    assert_eq!(
        cmd.get_current_dir(),
        Some(policy.workspace.as_path()),
        "the workspace is the cwd even without a sandbox"
    );
}

#[test]
fn workspace_write_discovers_git_metadata_as_read_only() {
    let dir = TempDir::new("git");
    let repo = dir.path().join("proj");
    std::fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "-q"]);
    let policy = SandboxPolicy::workspace_write(dir.path()).unwrap();
    let git_dir = std::fs::canonicalize(repo.join(".git")).unwrap();
    assert!(
        policy.read_only.contains(&git_dir),
        "the .git directory must be read-only; got {:?}",
        policy.read_only
    );
}

#[test]
fn a_worktree_pointer_file_adds_the_shared_common_dir() {
    let dir = TempDir::new("worktree");
    let main = dir.path().join("main");
    let wt = dir.path().join("wt");
    std::fs::create_dir(&main).unwrap();
    run_git(&main, &["init", "-q"]);
    run_git(&main, &["config", "user.email", "t@example.com"]);
    run_git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("a.txt"), "a\n").unwrap();
    run_git(&main, &["add", "a.txt"]);
    run_git(&main, &["commit", "-qm", "init"]);
    run_git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );

    let policy = SandboxPolicy::workspace_write(&wt).unwrap();
    let pointer = std::fs::canonicalize(wt.join(".git")).unwrap();
    let common = std::fs::canonicalize(main.join(".git")).unwrap();
    assert!(
        policy.read_only.contains(&pointer),
        "the worktree pointer file must be read-only: {:?}",
        policy.read_only
    );
    assert!(
        policy.read_only.contains(&common),
        "the shared common dir must be read-only: {:?}",
        policy.read_only
    );
}

#[test]
fn denied_read_paths_are_kept_and_never_widened() {
    let dir = TempDir::new("deny");
    let secret = dir.path().join(".ssh");
    std::fs::create_dir(&secret).unwrap();
    let policy = SandboxPolicy::workspace_write(dir.path())
        .unwrap()
        .deny_read(&secret);
    let secret = std::fs::canonicalize(&secret).unwrap();
    assert!(policy.deny_read.contains(&secret));
    // Not a directory: still recorded (an existing file).
    let netrc = dir.path().join(".netrc");
    std::fs::write(&netrc, "machine x\n").unwrap();
    let policy = policy.deny_read(&netrc);
    assert!(
        policy.deny_read.iter().any(|p| p.ends_with(".netrc")),
        "{:?}",
        policy.deny_read
    );
}

/// ADR 0020: a workspace inside a directory the sandbox denies reading is refused before any
/// backend probes (Linux would `--tmpfs` the parent over the bound workspace; macOS's
/// `deny file-read* (subpath P)` covers the workspace too).
#[test]
fn a_workspace_inside_a_denied_directory_is_refused() {
    let dir = TempDir::new("nested");
    let parent = dir.path().join("parent");
    let ws = parent.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ws = std::fs::canonicalize(&ws).unwrap();
    let parent = std::fs::canonicalize(&parent).unwrap();
    let policy = SandboxPolicy::workspace_write(&ws)
        .unwrap()
        .deny_read(&parent);
    let error = Sandbox::from_policy(policy).unwrap_err();
    match error {
        SandboxError::InvalidWorkspace { path, detail } => {
            assert_eq!(path, ws);
            assert!(
                detail.contains("workspace is inside") && detail.contains("session directory"),
                "{detail}"
            );
        }
        other => panic!("expected InvalidWorkspace, got {other}"),
    }
}

/// The reverse direction is allowed: a deny path inside the workspace (e.g.
/// `--session-dir ./sessions`) only masks that subtree; the sandbox is still built.
#[test]
fn a_denied_directory_inside_the_workspace_is_allowed() {
    let dir = TempDir::new("inside");
    let ws = dir.path().join("ws");
    let sessions = ws.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = std::fs::canonicalize(&ws).unwrap();
    let policy = SandboxPolicy::workspace_write(&ws)
        .unwrap()
        .deny_read(&sessions);
    match Sandbox::from_policy(policy) {
        Ok(_) => {}
        // A host with no working backend is fine: what must not happen is the
        // workspace-inside-deny rejection.
        Err(SandboxError::InvalidWorkspace { .. }) => {
            panic!("the reverse direction (deny inside the workspace) must be allowed")
        }
        Err(_) => {}
    }
}

/// ADR 0016: the `inherited` backend adds no local rules — the outer
/// sandbox owns what is readable, writable and reachable — but the **host-side** rules that are
/// not sandbox rules still apply: the environment is cleared to the policy's whitelist, and the
/// child runs in the workspace with `TMPDIR` inside it (the one tree the outer sandbox is known
/// to allow writing). Mutations: drop `apply_env` (the child inherits the whole host
/// environment) or drop the `TMPDIR` line.
#[test]
fn the_inherited_backend_keeps_the_environment_whitelist_and_no_local_rules() {
    let dir = TempDir::new("inherited");
    let sandbox = Sandbox::inherited(dir.path());
    assert_eq!(sandbox.backend_name(), "inherited");
    assert_eq!(sandbox.mode(), SandboxMode::WorkspaceWrite);
    assert_eq!(sandbox.probe().backend, "inherited");
    assert!(
        sandbox.probe().detail.contains("no probe"),
        "{:?}",
        sandbox.probe()
    );
    // Nothing local: no deny-read list, no read-only git metadata, no whitelisted socket.
    assert!(sandbox.policy().deny_read.is_empty());
    assert!(sandbox.policy().read_only.is_empty());
    assert!(sandbox.policy().allow_unix_sockets.is_empty());

    // Run a real child: what it sees is what the shell tool's commands see.
    let output = sandbox
        .command("/usr/bin/env", std::iter::empty::<&str>())
        .unwrap()
        .output()
        .expect("the inherited backend runs a plain child");
    assert!(output.status.success());
    let env_output = String::from_utf8_lossy(&output.stdout);
    let child: std::collections::BTreeSet<String> = env_output
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_owned()))
        .collect();
    let mut expected: std::collections::BTreeSet<String> = radian_sandbox::DEFAULT_ALLOW_ENV
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .map(|name| (*name).to_owned())
        .collect();
    // The backend sets `TMPDIR` itself, whether or not the host has one.
    expected.insert("TMPDIR".to_owned());
    assert!(expected.len() > 1, "the test host must have PATH-like vars");
    assert_eq!(
        child, expected,
        "the child's environment is not the whitelist"
    );
    let tmpdir = env_output
        .lines()
        .find_map(|line| line.strip_prefix("TMPDIR="))
        .expect("the child has a TMPDIR");
    assert!(
        tmpdir.ends_with(".radian-sandbox/tmp"),
        "TMPDIR must stay inside the workspace: {tmpdir}"
    );
    let command = sandbox
        .command("/bin/true", std::iter::empty::<&str>())
        .unwrap();
    assert_eq!(
        command.get_current_dir(),
        Some(dir.path()),
        "the workspace is the child's cwd"
    );
}

#[test]
fn a_missing_workspace_is_an_invalid_workspace_error() {
    let dir = TempDir::new("missing");
    let err = SandboxPolicy::workspace_write(dir.path().join("nope")).unwrap_err();
    assert!(
        matches!(err, SandboxError::InvalidWorkspace { .. }),
        "{err}"
    );
}

/// A dependency tree (`node_modules`) and a cache-tagged directory (`CACHEDIR.TAG`, what cargo
/// writes at the root of `target/`) are not searched for git metadata: a repository **deeper
/// inside** one of them is not collected, while a normal nested repository still is. Walking
/// `target/` is what made startup take ~2 s on a 10+ GB tree.
#[test]
fn a_repository_inside_a_skipped_directory_is_not_collected() {
    let dir = TempDir::new("skip");
    let normal = dir.path().join("proj").join("nested");
    // The repositories sit one level **below** the skipped directory root, so it is the
    // skipping of the subtree (not the root's own `.git` rule) that is under test here.
    let node_modules = dir.path().join("proj").join("node_modules").join("pkg");
    let target = dir.path().join("target").join("debug");
    let node_modules_root = node_modules.parent().unwrap().to_path_buf();
    let target_root = target.parent().unwrap().to_path_buf();
    for path in [&normal, &node_modules, &target] {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-q"]);
    }
    std::fs::write(
        target_root.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    assert!(node_modules_root.is_dir() && target_root.is_dir());

    let policy = SandboxPolicy::workspace_write(dir.path()).unwrap();

    let normal_git = std::fs::canonicalize(normal.join(".git")).unwrap();
    assert!(
        policy.read_only.contains(&normal_git),
        "a repository in a normal subdirectory must still be read-only: {:?}",
        policy.read_only
    );
    for (label, path) in [("node_modules", &node_modules), ("CACHEDIR.TAG", &target)] {
        let git = std::fs::canonicalize(path.join(".git")).unwrap();
        assert!(
            !policy.read_only.contains(&git),
            "a repository under {label} must not be discovered: {:?}",
            policy.read_only
        );
    }
}

/// A repository **root** that is itself the skipped directory must still have its `.git`
/// collected. Without this, an untrusted repository could drop a `CACHEDIR.TAG` (or name
/// itself `node_modules`) and have `collect_git` skip it entirely, leaving `.git` writable —
/// the model could then write its hooks, which run the next time the user runs git there.
/// The subtree is still not descended into (see the scale test in `policy.rs`).
#[test]
fn a_skipped_directory_that_is_itself_a_repository_still_has_its_dot_git_collected() {
    let dir = TempDir::new("skip-root-repo");
    let cached = dir.path().join("target");
    let modules = dir.path().join("node_modules");
    for path in [&cached, &modules] {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-q"]);
    }
    std::fs::write(
        cached.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    // A repository deeper inside the cache-tagged root must stay undiscovered (the skip still
    // applies to the subtree).
    let hidden = cached.join("nested").join("pkg");
    std::fs::create_dir_all(&hidden).unwrap();
    run_git(&hidden, &["init", "-q"]);

    let policy = SandboxPolicy::workspace_write(dir.path()).unwrap();

    for (label, path) in [("CACHEDIR.TAG", &cached), ("node_modules", &modules)] {
        let git = std::fs::canonicalize(path.join(".git")).unwrap();
        assert!(
            policy.read_only.contains(&git),
            "a repository root skipped by name/marker must still be read-only ({label}): {:?}",
            policy.read_only
        );
    }
    let hidden_git = std::fs::canonicalize(hidden.join(".git")).unwrap();
    assert!(
        !policy.read_only.contains(&hidden_git),
        "the subtree of a skipped directory is still not searched: {:?}",
        policy.read_only
    );
}

/// The walk does not follow a symlinked directory — the same rule `DirEntry::metadata()`
/// (an `lstat`) had before it became `file_type()`. A `.git` reachable only through a symlink
/// is therefore not discovered. This pins the semantics the change had to preserve.
#[test]
fn a_symlinked_directory_is_not_traversed() {
    let host = TempDir::new("symlink-target");
    let real = host.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    run_git(&real, &["init", "-q"]);

    let dir = TempDir::new("symlink");
    let linked = dir.path().join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();

    let policy = SandboxPolicy::workspace_write(dir.path()).unwrap();
    let git = std::fs::canonicalize(real.join(".git")).unwrap();
    assert!(
        !policy.read_only.contains(&git),
        "a symlinked directory is not traversed, so its .git is not discovered: {:?}",
        policy.read_only
    );
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be installed for these tests");
    assert!(status.success(), "git {args:?} failed");
}

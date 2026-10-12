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
    assert!(Sandbox::from_backend(backend, SandboxMode::Safe, dir.path()).is_err());
    // ... FullAccess does not even need the backend's probe.
    let sandbox = Sandbox::from_backend(backend, SandboxMode::FullAccess, dir.path()).unwrap();
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        1,
        "only the Safe call probed"
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

#[test]
fn a_missing_workspace_is_an_invalid_workspace_error() {
    let dir = TempDir::new("missing");
    let err = SandboxPolicy::workspace_write(dir.path().join("nope")).unwrap_err();
    assert!(
        matches!(err, SandboxError::InvalidWorkspace { .. }),
        "{err}"
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
